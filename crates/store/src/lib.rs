//! SQLite item database, schema-compatible with the D client (`src/itemdb.d`,
//! `PRAGMA user_version = 18`). It is the single source of truth for what is
//! known to be in sync.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};

pub const SCHEMA_VERSION: i32 = 18;

const SCHEMA: &str = "
CREATE TABLE item (
  driveId TEXT NOT NULL, id TEXT NOT NULL, name TEXT NOT NULL, remoteName TEXT,
  type TEXT NOT NULL, eTag TEXT, cTag TEXT, mtime TEXT NOT NULL, parentId TEXT,
  quickXorHash TEXT, sha256Hash TEXT, remoteDriveId TEXT, remoteParentId TEXT, remoteId TEXT,
  remoteType TEXT, deltaLink TEXT, syncStatus TEXT, size TEXT, relocDriveId TEXT, relocParentId TEXT,
  PRIMARY KEY (driveId, id),
  FOREIGN KEY (driveId, parentId) REFERENCES item (driveId, id) ON DELETE CASCADE ON UPDATE RESTRICT);
CREATE INDEX name_idx ON item (name);
CREATE INDEX remote_idx ON item (remoteDriveId, remoteId);
CREATE INDEX item_children_idx ON item (driveId, parentId);
CREATE INDEX selectByPath_idx ON item (name, driveId, parentId);
";

const COLUMNS: &str = "driveId, id, name, remoteName, type, eTag, cTag, mtime, parentId, quickXorHash, sha256Hash, \
                       remoteDriveId, remoteParentId, remoteId, remoteType, syncStatus, size, relocDriveId, relocParentId";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("unsupported database schema version {0} (expected {SCHEMA_VERSION})")]
    SchemaVersion(i32),
    #[error("invalid item type {0:?} in database")]
    InvalidType(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemType {
    File,
    Dir,
    Remote,
    Root,
    Unknown,
}

impl ItemType {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemType::File => "file",
            ItemType::Dir => "dir",
            ItemType::Remote => "remote",
            ItemType::Root => "root",
            ItemType::Unknown => "unknown",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "file" => ItemType::File,
            "dir" => ItemType::Dir,
            "remote" => ItemType::Remote,
            "root" => ItemType::Root,
            "unknown" => ItemType::Unknown,
            other => return Err(Error::InvalidType(other.to_owned())),
        })
    }
}

/// One row of the `item` table (the `deltaLink` column is accessed separately).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub drive_id: String,
    pub id: String,
    pub name: String,
    pub remote_name: Option<String>,
    pub item_type: ItemType,
    pub e_tag: Option<String>,
    pub c_tag: Option<String>,
    /// ISO-8601 UTC, e.g. `2019-03-05T03:15:20Z`.
    pub mtime: String,
    pub parent_id: Option<String>,
    pub quick_xor_hash: Option<String>,
    pub sha256_hash: Option<String>,
    pub remote_drive_id: Option<String>,
    pub remote_parent_id: Option<String>,
    pub remote_id: Option<String>,
    pub remote_type: Option<ItemType>,
    pub sync_status: Option<String>,
    pub size: Option<i64>,
    pub reloc_drive_id: Option<String>,
    pub reloc_parent_id: Option<String>,
}

