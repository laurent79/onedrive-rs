//! Reconciliation engine. Decisions are pure functions ([`decide`],
//! [`convert`]); filesystem and network I/O stay at the edges.

pub mod convert;
pub mod decide;
mod download;
pub mod local;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use odrs_graph::Graph;
use odrs_store::Store;

pub use download::DownloadSync;

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
}

/// Run one download-only pass: fetch all pending `/delta` changes and apply them locally.
pub async fn sync_download_only(graph: Arc<Graph>, store: &Store, opts: SyncOptions, stats: Arc<Stats>) -> Result<(), Error> {
    DownloadSync::new(graph, store, opts, stats).run().await
}
