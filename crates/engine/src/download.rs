//! Download-only synchronisation: apply `/delta` pages to the local tree.
//!
//! Pages are processed one at a time. Folder creation, moves and deletions are
//! applied in delta order on the main task, which alone owns the database.
//! File downloads run concurrently (bounded) and their completion is recorded
//! by the main task; before any operation that could invalidate the path of an
//! in-flight download (folder move, deletion, same item seen again) all
//! in-flight downloads are drained first.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use odrs_graph::{DriveItem, Graph};
use odrs_store::{Item, ItemType, Store};
use time::OffsetDateTime;
use tokio::task::JoinSet;

use crate::convert::{is_safe_name, parse_graph_time, to_db_item};
use crate::decide::{FileAction, decide_with_hash, decide_without_hash, is_local_deletion, may_delete_local};
use crate::local::{self, LocalState};
use crate::{Error, Stats, SyncOptions};

struct Outcome {
    item: Item,
    path: PathBuf,
    result: Result<u64, Error>,
}

pub struct DownloadSync<'a> {
    graph: Arc<Graph>,
    store: &'a Store,
    opts: SyncOptions,
    stats: Arc<Stats>,
    drive_id: String,
    tasks: JoinSet<Outcome>,
    in_flight: HashSet<String>,
    keep_local_deletions: bool,
}

impl<'a> DownloadSync<'a> {
    pub fn new(graph: Arc<Graph>, store: &'a Store, opts: SyncOptions, stats: Arc<Stats>) -> Self {
        Self {
            graph,
            store,
            opts,
            stats,
            drive_id: String::new(),
            tasks: JoinSet::new(),
            in_flight: HashSet::new(),
            keep_local_deletions: false,
        }
    }

    /// Bidirectional mode: do not restore synced items the user deleted locally
    /// (see [`is_local_deletion`]); download-only mode restores them.
    pub fn keep_local_deletions(mut self, on: bool) -> Self {
        self.keep_local_deletions = on;
        self
    }

