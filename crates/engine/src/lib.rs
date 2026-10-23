//! Reconciliation engine. Decisions are pure functions ([`decide`],
//! [`convert`]); filesystem and network I/O stay at the edges.

pub mod convert;
pub mod decide;
mod download;
pub mod filter;
pub mod local;
mod upload;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

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
    /// Local names never uploaded (D: `skip_file`).
    pub skip_file: filter::SkipList,
    /// Refuse online deletions of more items than this in one pass (D: `classify_as_big_delete`).
    pub big_delete_threshold: u64,
    /// Report what would be done without changing anything (online or local).
    pub dry_run: bool,
}

impl SyncOptions {
    pub fn new(sync_dir: PathBuf) -> Self {
        Self {
            sync_dir,
            max_transfers: 8,
            device_name: local::device_name(),
            skip_file: filter::SkipList::default(),
            big_delete_threshold: 1000,
            dry_run: false,
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
    pub unchanged: AtomicU64,
    pub dirs_created: AtomicU64,
    pub deleted: AtomicU64,
    pub skipped: AtomicU64,
    pub errors: AtomicU64,
    pub uploaded: AtomicU64,
    pub uploaded_bytes: AtomicU64,
    pub folders_created_online: AtomicU64,
    pub deleted_online: AtomicU64,
    /// Files changed both locally and online, kept as safeBackup.
    pub conflicts: AtomicU64,
}

/// Run one pass: apply pending online changes (`/delta`) locally, then, in
/// bidirectional mode, push local changes online (D's default remote-first order).
///
/// In dry-run mode the online changes are not fetched at all; only the local
/// side is compared with the database and reported.
pub async fn sync(graph: Arc<Graph>, store: &Store, opts: SyncOptions, stats: Arc<Stats>, mode: SyncMode) -> Result<(), Error> {
    let drive_id = if opts.dry_run {
        let drive = graph.me_drive().await?;
        match store.root(&drive.id)? {
            Some(root) => root.drive_id,
            None => drive.id,
        }
    } else {
        DownloadSync::new(graph.clone(), store, opts.clone(), stats.clone()).run().await?
    };
    if mode == SyncMode::Bidirectional {
        UploadSync::new(graph, store, opts, stats).run(&drive_id).await?;
    }
    Ok(())
}
