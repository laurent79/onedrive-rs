//! Upload side of a bidirectional pass: walk the local tree against the
//! database and push local changes online (D: database consistency pass +
//! local filesystem scan).
//!
//! Directories are visited one at a time: the DB children of a folder are
//! compared with its `read_dir` entries, so memory stays proportional to the
//! largest folder. Online folder creation happens inline (children need the new
//! id); file uploads run concurrently (bounded) and are recorded by the main
//! task, which alone owns the database. Online deletions are collected during
//! the walk and only applied afterwards, if they stay under the big-delete limit.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use odrs_graph::{Graph, UploadTarget};
use odrs_store::{Item, ItemType, Store};
use time::OffsetDateTime;
use tokio::task::JoinSet;

use crate::convert::{format_db_time, parse_graph_time, to_db_item};
use crate::decide::{LocalChange, local_change_with_hash, local_change_without_hash};
use crate::filter::is_valid_onedrive_name;
use crate::local::{self, LocalState};
use crate::{Error, Stats, SyncOptions};

struct Uploaded {
    rel: String,
    result: Result<Option<(Item, u64)>, Error>,
}

pub struct UploadSync<'a> {
    graph: Arc<Graph>,
    store: &'a Store,
    opts: SyncOptions,
    stats: Arc<Stats>,
    tasks: JoinSet<Uploaded>,
    /// Tracked items missing locally: (item, path), deleted online after the walk.
    deletions: Vec<(Item, String)>,
}

impl<'a> UploadSync<'a> {
    pub fn new(graph: Arc<Graph>, store: &'a Store, opts: SyncOptions, stats: Arc<Stats>) -> Self {
        Self { graph, store, opts, stats, tasks: JoinSet::new(), deletions: Vec::new() }
    }

    pub async fn run(mut self, drive_id: &str) -> Result<(), Error> {
        self.store.abort_open_transaction()?;
        let root = self.store.root(drive_id)?.ok_or(Error::InvalidItem(drive_id.to_owned(), "no root in database; run a download first"))?;
        if local::stat(&self.opts.sync_dir)? != LocalState::Dir {
            // Never interpret a missing (e.g. unmounted) sync dir as "everything was deleted".
            return Err(Error::Blocked(self.opts.sync_dir.clone()));
        }
        if self.opts.dry_run {
            tracing::info!("dry run: no change will be made online or locally");
        }
        let mut stack = vec![(root, String::new())];
        while let Some((dir, rel)) = stack.pop() {
            self.store.begin()?;
            let result = self.visit(&dir, &rel, &mut stack).await;
            self.reap_ready()?;
            self.store.commit()?;
            if let Err(e) = result {
                if e.is_fatal() {
                    return Err(e);
                }
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(path = %rel, error = %e, "failed to process local folder");
            }
        }
        self.store.begin()?;
        self.drain().await?;
        self.store.commit()?;
        self.apply_deletions().await
    }

    fn local_path(&self, rel: &str) -> PathBuf {
        if rel.is_empty() { self.opts.sync_dir.clone() } else { self.opts.sync_dir.join(rel) }
    }

    fn join(parent: &str, name: &str) -> String {
        if parent.is_empty() { name.to_owned() } else { format!("{parent}/{name}") }
    }

    async fn visit(&mut self, dir: &Item, rel: &str, stack: &mut Vec<(Item, String)>) -> Result<(), Error> {
        let mut local: HashMap<String, LocalState> = HashMap::new();
        for entry in std::fs::read_dir(self.local_path(rel))? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else {
                tracing::warn!(path = %rel, name = ?entry.file_name(), "skipping non UTF-8 name");
                continue;
            };
            local.insert(name, local::stat(&entry.path())?);
        }