    /// Returns the drive id of the synchronised root.
    pub async fn run(mut self) -> Result<String, Error> {
        self.store.abort_open_transaction()?;
        local::create_dir_all(&self.opts.sync_dir, self.opts.dir_mode)?;
        let drive = self.graph.me_drive().await?;
        self.graph.set_prefer_add_to_onedrive(drive.is_personal());
        self.drive_id = drive.id.clone();
        let root = self.graph.drive_root(&drive.id).await?;
        let root_item = to_db_item(&root, &drive.id)?;
        let root_drive = root_item.drive_id.clone();
        if self.store.get(&root_drive, &root_item.id)?.is_none() {
            self.store.upsert(&root_item)?;
        }
        tracing::info!(drive = %drive.id, root = %root_item.id, sync_dir = %self.opts.sync_dir.display(), "applying online changes");

        let mut url = match self.store.delta_link(&root_drive, &root_item.id)? {
            Some(link) => link,
            None => Graph::delta_initial_url(&root_drive, &root_item.id),
        };
        let delta_link = loop {
            let page = match self.graph.delta_page(&url).await {
                Ok(p) => p,
                Err(e) if e.status() == Some(410) => {
                    tracing::warn!("delta token expired (410 Gone), restarting a full enumeration");
                    self.store.set_delta_link(&root_drive, &root_item.id, None)?;
                    url = Graph::delta_initial_url(&root_drive, &root_item.id);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            self.stats.pages.fetch_add(1, Ordering::Relaxed);
            self.store.begin()?;
            for item in &page.value {
                self.stats.items.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = self.apply(item).await {
                    if e.is_fatal() {
                        return Err(e);
                    }
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(id = %item.id, name = item.name.as_deref().unwrap_or(""), error = %e, "failed to apply item");
                }
            }
            self.reap_ready()?;
            self.store.commit()?;
            match (page.next_link, page.delta_link) {
                (Some(next), _) => url = next,
                (None, link) => break link,
            }
        };

        self.store.begin()?;
        self.drain().await?;
        let errors = self.stats.errors.load(Ordering::Relaxed);
        match (errors, delta_link) {
            (0, Some(link)) => self.store.set_delta_link(&root_drive, &root_item.id, Some(&link))?,
            (0, None) => tracing::warn!("delta enumeration ended without a deltaLink"),
            (n, _) => tracing::warn!(errors = n, "sync finished with errors; deltaLink not saved, the next run will retry"),
        }
        self.store.commit()?;
        Ok(root_drive)
    }

    fn local_path(&self, rel: &str) -> PathBuf {
        if rel.is_empty() { self.opts.sync_dir.clone() } else { self.opts.sync_dir.join(rel) }
    }

    async fn apply(&mut self, item: &DriveItem) -> Result<(), Error> {
        if item.is_root() {
            // Never let the root be re-typed or re-parented; keep its deltaLink (not an upserted column).
            let db = to_db_item(item, &self.drive_id)?;
            return Ok(self.store.upsert(&db)?);
        }
        let drive_id = item.parent_drive_id().unwrap_or(&self.drive_id).to_owned();
        if item.is_deleted() {
            return self.apply_delete(&drive_id, &item.id).await;
        }
        if item.is_malware() || item.is_onenote_package() || item.is_remote() {
            tracing::debug!(id = %item.id, name = ?item.name, "skipping malware / OneNote / remote item");
            self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let Some(parent_id) = item.parent_id() else {
            self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        // Children of skipped items (e.g. inside OneNote packages) have no parent in the DB.
        let Some(parent_rel) = self.store.path_of(&drive_id, parent_id)? else {
            tracing::debug!(id = %item.id, name = ?item.name, "parent not in database, skipping");
            self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        let name = item.name.as_deref().unwrap_or("");
        if !is_safe_name(name) {
            return Err(Error::InvalidItem(item.id.clone(), "unusable file name"));
        }
        // Like the D client, skip_file applies to downloads as well as uploads.
        if item.is_file() && self.opts.skip_file.matches(name) {
            tracing::debug!(id = %item.id, name, "skip_file match, not downloading");
            self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let db = to_db_item(item, &self.drive_id)?;
        let rel = if parent_rel.is_empty() { name.to_owned() } else { format!("{parent_rel}/{name}") };

        if self.in_flight.contains(&db.id) {
            self.drain().await?;
        }
        let prev = self.store.get(&db.drive_id, &db.id)?;
        if let Some(prev) = &prev {
            self.apply_move(prev, &rel).await?;
        }
        if self.keep_local_deletions && is_local_deletion(prev.as_ref(), &db, local::stat(&self.local_path(&rel))?) {
            tracing::debug!(path = %rel, "deleted locally, not restoring");
            return Ok(self.store.upsert(&db)?);
        }
        match db.item_type {
            ItemType::Dir => self.apply_dir(db, &rel),
            ItemType::File => self.apply_file(prev, db, rel).await,
            _ => {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
    }

    /// The item already exists in the DB under another path: move the local copy along.
    async fn apply_move(&mut self, prev: &Item, new_rel: &str) -> Result<(), Error> {
        let Some(old_rel) = self.store.path_of(&prev.drive_id, &prev.id)? else { return Ok(()) };
        if old_rel == new_rel {
            return Ok(());
        }
        if prev.item_type == ItemType::Dir {
            self.drain().await?;
        }
        let (old, new) = (self.local_path(&old_rel), self.local_path(new_rel));
        if local::stat(&old)? != LocalState::Absent && local::stat(&new)? == LocalState::Absent {
            if let Some(parent) = new.parent() {
                local::create_dir_all(parent, self.opts.dir_mode)?;
            }
            std::fs::rename(&old, &new)?;
            tracing::info!(from = %old_rel, to = %new_rel, "moved");
        }
        Ok(())
    }

    fn apply_dir(&mut self, db: Item, rel: &str) -> Result<(), Error> {
        let path = self.local_path(rel);
        match local::stat(&path)? {
            LocalState::Dir => {}
            LocalState::Absent => {
                local::create_dir_all(&path, self.opts.dir_mode)?;
                self.stats.dirs_created.fetch_add(1, Ordering::Relaxed);
            }
            _ => return Err(Error::Blocked(path)),
        }
        Ok(self.store.upsert(&db)?)
    }

    async fn apply_file(&mut self, prev: Option<Item>, db: Item, rel: String) -> Result<(), Error> {
        let path = self.local_path(&rel);
        let mtime = parse_graph_time(&db.mtime).ok_or(Error::InvalidItem(db.id.clone(), "bad mtime"))?;
        let local = local::stat(&path)?;
        let action = match decide_without_hash(prev.as_ref(), &db, local, mtime.unix_timestamp()) {
            Some(a) => a,
            None => {
                let hash = hash_file(path.clone()).await?;
                decide_with_hash(prev.as_ref(), &db, local, mtime.unix_timestamp(), &hash)
            }
        };
        match action {
            FileAction::Blocked => Err(Error::Blocked(path)),
            FileAction::Record { fix_mtime } => {
                if fix_mtime {
                    local::set_mtime(&path, mtime)?;
                }
                self.stats.unchanged.fetch_add(1, Ordering::Relaxed);
                Ok(self.store.upsert(&db)?)
            }
            FileAction::Download { backup } => {
                while self.tasks.len() >= self.opts.max_transfers.max(1) {
                    self.join_one().await?;
                }
                let device = backup.then(|| self.opts.device_name.clone());
                let file_mode = self.opts.file_mode;
                let graph = self.graph.clone();
                let stats = self.stats.clone();
                self.in_flight.insert(db.id.clone());
                self.tasks.spawn(async move {
                    let result = download(&graph, &db, &path, mtime, device.as_deref(), file_mode).await;
                    if let Ok(bytes) = result {
                        stats.downloaded.fetch_add(1, Ordering::Relaxed);
                        stats.bytes.fetch_add(bytes, Ordering::Relaxed);
                    }
                    Outcome { item: db, path, result }
                });
                Ok(())
            }
        }
    }

    async fn apply_delete(&mut self, drive_id: &str, id: &str) -> Result<(), Error> {
        let Some(prev) = self.store.get(drive_id, id)? else { return Ok(()) };
        if prev.item_type == ItemType::Root {
            return Ok(());
        }
        self.drain().await?;
        if let Some(rel) = self.store.path_of(drive_id, id)? {
            self.delete_local(&prev, &rel).await?;
        }
        Ok(self.store.delete(drive_id, id)?)
    }

    /// Remove the local copy of a deleted item, preserving anything not known to be synced.
    async fn delete_local(&mut self, item: &Item, rel: &str) -> Result<(), Error> {
        let path = self.local_path(rel);
        let state = local::stat(&path)?;
        match item.item_type {
            ItemType::File => {
                if !matches!(state, LocalState::File { .. }) {
                    return Ok(());
                }
                let mtime = parse_graph_time(&item.mtime).map_or(0, OffsetDateTime::unix_timestamp);
                let hash = match &item.quick_xor_hash {
                    Some(_) => Some(hash_file(path.clone()).await?),
                    None => None,
                };
                if may_delete_local(item, state, mtime, hash.as_deref()) {
                    std::fs::remove_file(&path)?;
                    self.stats.deleted.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(path = %rel, "deleted (removed online)");
                } else {
                    let backup = local::safe_backup(&path, &self.opts.device_name)?;
                    tracing::warn!(path = %rel, backup = %backup.display(), "deleted online but modified locally; kept as safeBackup");
                }
            }
            ItemType::Dir => {
                if state != LocalState::Dir {
                    return Ok(());
                }
                for child in self.store.children(&item.drive_id, &item.id)? {
                    let child_rel = format!("{rel}/{}", child.name);
                    Box::pin(self.delete_local(&child, &child_rel)).await?;
                }
                match std::fs::remove_dir(&path) {
                    Ok(()) => {
                        self.stats.deleted.fetch_add(1, Ordering::Relaxed);
                        tracing::info!(path = %rel, "deleted folder (removed online)");
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                        tracing::warn!(path = %rel, "folder deleted online still contains local data; kept");
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn record(&mut self, outcome: Outcome) -> Result<(), Error> {
        self.in_flight.remove(&outcome.item.id);
        match outcome.result {
            Ok(_) => {
                tracing::debug!(path = %outcome.path.display(), "downloaded");
                // The parent may have been deleted while downloading (it was drained first, so not here).
                Ok(self.store.upsert(&outcome.item)?)
            }
            Err(e) if e.is_fatal() => Err(e),
            Err(e) => {
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(path = %outcome.path.display(), error = %e, "download failed");
                Ok(())
            }
        }
    }

    async fn join_one(&mut self) -> Result<(), Error> {
        if let Some(joined) = self.tasks.join_next().await {
            self.record(joined.map_err(|e| Error::Task(e.to_string()))?)?;
        }
        Ok(())
    }

    fn reap_ready(&mut self) -> Result<(), Error> {
        while let Some(joined) = self.tasks.try_join_next() {
            self.record(joined.map_err(|e| Error::Task(e.to_string()))?)?;
        }
        Ok(())
    }

    async fn drain(&mut self) -> Result<(), Error> {
        while !self.tasks.is_empty() {
            self.join_one().await?;
        }
        Ok(())
    }
}

pub(crate) async fn hash_file(path: PathBuf) -> Result<String, Error> {
    tokio::task::spawn_blocking(move || odrs_quickxor::hash_file(&path))
        .await
        .map_err(|e| Error::Task(e.to_string()))?
        .map_err(Error::from)
}

/// Download into `<path>.partial`, verify, set mtime, then atomically replace `path`.
pub(crate) async fn download(
    graph: &Graph,
    item: &Item,
    path: &Path,
    mtime: OffsetDateTime,
    backup_device: Option<&str>,
    file_mode: u32,
) -> Result<u64, Error> {
    let partial = local::partial_path(path);
    let result = async {
        let (hash, size) = graph.download_to(&item.drive_id, &item.id, &partial).await?;
        if let Some(expected) = &item.quick_xor_hash
            && *expected != hash
        {
            return Err(Error::Integrity(format!("QuickXorHash {hash} != expected {expected}")));
        }
        if item.size.is_some_and(|s| s != size as i64) {
            return Err(Error::Integrity(format!("size {size} != expected {:?}", item.size)));
        }
        local::set_permissions(&partial, file_mode)?;
        local::set_mtime(&partial, mtime)?;
        if let Some(device) = backup_device
            && matches!(local::stat(path)?, LocalState::File { .. })
        {
            let b = local::safe_backup(path, device)?;
            tracing::warn!(path = %path.display(), backup = %b.display(), "local file differs from online version; kept as safeBackup");
        }
        std::fs::rename(&partial, path)?;
        Ok(size)
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}
