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
//!
//! Before the walk, local moves/renames are detected (see [`crate::moves`]) and
//! applied online as single moves, so they are neither re-uploaded nor deleted.

use std::collections::{BTreeSet, HashMap, HashSet};
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
use crate::moves::{self, Missing, NewDir, NewFile, is_within};
use crate::{Error, Stats, SyncOptions};

enum Outcome {
    Uploaded(Item, u64),
    /// Changed both locally and online: the local version was kept as a
    /// safeBackup and the online version downloaded in its place.
    Conflict(Item),
}

struct Uploaded {
    rel: String,
    result: Result<Outcome, Error>,
}

pub struct UploadSync<'a> {
    graph: Arc<Graph>,
    store: &'a Store,
    opts: SyncOptions,
    stats: Arc<Stats>,
    tasks: JoinSet<Uploaded>,
    /// Tracked items missing locally: (item, path), deleted online after the walk.
    deletions: Vec<(Item, String)>,
    /// Dry run only: detected moves (old and new paths) the walk must not report again.
    moved_old: HashSet<String>,
    moved_new: Vec<String>,
    root_drive: Option<String>,
}

impl<'a> UploadSync<'a> {
    pub fn new(graph: Arc<Graph>, store: &'a Store, opts: SyncOptions, stats: Arc<Stats>) -> Self {
        Self {
            graph,
            store,
            opts,
            stats,
            tasks: JoinSet::new(),
            deletions: Vec::new(),
            moved_old: HashSet::new(),
            moved_new: Vec::new(),
            root_drive: None,
        }
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
        self.root_drive = Some(root.drive_id.clone());
        self.store.begin()?;
        let moved = self.apply_moves(&root).await;
        self.store.commit()?;
        if let Err(e) = moved {
            if e.is_fatal() {
                return Err(e);
            }
            self.stats.errors.fetch_add(1, Ordering::Relaxed);
            tracing::error!(error = %e, "move detection failed; moves will be handled as upload + delete");
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
                (_, None | Some(LocalState::Absent)) if self.moved_old.contains(&child_rel) => {}
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
            if self.moved_new.iter().any(|d| is_within(&child_rel, d)) {
                continue;
            }
            let is_dir = state == LocalState::Dir;
            if self.opts.filters.excludes(&name, &child_rel, is_dir) || !is_valid_onedrive_name(&name) {
                tracing::debug!(path = %child_rel, "not uploading (excluded or invalid OneDrive name)");
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

    /// Local entries worth a move check: valid names not excluded by filters.
    fn local_entries(&self, rel: &str) -> Result<HashMap<String, LocalState>, Error> {
        let mut out = HashMap::new();
        for entry in std::fs::read_dir(self.local_path(rel))? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else { continue };
            let state = local::stat(&entry.path())?;
            let excluded = self.opts.filters.excludes(&name, &Self::join(rel, &name), state == LocalState::Dir);
            if !excluded && is_valid_onedrive_name(&name) {
                out.insert(name, state);
            }
        }
        Ok(out)
    }

    /// Whole-tree scan for tracked items missing locally and untracked local entries.
    fn collect_changes(&self, root: &Item) -> Result<(Vec<Missing>, Vec<NewFile>, Vec<NewDir>), Error> {
        let (mut missing, mut new_files, mut new_dirs) = (Vec::new(), Vec::new(), Vec::new());
        // (tracked folder or None for a new one, path, index in new_dirs)
        let mut stack: Vec<(Option<Item>, String, Option<usize>)> = vec![(Some(root.clone()), String::new(), None)];
        while let Some((dir, rel, new_idx)) = stack.pop() {
            let mut local = self.local_entries(&rel)?;
            if let Some(i) = new_idx {
                new_dirs[i] = NewDir { rel: rel.clone(), children: local.keys().cloned().collect() };
            }
            if let Some(dir) = dir {
                for child in self.store.children(&dir.drive_id, &dir.id)? {
                    let child_rel = Self::join(&rel, &child.name);
                    match local.remove(&child.name) {
                        None | Some(LocalState::Absent) => {
                            let children: BTreeSet<String> = match child.item_type {
                                ItemType::Dir => self.store.children(&child.drive_id, &child.id)?.into_iter().map(|c| c.name).collect(),
                                _ => BTreeSet::new(),
                            };
                            let mtime = parse_graph_time(&child.mtime).map_or(0, OffsetDateTime::unix_timestamp);
                            missing.push(Missing { item: child, rel: child_rel, children, mtime });
                        }
                        Some(LocalState::Dir) if child.item_type == ItemType::Dir => stack.push((Some(child), child_rel, None)),
                        Some(_) => {}
                    }
                }
            }
            for (name, state) in local {
                let child_rel = Self::join(&rel, &name);
                match state {
                    LocalState::Dir => {
                        new_dirs.push(NewDir { rel: child_rel.clone(), children: BTreeSet::new() });
                        stack.push((None, child_rel, Some(new_dirs.len() - 1)));
                    }
                    LocalState::File { size, mtime } => new_files.push(NewFile { rel: child_rel, size, mtime }),
                    _ => {}
                }
            }
        }
        Ok((missing, new_files, new_dirs))
    }

    async fn apply_moves(&mut self, root: &Item) -> Result<(), Error> {
        let (missing, new_files, new_dirs) = self.collect_changes(root)?;
        if missing.is_empty() || (new_files.is_empty() && new_dirs.is_empty()) {
            return Ok(());
        }
        // (item, old path, new path)
        let mut found: Vec<(Item, String, String)> = Vec::new();
        let dir_matches = moves::match_dirs(&missing, &new_dirs);
        for &(m, n) in &dir_matches {
            found.push((missing[m].item.clone(), missing[m].rel.clone(), new_dirs[n].rel.clone()));
        }
        // Files inside a moved folder move with it.
        let moved_dirs: Vec<&str> = dir_matches.iter().map(|&(_, n)| new_dirs[n].rel.as_str()).collect();
        let new_files: Vec<NewFile> = new_files.into_iter().filter(|f| !moved_dirs.iter().any(|d| is_within(&f.rel, d))).collect();
        let mut used = HashSet::new();
        for (m, candidates) in moves::file_candidates(&missing, &new_files) {
            for c in candidates {
                if used.contains(&c) {
                    continue;
                }
                let hash = crate::download::hash_file(self.local_path(&new_files[c].rel)).await?;
                if missing[m].item.quick_xor_hash.as_deref() == Some(hash.as_str()) {
                    used.insert(c);
                    found.push((missing[m].item.clone(), missing[m].rel.clone(), new_files[c].rel.clone()));
                    break;
                }
            }
        }
        // Folders first, shallowest first.
        found.sort_by_key(|(item, _, new)| (item.item_type != ItemType::Dir, new.matches('/').count()));
        for (item, old_rel, new_rel) in found {
            if let Err(e) = self.move_online(&item, &old_rel, &new_rel).await {
                if e.is_fatal() {
                    return Err(e);
                }
                // The walk falls back to upload + delete for this one.
                tracing::warn!(from = %old_rel, to = %new_rel, error = %e, "online move failed");
            }
        }
        Ok(())
    }

    async fn move_online(&mut self, item: &Item, old_rel: &str, new_rel: &str) -> Result<(), Error> {
        tracing::info!(from = %old_rel, to = %new_rel, kind = item.item_type.as_str(), dry_run = self.opts.dry_run, "moved locally, moving online");
        if self.opts.dry_run {
            self.stats.moved_online.fetch_add(1, Ordering::Relaxed);
            self.moved_old.insert(old_rel.to_owned());
            self.moved_new.push(new_rel.to_owned());
            return Ok(());
        }
        let (parent_rel, name) = new_rel.rsplit_once('/').unwrap_or(("", new_rel));
        let parent = self.ensure_online_dir(parent_rel).await?;
        let moved = self.graph.move_item(&item.drive_id, &item.id, &parent.id, name).await?;
        self.store.upsert(&to_db_item(&moved, &item.drive_id)?)?;
        self.stats.moved_online.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// The tracked folder at `rel`, creating missing levels online (move target inside a new folder).
    async fn ensure_online_dir(&mut self, rel: &str) -> Result<Item, Error> {
        let mut dir = self.store.root(&self.root_drive()?)?.ok_or(Error::InvalidItem(String::new(), "no root"))?;
        for name in rel.split('/').filter(|n| !n.is_empty()) {
            dir = match self.store.child_by_name(&dir.drive_id, &dir.id, name)? {
                Some(child) if child.item_type == ItemType::Dir => child,
                Some(_) => return Err(Error::Blocked(self.local_path(rel))),
                None => {
                    let created = self.graph.create_folder(&dir.drive_id, &dir.id, name).await?;
                    let item = to_db_item(&created, &dir.drive_id)?;
                    self.store.upsert(&item)?;
                    self.stats.folders_created_online.fetch_add(1, Ordering::Relaxed);
                    item
                }
            };
        }
        Ok(dir)
    }

    fn root_drive(&self) -> Result<String, Error> {
        self.root_drive.clone().ok_or(Error::InvalidItem(String::new(), "no root"))
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
            let state = local::stat(&entry.path())?;
            let child_rel = Self::join(rel, &name);
            if self.opts.filters.excludes(&name, &child_rel, state == LocalState::Dir)
                || !is_valid_onedrive_name(&name)
                || self.moved_new.iter().any(|d| is_within(&child_rel, d))
            {
                continue;
            }
            match state {
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
        let file_mode = self.opts.file_mode;
        self.tasks.spawn(async move {
            let result = upload(&graph, &target, &path, size, mtime, &device, file_mode).await;
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
            Ok(Outcome::Uploaded(item, size)) => {
                self.stats.uploaded.fetch_add(1, Ordering::Relaxed);
                self.stats.uploaded_bytes.fetch_add(size, Ordering::Relaxed);
                Ok(self.store.upsert(&item)?)
            }
            Ok(Outcome::Conflict(item)) => {
                self.stats.conflicts.fetch_add(1, Ordering::Relaxed);
                Ok(self.store.upsert(&item)?)
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

/// Upload one file. If it changed online since the last sync (HTTP 412), the
/// local version is kept aside as a safeBackup (uploaded as a new file on the
/// next pass) and the current online version is downloaded in its place, as
/// the D client does; nothing relies on `/delta` re-delivering that item.
async fn upload(
    graph: &Graph,
    target: &UploadTarget,
    path: &std::path::Path,
    size: u64,
    mtime: i64,
    device: &str,
    file_mode: u32,
) -> Result<Outcome, Error> {
    let local_hash = crate::download::hash_file(path.to_owned()).await?;
    let mtime_str = OffsetDateTime::from_unix_timestamp(mtime).map(format_db_time).unwrap_or_default();
    let drive_id = match target {
        UploadTarget::New { drive_id, .. } | UploadTarget::Replace { drive_id, .. } => drive_id.clone(),
    };
    let uploaded = match graph.upload_file(target, path, size, &mtime_str).await {
        Ok(item) => item,
        Err(e) if e.status() == Some(412) => {
            let UploadTarget::Replace { item_id, .. } = target else { return Err(e.into()) };
            let backup = local::safe_backup(path, device)?;
            tracing::warn!(path = %path.display(), backup = %backup.display(), "changed both locally and online; local version kept as safeBackup");
            let online = to_db_item(&graph.get_item(&drive_id, item_id).await?, &drive_id)?;
            let online_mtime = parse_graph_time(&online.mtime).ok_or(Error::InvalidItem(online.id.clone(), "bad mtime"))?;
            crate::download::download(graph, &online, path, online_mtime, None, file_mode).await?;
            return Ok(Outcome::Conflict(online));
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
    Ok(Outcome::Uploaded(item, size))
}