        for child in self.store.children(&dir.drive_id, &dir.id)? {
            let child_rel = Self::join(rel, &child.name);
            match (child.item_type, local.remove(&child.name)) {
                (_, None | Some(LocalState::Absent)) => self.deletions.push((child, child_rel)),
                (ItemType::Dir, Some(LocalState::Dir)) => stack.push((child, child_rel)),
                (ItemType::File, Some(LocalState::File { size, mtime })) => self.tracked_file(child, child_rel, size, mtime).await?,
                (t, Some(state)) => {
                    tracing::warn!(path = %child_rel, db = t.as_str(), local = ?state, "local type differs from online, skipping");
                    self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        let mut new: Vec<_> = local.into_iter().collect();
        new.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, state) in new {
            let child_rel = Self::join(rel, &name);
            if self.opts.skip_file.matches(&name) || !is_valid_onedrive_name(&name) {
                tracing::debug!(path = %child_rel, "not uploading (skip_file or invalid OneDrive name)");
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            match state {
                LocalState::Dir => {
                    if let Some(created) = self.create_folder(dir, &name, &child_rel).await? {
                        stack.push((created, child_rel));
                    }
                }
                LocalState::File { size, mtime } => {
                    let target = UploadTarget::New { drive_id: dir.drive_id.clone(), parent_id: dir.id.clone(), name };
                    self.spawn_upload(target, child_rel, size, mtime).await?;
                }
                LocalState::Other | LocalState::Absent => {
                    tracing::debug!(path = %child_rel, "skipping symlink or special file");
                    self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Ok(())
    }

    async fn tracked_file(&mut self, db: Item, rel: String, size: u64, mtime: i64) -> Result<(), Error> {
        let db_time = parse_graph_time(&db.mtime).ok_or(Error::InvalidItem(db.id.clone(), "bad mtime in database"))?;
        let change = match local_change_without_hash(&db, size, mtime, db_time.unix_timestamp()) {
            Some(c) => c,
            None => {
                let hash = crate::download::hash_file(self.local_path(&rel)).await?;
                local_change_with_hash(&db, &hash)
            }
        };
        match change {
            LocalChange::Unchanged => Ok(()),
            LocalChange::MtimeOnly => {
                tracing::debug!(path = %rel, "same content, restoring recorded mtime");
                if !self.opts.dry_run {
                    local::set_mtime(&self.local_path(&rel), db_time)?;
                }
                Ok(())
            }
            LocalChange::Modified => {
                let if_match = db.c_tag.clone().or_else(|| db.e_tag.clone());
                let target = UploadTarget::Replace { drive_id: db.drive_id.clone(), item_id: db.id.clone(), if_match };
                self.spawn_upload(target, rel, size, mtime).await
            }
        }
    }

    async fn create_folder(&mut self, parent: &Item, name: &str, rel: &str) -> Result<Option<Item>, Error> {
        tracing::info!(path = %rel, dry_run = self.opts.dry_run, "creating online folder");
        self.stats.folders_created_online.fetch_add(1, Ordering::Relaxed);
        if self.opts.dry_run {
            // Children of a folder that does not exist online would all be new too: count them.
            self.count_new_tree(rel)?;
            return Ok(None);
        }
        let created = self.graph.create_folder(&parent.drive_id, &parent.id, name).await?;
        let item = to_db_item(&created, &parent.drive_id)?;
        self.store.upsert(&item)?;
        Ok(Some(item))
    }

    /// Dry run only: account for the files below a folder that would be created.
    fn count_new_tree(&self, rel: &str) -> Result<(), Error> {
        for entry in std::fs::read_dir(self.local_path(rel))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if self.opts.skip_file.matches(&name) || !is_valid_onedrive_name(&name) {
                continue;
            }
            match local::stat(&entry.path())? {
                LocalState::Dir => {
                    self.stats.folders_created_online.fetch_add(1, Ordering::Relaxed);
                    self.count_new_tree(&Self::join(rel, &name))?;
                }
                LocalState::File { size, .. } => {
                    self.stats.uploaded.fetch_add(1, Ordering::Relaxed);
                    self.stats.uploaded_bytes.fetch_add(size, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn spawn_upload(&mut self, target: UploadTarget, rel: String, size: u64, mtime: i64) -> Result<(), Error> {
        let kind = if matches!(target, UploadTarget::New { .. }) { "new" } else { "modified" };
        tracing::info!(path = %rel, size, kind, dry_run = self.opts.dry_run, "uploading");
        if self.opts.dry_run {
            self.stats.uploaded.fetch_add(1, Ordering::Relaxed);
            self.stats.uploaded_bytes.fetch_add(size, Ordering::Relaxed);
            return Ok(());
        }
        while self.tasks.len() >= self.opts.max_transfers.max(1) {
            self.join_one().await?;
        }
        let graph = self.graph.clone();
        let path = self.local_path(&rel);
        let device = self.opts.device_name.clone();
        self.tasks.spawn(async move {
            let result = upload(&graph, &target, &path, size, mtime, &device).await;
            Uploaded { rel, result }
        });
        Ok(())
    }

    async fn apply_deletions(&mut self) -> Result<(), Error> {
        if self.deletions.is_empty() {
            return Ok(());
        }
        let mut total = 0u64;
        for (item, _) in &self.deletions {
            total += 1 + self.store.count_descendants(&item.drive_id, &item.id)?;
        }
        if total > self.opts.big_delete_threshold {
            self.stats.errors.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                items = total,
                limit = self.opts.big_delete_threshold,
                "refusing to delete this many items online (missing local data?); check the sync folder"
            );
            return Ok(());
        }
        let deletions = std::mem::take(&mut self.deletions);
        for (item, rel) in deletions {
            tracing::info!(path = %rel, kind = item.item_type.as_str(), dry_run = self.opts.dry_run, "deleted locally, deleting online");
            self.stats.deleted_online.fetch_add(1, Ordering::Relaxed);
            if self.opts.dry_run {
                continue;
            }
            // Files: only if unchanged online since last sync. Folders go to the recycle bin with their content.
            let if_match = (item.item_type == ItemType::File).then(|| item.e_tag.clone()).flatten();
            match self.graph.delete_item(&item.drive_id, &item.id, if_match.as_deref()).await {
                Ok(()) => self.store.delete(&item.drive_id, &item.id)?,
                Err(e) if e.status() == Some(412) => {
                    tracing::warn!(path = %rel, "changed online since last sync: not deleting it, it will be downloaded again");
                }
                Err(e) => {
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(path = %rel, error = %e, "online deletion failed");
                }
            }
        }
        Ok(())
    }

    fn record(&mut self, done: Uploaded) -> Result<(), Error> {
        match done.result {
            Ok(Some((item, size))) => {
                self.stats.uploaded.fetch_add(1, Ordering::Relaxed);
                self.stats.uploaded_bytes.fetch_add(size, Ordering::Relaxed);
                Ok(self.store.upsert(&item)?)
            }
            Ok(None) => {
                self.stats.conflicts.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) if e.is_fatal() => Err(e),
            Err(e) => {
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(path = %done.rel, error = %e, "upload failed");
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

/// Upload one file. `Ok(None)` means a conflict: the file changed online since
/// the last sync, so the local version was kept aside as a safeBackup (it will
/// be uploaded as a new file and the online version downloaded on the next pass).
async fn upload(graph: &Graph, target: &UploadTarget, path: &std::path::Path, size: u64, mtime: i64, device: &str) -> Result<Option<(Item, u64)>, Error> {
    let local_hash = crate::download::hash_file(path.to_owned()).await?;
    let mtime_str = OffsetDateTime::from_unix_timestamp(mtime).map(format_db_time).unwrap_or_default();
    let drive_id = match target {
        UploadTarget::New { drive_id, .. } | UploadTarget::Replace { drive_id, .. } => drive_id.clone(),
    };
    let uploaded = match graph.upload_file(target, path, size, &mtime_str).await {
        Ok(item) => item,
        Err(e) if e.status() == Some(412) => {
            let backup = local::safe_backup(path, device)?;
            tracing::warn!(path = %path.display(), backup = %backup.display(), "changed both locally and online; local version kept as safeBackup");
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let mut item = to_db_item(&uploaded, &drive_id)?;
    if item.quick_xor_hash.as_deref() == Some(local_hash.as_str()) {
        // Simple uploads get a server-side timestamp: align the local file on it (as the D client).
        if let Some(t) = parse_graph_time(&item.mtime)
            && t.unix_timestamp() != mtime
        {
            local::set_mtime(path, t)?;
        }
    } else {
        // Changed during upload, or modified online by SharePoint after upload: record
        // what was sent so the next pass re-uploads (local change) or downloads (online change).
        tracing::warn!(path = %path.display(), online = ?item.quick_xor_hash, local = %local_hash, "online hash differs from uploaded content");
        item.quick_xor_hash = Some(local_hash);
        item.size = Some(size as i64);
        item.mtime = mtime_str;
    }
    Ok(Some((item, size)))
}
