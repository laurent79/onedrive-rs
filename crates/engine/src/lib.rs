//! Reconciliation engine. Decisions are pure functions ([`decide`],
//! [`convert`]); filesystem and network I/O stay at the edges.

pub mod convert;
pub mod decide;
mod download;
pub mod filter;
pub mod local;
pub mod moves;
mod selective;
mod sessions;
mod upload;
pub mod verify;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use odrs_graph::Graph;
use odrs_store::Store;

pub use download::DownloadSync;
pub use upload::UploadSync;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Graph(#[from] odrs_graph::Error),
    #[error(transparent)]
    Store(#[from] odrs_store::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid item {0}: {1}")]
    InvalidItem(String, &'static str),
    #[error("local path is occupied by something else: {}", .0.display())]
    Blocked(PathBuf),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("background task failed: {0}")]
    Task(String),
}

impl Error {
    /// Network unreachable (offline, DNS, timeout): worth retrying soon.
    pub fn is_network(&self) -> bool {
        matches!(self, Error::Graph(odrs_graph::Error::Http(e)) if e.is_connect() || e.is_timeout() || e.is_request())
    }

    /// The stored login is no longer valid: the user must run `onedrive-rs login`.
    pub fn is_auth(&self) -> bool {
        matches!(self, Error::Graph(odrs_graph::Error::Auth(_) | odrs_graph::Error::NotLoggedIn))
    }

    /// Errors that must stop the whole sync rather than just the current item.
    pub fn is_fatal(&self) -> bool {
        match self {
            Error::Store(_) | Error::Task(_) => true,
            Error::Graph(e) => matches!(e, odrs_graph::Error::NotLoggedIn | odrs_graph::Error::Auth(_)),
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub sync_dir: PathBuf,
    /// Maximum number of concurrent downloads.
    pub max_transfers: usize,
    /// Used in safeBackup file names.
    pub device_name: String,
    /// Client-side filtering (D: `skip_file`, `skip_dir`, `skip_dotfiles`), both directions.
    pub filters: filter::Filters,
    /// Refuse online deletions of more items than this in one pass (D: `classify_as_big_delete`).
    pub big_delete_threshold: u64,
    /// Report what would be done without changing anything (online or local).
    pub dry_run: bool,
    /// Mode of directories created locally (D: `sync_dir_permissions`, default 700).
    pub dir_mode: u32,
    /// Mode of files downloaded (D: `sync_file_permissions`, default 600).
    pub file_mode: u32,
    /// Drive to sync (e.g. a SharePoint document library); `None` = the account's own OneDrive.
    pub drive_id: Option<String>,
    /// Where interrupted upload sessions are kept to be resumed; `None`: not resumable.
    pub upload_sessions: Option<PathBuf>,
    /// Selective sync rules (D's `sync_list` file), read again at each pass
    /// into `filters.sync_list`; a missing file syncs everything.
    pub sync_list_file: Option<PathBuf>,
    /// Where the filters of each drive's previous pass are remembered, to
    /// notice a change; `None`: filter changes are not handled.
    pub filter_state: Option<PathBuf>,
    /// When the filters changed, also delete the local copies of what they
    /// now exclude (files unchanged since their sync only).
    pub delete_excluded: bool,
}

impl SyncOptions {
    pub fn new(sync_dir: PathBuf) -> Self {
        Self {
            sync_dir,
            max_transfers: 8,
            device_name: local::device_name(),
            filters: filter::Filters::default(),
            big_delete_threshold: 1000,
            dry_run: false,
            dir_mode: 0o700,
            file_mode: 0o600,
            drive_id: None,
            upload_sessions: None,
            sync_list_file: None,
            filter_state: None,
            delete_excluded: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    DownloadOnly,
    Bidirectional,
}

/// Live counters, readable from another task for progress reporting.
#[derive(Debug, Default)]
pub struct Stats {
    pub pages: AtomicU64,
    pub items: AtomicU64,
    pub downloaded: AtomicU64,
    pub bytes: AtomicU64,
    /// Size of the whole drive online (its root folder), for overall progress.
    pub drive_bytes: AtomicU64,
    /// Size of the files already synced when the pass started.
    pub synced_bytes_at_start: AtomicU64,
    /// Size of the files found identical locally and recorded without a
    /// download (resync, takeover): synced too, for overall progress.
    pub recorded_bytes: AtomicU64,
    pub unchanged: AtomicU64,
    pub dirs_created: AtomicU64,
    pub deleted: AtomicU64,
    pub skipped: AtomicU64,
    pub errors: AtomicU64,
    pub uploaded: AtomicU64,
    pub uploaded_bytes: AtomicU64,
    pub folders_created_online: AtomicU64,
    pub deleted_online: AtomicU64,
    pub moved_online: AtomicU64,
    /// Uploads postponed because the file was modified a moment ago.
    pub deferred: AtomicU64,
    /// Local versions kept as safeBackup (changed on both sides, or deleted
    /// online but modified locally).
    pub conflicts: AtomicU64,
    /// The safeBackup copies made during the pass, in order.
    pub backups: Mutex<Vec<Backup>>,
    /// The pass started without a sync cursor and enumerated the whole drive
    /// (first sync, resync, expired cursor).
    pub full_enumeration: AtomicBool,
}

/// A local version moved aside before it was replaced or deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    pub path: PathBuf,
    pub backup: PathBuf,
}

impl Stats {
    pub(crate) fn record_backup(&self, path: &Path, backup: PathBuf) {
        self.conflicts.fetch_add(1, Ordering::Relaxed);
        self.backups.lock().unwrap().push(Backup { path: path.to_owned(), backup });
    }
}

pub(crate) async fn resolve_drive(graph: &Graph, opts: &SyncOptions) -> Result<odrs_graph::Drive, Error> {
    Ok(match &opts.drive_id {
        Some(id) => graph.drive(id).await?,
        None => graph.me_drive().await?,
    })
}

/// Run one pass: apply pending online changes (`/delta`) locally, then, in
/// bidirectional mode, push local changes online (D's default remote-first order).
///
/// In dry-run mode the online changes are not fetched at all; only the local
/// side is compared with the database and reported.
pub async fn sync(graph: Arc<Graph>, store: &Store, mut opts: SyncOptions, stats: Arc<Stats>, mode: SyncMode) -> Result<(), Error> {
    if let Some(file) = &opts.sync_list_file {
        opts.filters.sync_list = filter::SyncList::load(file)?;
    }
    let drive_id = if opts.dry_run {
        let drive = resolve_drive(&graph, &opts).await?;
        match store.root(&drive.id)? {
            Some(root) => root.drive_id,
            None => drive.id,
        }
    } else {
        DownloadSync::new(graph.clone(), store, opts.clone(), stats.clone())
            .keep_local_deletions(mode == SyncMode::Bidirectional)
            .run()
            .await?
    };
    if mode == SyncMode::Bidirectional {
        UploadSync::new(graph, store, opts, stats).run(&drive_id).await?;
    }
    Ok(())
}
