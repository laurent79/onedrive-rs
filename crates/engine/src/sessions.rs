//! Upload sessions kept on disk between runs, one small JSON file per file
//! being uploaded (D: `session_upload.*`), so that an interrupted large upload
//! continues where it stopped. A session is only reused for the same file,
//! unchanged (size and mtime), and before it expires. The URL grants write
//! access to the target: files are private (0600).

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use odrs_graph::SessionStore;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::convert::parse_graph_time;

/// A session this close to its expiry is not worth resuming.
const EXPIRY_MARGIN_SECS: i64 = 60;

#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    path: String,
    size: u64,
    mtime: i64,
    upload_url: String,
    expiration: Option<String>,
}

impl Saved {
    fn expired(&self) -> bool {
        let limit = OffsetDateTime::now_utc().unix_timestamp() + EXPIRY_MARGIN_SECS;
        self.expiration.as_deref().and_then(parse_graph_time).is_some_and(|t| t.unix_timestamp() <= limit)
    }
}

/// The saved session of one upload: `key` identifies the target (drive, new
/// file name or replaced item), `path` the local file.
pub(crate) struct FileSession {
    dir: PathBuf,
    file: PathBuf,
    path: String,
    size: u64,
    mtime: i64,
}

impl FileSession {
    pub(crate) fn new(dir: &Path, key: &str, path: &Path, size: u64, mtime: i64) -> Self {
        let path = path.display().to_string();
        let file = dir.join(format!("{:016x}.json", fnv1a(&format!("{key}\0{path}"))));
        Self { dir: dir.to_owned(), file, path, size, mtime }
    }

    fn write(&self, saved: &Saved) -> std::io::Result<()> {
        crate::local::create_dir_all(&self.dir, 0o700)?;
        let tmp = self.file.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec(saved)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.file)
    }
}

impl SessionStore for FileSession {
    fn load(&self) -> Option<String> {
        let saved: Saved = serde_json::from_slice(&std::fs::read(&self.file).ok()?).ok()?;
        if saved.path != self.path || saved.size != self.size || saved.mtime != self.mtime || saved.expired() {
            // Another version of the file, or too late: a new session is needed.
            let _ = std::fs::remove_file(&self.file);
            return None;
        }
        Some(saved.upload_url)
    }

    fn save(&self, upload_url: &str, expiration: Option<&str>) {
        let saved = Saved {
            path: self.path.clone(),
            size: self.size,
            mtime: self.mtime,
            upload_url: upload_url.to_owned(),
            expiration: expiration.map(str::to_owned),
        };
        if let Err(e) = self.write(&saved) {
            tracing::warn!(error = %e, file = %self.file.display(), "cannot save the upload session; it will not be resumable");
        }
    }

    fn clear(&self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

/// Remove expired or unreadable sessions (files deleted or uploaded otherwise
/// since). Missing directory: nothing to do.
pub(crate) fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = match std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Saved>(&b).ok()) {
            Some(saved) => saved.expired(),
            None => true,
        };
        if stale {
            tracing::debug!(file = %path.display(), "removing an expired upload session");
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Stable across runs and Rust versions (unlike `DefaultHasher`).
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_session_lifecycle() {
        let dir = std::env::temp_dir().join(format!("odrs-sessions-{}", std::process::id()));
        let file = Path::new("/data/video.mp4");
        let session = FileSession::new(&dir, "new:D:P:video.mp4", file, 100, 7);
        assert_eq!(session.load(), None);

        let later = OffsetDateTime::now_utc() + time::Duration::days(1);
        session.save("https://up/1", Some(&crate::convert::format_db_time(later)));
        assert_eq!(session.load().as_deref(), Some("https://up/1"));
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&session.file).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);

        // The file changed: the session is dropped.
        assert_eq!(FileSession::new(&dir, "new:D:P:video.mp4", file, 101, 7).load(), None);
        assert!(!session.file.exists());

        // Another target: another session file.
        assert_ne!(FileSession::new(&dir, "replace:D:I", file, 100, 7).file, session.file);

        session.save("https://up/2", Some("2001-01-01T00:00:00Z"));
        prune(&dir);
        assert!(!session.file.exists());
        session.save("https://up/3", None);
        prune(&dir);
        assert_eq!(session.load().as_deref(), Some("https://up/3"));
        session.clear();
        assert!(!session.file.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
