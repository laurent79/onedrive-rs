//! Local change notification (inotify via `notify`) for the daemon.
//!
//! The watcher only says *that* something changed under the sync folder; the
//! daemon then runs a normal pass, whose walk finds out *what* changed. This
//! keeps the event handling trivial and the database the single source of truth.

use std::path::{Path, PathBuf};

use notify::{EventKind, RecursiveMode, Watcher as _};
use tokio::sync::mpsc;

pub use notify::Error;

/// Decides whether a changed path is worth a pass (e.g. not a `.partial` file).
pub type Filter = Box<dyn Fn(&Path) -> bool + Send + Sync>;

pub struct Watcher {
    // Dropping it stops the inotify thread.
    _inner: notify::RecommendedWatcher,
}

impl Watcher {
    /// Watch `root` recursively. A message is sent on the returned channel for
    /// every relevant change; callers debounce.
    pub fn new(root: &Path, relevant: Filter) -> Result<(Self, mpsc::UnboundedReceiver<PathBuf>), Error> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                if let Some(path) = event.paths.into_iter().find(|p| relevant(p)) {
                    let _ = tx.send(path);
                }
            }
            Err(e) => tracing::warn!(error = %e, "file watcher error"),
        })?;
        inner.watch(root, RecursiveMode::Recursive)?;
        Ok((Self { _inner: inner }, rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn reports_relevant_changes_in_new_subfolders() {
        let root = std::env::temp_dir().join(format!("odrs-watch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let filter: Filter = Box::new(|p| p.extension().is_none_or(|e| e != "partial"));
        let (_w, mut rx) = Watcher::new(&root, filter).unwrap();

        std::fs::write(root.join("x.partial"), b"ignored").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        assert_eq!(next(&mut rx).await, Some(root.join("sub")));
        // Folders created after start are watched too.
        tokio::time::sleep(Duration::from_millis(200)).await;
        while tokio::time::timeout(Duration::from_millis(100), rx.recv()).await.is_ok() {}
        std::fs::write(root.join("sub/new.txt"), b"hello").unwrap();
        assert_eq!(next(&mut rx).await, Some(root.join("sub/new.txt")));

        std::fs::remove_dir_all(&root).unwrap();
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<PathBuf>) -> Option<PathBuf> {
        tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.ok().flatten()
    }
}