impl Item {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        let item_type: String = row.get(4)?;
        let remote_type: Option<String> = row.get(14)?;
        let size: Option<String> = row.get(16)?;
        Ok(Item {
            drive_id: row.get(0)?,
            id: row.get(1)?,
            name: row.get(2)?,
            remote_name: row.get(3)?,
            item_type: ItemType::parse(&item_type)?,
            e_tag: row.get(5)?,
            c_tag: row.get(6)?,
            mtime: row.get(7)?,
            parent_id: row.get(8)?,
            quick_xor_hash: row.get(9)?,
            sha256_hash: row.get(10)?,
            remote_drive_id: row.get(11)?,
            remote_parent_id: row.get(12)?,
            remote_id: row.get(13)?,
            remote_type: remote_type.as_deref().map(ItemType::parse).transpose()?,
            sync_status: row.get(15)?,
            size: size.and_then(|s| s.parse().ok()),
            reloc_drive_id: row.get(17)?,
            reloc_parent_id: row.get(18)?,
        })
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// Consistent copy of another item database (e.g. the D client's) into
    /// `dest`, with SQLite's online backup. Fails if `src` is locked by a running
    /// D client (it uses `locking_mode = EXCLUSIVE`) or has another schema version.
    pub fn import(src: &Path, dest: &Path) -> Result<Self> {
        let source = Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let version: i32 = source.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(Error::SchemaVersion(version));
        }
        let mut target = Connection::open(dest)?;
        rusqlite::backup::Backup::new(&source, &mut target)?.run_to_completion(1024, std::time::Duration::ZERO, None)?;
        drop(target);
        Self::open(dest)
    }

    /// Copy the rows of drive `drive_id` from another item database (a D client
    /// instance dedicated to that drive) into this one, replacing any rows this
    /// database already has for it. Returns the number of rows copied. Fails if
    /// `src` is locked by a running D client or has another schema version.
    pub fn merge_drive(&self, src: &Path, drive_id: &str) -> Result<u64> {
        let source = Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let version: i32 = source.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(Error::SchemaVersion(version));
        }
        drop(source);
        let uri = format!("file:{}?mode=ro", src.display());
        self.conn.execute("ATTACH DATABASE ?1 AS src", params![uri])?;
        let result = (|| {
            let tx = self.conn.unchecked_transaction()?;
            // Rows arrive in any order: check parent references at commit time.
            tx.execute_batch("PRAGMA defer_foreign_keys = ON")?;
            tx.execute("DELETE FROM item WHERE driveId = ?1", params![drive_id])?;
            let n = tx.execute(
                &format!("INSERT INTO item ({COLUMNS}, deltaLink) SELECT {COLUMNS}, deltaLink FROM src.item WHERE driveId = ?1"),
                params![drive_id],
            )?;
            tx.commit()?;
            Ok::<_, rusqlite::Error>(n as u64)
        })();
        self.conn.execute_batch("DETACH DATABASE src")?;
        Ok(result?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA foreign_keys = TRUE;
             PRAGMA recursive_triggers = TRUE;
             PRAGMA journal_mode = WAL;
             PRAGMA temp_store = MEMORY;
             PRAGMA synchronous = NORMAL;",
        )?;
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => {
                conn.execute_batch(SCHEMA)?;
                conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
            }
            SCHEMA_VERSION => {}
            v => return Err(Error::SchemaVersion(v)),
        }
        Ok(Self { conn })
    }

    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Roll back a transaction left open by an interrupted pass (e.g. a
    /// cancelled future), so the next `begin` does not fail.
    pub fn abort_open_transaction(&self) -> Result<()> {
        if !self.conn.is_autocommit() {
            self.conn.execute_batch("ROLLBACK")?;
        }
        Ok(())
    }

    pub fn rollback(&self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    /// Insert or update an item. Never uses `INSERT OR REPLACE`, which would
    /// delete the row first and cascade to its children.
    pub fn upsert(&self, item: &Item) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "INSERT INTO item ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
             ON CONFLICT (driveId, id) DO UPDATE SET name = ?3, remoteName = ?4, type = ?5, eTag = ?6, cTag = ?7, mtime = ?8,
               parentId = ?9, quickXorHash = ?10, sha256Hash = ?11, remoteDriveId = ?12, remoteParentId = ?13, remoteId = ?14,
               remoteType = ?15, syncStatus = ?16, size = ?17, relocDriveId = ?18, relocParentId = ?19"
        ))?;
        stmt.execute(params![
            item.drive_id,
            item.id,
            item.name,
            item.remote_name,
            item.item_type.as_str(),
            item.e_tag,
            item.c_tag,
            item.mtime,
            item.parent_id,
            item.quick_xor_hash,
            item.sha256_hash,
            item.remote_drive_id,
            item.remote_parent_id,
            item.remote_id,
            item.remote_type.map(ItemType::as_str),
            item.sync_status,
            item.size.map(|s| s.to_string()),
            item.reloc_drive_id,
            item.reloc_parent_id,
        ])?;
        Ok(())
    }

    pub fn get(&self, drive_id: &str, id: &str) -> Result<Option<Item>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT {COLUMNS} FROM item WHERE driveId = ?1 AND id = ?2"))?;
        let mut rows = stmt.query(params![drive_id, id])?;
        rows.next()?.map(Item::from_row).transpose()
    }

    pub fn contains(&self, drive_id: &str, id: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare_cached("SELECT 1 FROM item WHERE driveId = ?1 AND id = ?2")?;
        Ok(stmt.exists(params![drive_id, id])?)
    }

    pub fn root(&self, drive_id: &str) -> Result<Option<Item>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT {COLUMNS} FROM item WHERE driveId = ?1 AND type = 'root'"))?;
        let mut rows = stmt.query(params![drive_id])?;
        rows.next()?.map(Item::from_row).transpose()
    }

    pub fn children(&self, drive_id: &str, id: &str) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT {COLUMNS} FROM item WHERE driveId = ?1 AND parentId = ?2"))?;
        let mut rows = stmt.query(params![drive_id, id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(Item::from_row(row)?);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Files of a drive whose name matches the SQL `LIKE` `pattern` (a scan: for rare lookups).
    pub fn files_named_like(&self, drive_id: &str, pattern: &str) -> Result<Vec<Item>> {
        let mut stmt =
            self.conn.prepare_cached(&format!("SELECT {COLUMNS} FROM item WHERE driveId = ?1 AND type = 'file' AND name LIKE ?2"))?;
        let mut rows = stmt.query(params![drive_id, pattern])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(Item::from_row(row)?);
        }
        Ok(out)
    }

    pub fn child_by_name(&self, drive_id: &str, parent_id: &str, name: &str) -> Result<Option<Item>> {
        let mut stmt =
            self.conn.prepare_cached(&format!("SELECT {COLUMNS} FROM item WHERE name = ?3 AND driveId = ?1 AND parentId = ?2"))?;
        let mut rows = stmt.query(params![drive_id, parent_id, name])?;
        rows.next()?.map(Item::from_row).transpose()
    }

    /// Delete an item; its descendants go with it (`ON DELETE CASCADE`).
    pub fn delete(&self, drive_id: &str, id: &str) -> Result<()> {
        self.conn
            .prepare_cached("DELETE FROM item WHERE driveId = ?1 AND id = ?2")?
            .execute(params![drive_id, id])?;
        Ok(())
    }

    /// Forget everything known about a drive, sync cursor included (resync:
    /// the next pass enumerates it again). Returns the number of rows removed.
    pub fn delete_drive(&self, drive_id: &str) -> Result<u64> {
        // Counted first: SQLite does not count the rows removed by ON DELETE CASCADE.
        let n: i64 = self.conn.query_row("SELECT count(*) FROM item WHERE driveId = ?1", params![drive_id], |r| r.get(0))?;
        self.conn.execute("DELETE FROM item WHERE driveId = ?1", params![drive_id])?;
        Ok(n as u64)
    }

    /// Number of items below `id` (not counting `id` itself).
    pub fn count_descendants(&self, drive_id: &str, id: &str) -> Result<u64> {
        let mut stmt = self.conn.prepare_cached(
            "WITH RECURSIVE sub(id) AS (SELECT id FROM item WHERE driveId = ?1 AND parentId = ?2
               UNION ALL SELECT item.id FROM item JOIN sub ON item.driveId = ?1 AND item.parentId = sub.id)
             SELECT count(*) FROM sub",
        )?;
        Ok(stmt.query_row(params![drive_id, id], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Path relative to the sync root (`""` for the root itself), built by
    /// walking up `parentId`. `None` if the item or one of its ancestors is unknown.
    pub fn path_of(&self, drive_id: &str, id: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT name, type, parentId FROM item WHERE driveId = ?1 AND id = ?2")?;
        let mut parts: Vec<String> = Vec::new();
        let mut current = id.to_owned();
        loop {
            let row: Option<(String, String, Option<String>)> = stmt
                .query_row(params![drive_id, current], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            let Some((name, item_type, parent)) = row else { return Ok(None) };
            if item_type == "root" {
                break;
            }
            parts.push(name);
            match parent {
                Some(p) => current = p,
                None => return Ok(None),
            }
            if parts.len() > 4096 {
                return Ok(None); // cycle guard
            }
        }
        parts.reverse();
        Ok(Some(parts.join("/")))
    }

    pub fn delta_link(&self, drive_id: &str, root_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT deltaLink FROM item WHERE driveId = ?1 AND id = ?2", params![drive_id, root_id], |r| r.get(0))
            .optional()?
            .flatten())
    }

    pub fn set_delta_link(&self, drive_id: &str, root_id: &str, link: Option<&str>) -> Result<()> {
        self.conn.execute("UPDATE item SET deltaLink = ?3 WHERE driveId = ?1 AND id = ?2", params![drive_id, root_id, link])?;
        Ok(())
    }

    /// Total size of the files recorded for a drive (what is synced locally).
    pub fn file_bytes(&self, drive_id: &str) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(CAST(size AS INTEGER)), 0) FROM item WHERE driveId = ?1 AND type = 'file'",
            params![drive_id],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    pub fn count_by_type(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare("SELECT type, count(*) FROM item GROUP BY type ORDER BY type")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Root rows: one per drive in the database.
    pub fn roots(&self) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {COLUMNS} FROM item WHERE type = 'root' ORDER BY driveId"))?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(Item::from_row(row)?);
        }
        Ok(out)
    }

    /// Visit every file item, of one drive or of all (used for verification
    /// against a local tree).
    pub fn for_each_file(&self, drive_id: Option<&str>, mut f: impl FnMut(Item)) -> Result<()> {
        let mut stmt =
            self.conn.prepare(&format!("SELECT {COLUMNS} FROM item WHERE type = 'file' AND (?1 IS NULL OR driveId = ?1)"))?;
        let mut rows = stmt.query(params![drive_id])?;
        while let Some(row) = rows.next()? {
            f(Item::from_row(row)?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, name: &str, t: ItemType, parent: Option<&str>) -> Item {
        Item {
            drive_id: "D".into(),
            id: id.into(),
            name: name.into(),
            remote_name: None,
            item_type: t,
            e_tag: None,
            c_tag: None,
            mtime: "2019-03-05T03:15:20Z".into(),
            parent_id: parent.map(Into::into),
            quick_xor_hash: None,
            sha256_hash: None,
            remote_drive_id: None,
            remote_parent_id: None,
            remote_id: None,
            remote_type: None,
            sync_status: Some("Y".into()),
            size: None,
            reloc_drive_id: None,
            reloc_parent_id: None,
        }
    }

    #[test]
    fn upsert_path_and_cascade() {
        let s = Store::open_in_memory().unwrap();
        s.upsert(&item("R", "root", ItemType::Root, None)).unwrap();
        s.upsert(&item("A", "Docs", ItemType::Dir, Some("R"))).unwrap();
        let mut f = item("F", "a.txt", ItemType::File, Some("A"));
        f.size = Some(42);
        s.upsert(&f).unwrap();

        assert_eq!(s.path_of("D", "R").unwrap().as_deref(), Some(""));
        assert_eq!(s.path_of("D", "F").unwrap().as_deref(), Some("Docs/a.txt"));
        assert_eq!(s.get("D", "F").unwrap().unwrap(), f);

        // Updating the parent must not cascade-delete the child.
        let mut a = item("A", "Documents", ItemType::Dir, Some("R"));
        a.e_tag = Some("x".into());
        s.upsert(&a).unwrap();
        assert_eq!(s.path_of("D", "F").unwrap().as_deref(), Some("Documents/a.txt"));

        s.set_delta_link("D", "R", Some("link")).unwrap();
        assert_eq!(s.delta_link("D", "R").unwrap().as_deref(), Some("link"));

        let mut other = item("G", "b.txt", ItemType::File, None);
        other.drive_id = "E".into();
        s.upsert(&other).unwrap();
        let count = |drive| {
            let mut n = 0;
            s.for_each_file(drive, |_| n += 1).unwrap();
            n
        };
        assert_eq!((count(None), count(Some("D")), count(Some("E"))), (2, 1, 1));
        assert_eq!(s.roots().unwrap().iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["R"]);
        assert_eq!(s.delete_drive("E").unwrap(), 1);
        assert_eq!(s.delete_drive("E").unwrap(), 0);
        assert_eq!(count(Some("D")), 1);

        assert_eq!(s.count_descendants("D", "R").unwrap(), 2);
        assert_eq!(s.count_descendants("D", "F").unwrap(), 0);
        s.delete("D", "A").unwrap();
        assert!(s.get("D", "F").unwrap().is_none());
        assert_eq!(s.children("D", "R").unwrap().len(), 0);
    }

    #[test]
    fn abort_open_transaction() {
        let s = Store::open_in_memory().unwrap();
        s.abort_open_transaction().unwrap(); // no-op outside a transaction
        s.begin().unwrap();
        s.upsert(&item("R", "root", ItemType::Root, None)).unwrap();
        s.abort_open_transaction().unwrap();
        assert!(s.get("D", "R").unwrap().is_none());
        s.begin().unwrap();
        s.commit().unwrap();
    }

    #[test]
    fn import_copies_a_database() {
        let dir = std::env::temp_dir().join(format!("odrs-import-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (src, dest) = (dir.join("src.sqlite3"), dir.join("dest.sqlite3"));
        let s = Store::open(&src).unwrap();
        s.upsert(&item("R", "root", ItemType::Root, None)).unwrap();
        s.set_delta_link("D", "R", Some("cursor")).unwrap();
        drop(s);
        let imported = Store::import(&src, &dest).unwrap();
        assert_eq!(imported.delta_link("D", "R").unwrap().as_deref(), Some("cursor"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn merge_drive_keeps_other_drives() {
        let dir = std::env::temp_dir().join(format!("odrs-merge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src_path = dir.join("library.sqlite3");
        let src = Store::open(&src_path).unwrap();
        let lib = |id: &str, t, parent: Option<&str>| Item { drive_id: "L".into(), ..item(id, id, t, parent) };
        // Children before parents, as rows may come in any order.
        src.conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        src.upsert(&lib("F", ItemType::File, Some("A"))).unwrap();
        src.upsert(&lib("A", ItemType::Dir, Some("LR"))).unwrap();
        src.upsert(&lib("LR", ItemType::Root, None)).unwrap();
        src.set_delta_link("L", "LR", Some("cursor")).unwrap();
        drop(src);

        let ours = Store::open_in_memory().unwrap();
        ours.upsert(&item("R", "root", ItemType::Root, None)).unwrap();
        assert_eq!(ours.merge_drive(&src_path, "L").unwrap(), 3);
        assert_eq!(ours.merge_drive(&src_path, "L").unwrap(), 3, "replaces, does not duplicate");
        assert_eq!(ours.path_of("L", "F").unwrap().as_deref(), Some("A/F"));
        assert_eq!(ours.delta_link("L", "LR").unwrap().as_deref(), Some("cursor"));
        assert!(ours.root("D").unwrap().is_some(), "main drive untouched");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn child_requires_parent() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.upsert(&item("F", "a", ItemType::File, Some("missing"))).is_err());
    }
}
