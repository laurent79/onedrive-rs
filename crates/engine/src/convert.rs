//! Pure mapping from Graph JSON items to database rows (`makeDatabaseItem` in
//! the D client's `src/itemdb.d`).

use odrs_graph::DriveItem;
use odrs_store::{Item, ItemType};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::Error;

/// Parse a Graph timestamp and normalise it to whole seconds, UTC.
pub fn parse_graph_time(s: &str) -> Option<OffsetDateTime> {
    let t = OffsetDateTime::parse(s.trim(), &Rfc3339).ok()?;
    t.to_offset(time::UtcOffset::UTC).replace_nanosecond(0).ok()
}

/// Format as stored in the DB, e.g. `2019-03-05T03:15:20Z`.
pub fn format_db_time(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// A name is usable as a single local path component.
pub fn is_safe_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

/// Build the DB row for a live (non-deleted) item. `default_drive_id` is used
/// for the root, which has no usable `parentReference`.
pub fn to_db_item(item: &DriveItem, default_drive_id: &str) -> Result<Item, Error> {
    let mtime = item
        .last_modified()
        .and_then(parse_graph_time)
        .map(format_db_time)
        .ok_or_else(|| Error::InvalidItem(item.id.clone(), "missing or invalid fileSystemInfo.lastModifiedDateTime"))?;

    let item_type = if item.is_root() {
        ItemType::Root
    } else if item.is_remote() {
        ItemType::Remote
    } else if item.is_folder() {
        ItemType::Dir
    } else if item.is_file() {
        ItemType::File
    } else {
        ItemType::Unknown
    };

    let (drive_id, parent_id) = if item.is_root() {
        (item.parent_drive_id().unwrap_or(default_drive_id).to_owned(), None)
    } else {
        let drive = item
            .parent_drive_id()
            .ok_or(Error::InvalidItem(item.id.clone(), "missing parentReference.driveId"))?;
        (drive.to_owned(), item.parent_id().map(str::to_owned))
    };

    let (size, quick_xor_hash, sha256_hash) = match &item.file {
        Some(f) if f.hashes.is_some() => match item.size {
            Some(size) => {
                let qx = item.quick_xor_hash().filter(|h| !h.is_empty()).map(str::to_owned);
                let sha = if qx.is_none() { item.sha256_hash().map(str::to_owned) } else { None };
                (Some(size), qx, sha)
            }
            None => (Some(0), None, None),
        },
        _ => (None, None, None),
    };

    let remote = item.remote_item.as_ref();
    let remote_ref = remote.and_then(|r| r.parent_reference.as_ref());
    Ok(Item {
        drive_id,
        id: item.id.clone(),
        name: item.name.clone().unwrap_or_default(),
        remote_name: None,
        item_type,
        e_tag: item.e_tag.clone(),
        c_tag: item.c_tag.clone(),
        mtime,
        parent_id,
        quick_xor_hash,
        sha256_hash,
        remote_drive_id: remote_ref.and_then(|r| r.drive_id.clone()),
        remote_parent_id: remote_ref.and_then(|r| r.id.clone()),
        remote_id: remote.and_then(|r| r.id.clone()),
        remote_type: remote.map(|r| {
            if r.folder.is_some() {
                ItemType::Dir
            } else if r.file.is_some() {
                ItemType::File
            } else {
                ItemType::Unknown
            }
        }),
        sync_status: Some("Y".to_owned()),
        size,
        reloc_drive_id: None,
        reloc_parent_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> DriveItem {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn time_normalisation() {
        let t = parse_graph_time("2019-03-05T03:15:20.1234567Z").unwrap();
        assert_eq!(format_db_time(t), "2019-03-05T03:15:20Z");
        let t = parse_graph_time("2019-03-05T05:15:20+02:00").unwrap();
        assert_eq!(format_db_time(t), "2019-03-05T03:15:20Z");
        assert!(parse_graph_time("garbage").is_none());
    }

    #[test]
    fn file_item() {
        let i = parse(
            r#"{"id":"F","name":"nut.html","size":9795,"eTag":"e","cTag":"c",
                "file":{"hashes":{"quickXorHash":"vjx4FqWxLHLoaBqk/jVKY97EZ7k=","sha256Hash":"AB"}},
                "fileSystemInfo":{"lastModifiedDateTime":"2019-03-05T03:15:20Z"},
                "parentReference":{"driveId":"D","id":"P"}}"#,
        );
        let db = to_db_item(&i, "X").unwrap();
        assert_eq!(db.item_type, ItemType::File);
        assert_eq!((db.drive_id.as_str(), db.parent_id.as_deref()), ("D", Some("P")));
        assert_eq!(db.size, Some(9795));
        assert_eq!(db.quick_xor_hash.as_deref(), Some("vjx4FqWxLHLoaBqk/jVKY97EZ7k="));
        assert_eq!(db.sha256_hash, None);
        assert_eq!(db.sync_status.as_deref(), Some("Y"));
    }

    #[test]
    fn root_and_dir() {
        let r = parse(r#"{"id":"R","name":"root","root":{},"folder":{},"fileSystemInfo":{"lastModifiedDateTime":"2026-10-21T09:04:48Z"}}"#);
        let db = to_db_item(&r, "D").unwrap();
        assert_eq!((db.item_type, db.drive_id.as_str(), db.parent_id), (ItemType::Root, "D", None));
        assert_eq!(db.size, None);

        let d = parse(r#"{"id":"A","name":"Docs","folder":{},"fileSystemInfo":{"lastModifiedDateTime":"2026-10-21T09:04:48Z"},"parentReference":{"driveId":"D","id":"R"}}"#);
        assert_eq!(to_db_item(&d, "D").unwrap().item_type, ItemType::Dir);
    }

    #[test]
    fn missing_mtime_is_an_error() {
        let i = parse(r#"{"id":"F","name":"a","file":{},"parentReference":{"driveId":"D","id":"R"}}"#);
        assert!(to_db_item(&i, "D").is_err());
    }

    #[test]
    fn names() {
        assert!(is_safe_name("a b.txt"));
        assert!(!is_safe_name(".."));
        assert!(!is_safe_name("a/b"));
        assert!(!is_safe_name(""));
    }
}
