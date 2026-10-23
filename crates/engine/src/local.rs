//! Local filesystem helpers (blocking; callers run them on a blocking thread
//! when they may be slow, e.g. hashing).

use std::io;
use std::path::{Path, PathBuf};

use filetime::FileTime;
use time::OffsetDateTime;

/// What is currently at a local path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalState {
    Absent,
    Dir,
    File { size: u64, mtime: i64 },
    /// Symlink or special file: never touched.
    Other,
}

pub fn stat(path: &Path) -> io::Result<LocalState> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => Ok(LocalState::Dir),
        Ok(m) if m.is_file() => Ok(LocalState::File {
            size: m.len(),
            mtime: FileTime::from_last_modification_time(&m).unix_seconds(),
        }),
        Ok(_) => Ok(LocalState::Other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(LocalState::Absent),
        Err(e) => Err(e),
    }
}

pub fn set_mtime(path: &Path, t: OffsetDateTime) -> io::Result<()> {
    filetime::set_file_mtime(path, FileTime::from_unix_time(t.unix_timestamp(), 0))
}

/// `<path>.partial`, the private staging file used while downloading (as the D client).
pub fn partial_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".partial");
    PathBuf::from(s)
}

/// Rename `path` out of the way as `<stem>-<device>-safeBackup-NNNN<ext>`,
/// like `safeBackup()` in the D client. Returns the new path.
pub fn safe_backup(path: &Path, device: &str) -> io::Result<PathBuf> {
    let file_name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| io::Error::other("non UTF-8 file name"))?;
    // Same notion of extension as D's std.path.extension: last '.', not at position 0.
    let (stem, ext) = match file_name.rfind('.') {
        Some(i) if i > 0 => file_name.split_at(i),
        _ => (file_name, ""),
    };
    let tag = format!("-{device}-safeBackup-");
    let (base, start) = match stem.len().checked_sub(tag.len() + 4) {
        Some(cut) if stem.is_char_boundary(cut) && stem[cut..].starts_with(&tag) => {
            match stem[cut + tag.len()..].parse::<u32>() {
                Ok(n) if stem[cut + tag.len()..].len() == 4 => (&stem[..cut], n + 1),
                _ => (stem, 1),
            }
        }
        _ => (stem, 1),
    };
    for n in start..=1000 {
        let candidate = path.with_file_name(format!("{base}{tag}{n:04}{ext}"));
        if stat(&candidate)? == LocalState::Absent {
            std::fs::rename(path, &candidate)?;
            return Ok(candidate);
        }
    }
    Err(io::Error::other(format!("no free safeBackup name for {}", path.display())))
}

pub fn device_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names() {
        let dir = std::env::temp_dir().join(format!("odrs-backup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("report.final.docx");
        std::fs::write(&f, b"1").unwrap();
        let b1 = safe_backup(&f, "pc").unwrap();
        assert_eq!(b1.file_name().unwrap(), "report.final-pc-safeBackup-0001.docx");
        std::fs::write(&f, b"2").unwrap();
        let b2 = safe_backup(&f, "pc").unwrap();
        assert_eq!(b2.file_name().unwrap(), "report.final-pc-safeBackup-0002.docx");
        let b3 = safe_backup(&b2, "pc").unwrap();
        assert_eq!(b3.file_name().unwrap(), "report.final-pc-safeBackup-0003.docx");
        let g = dir.join(".bashrc");
        std::fs::write(&g, b"x").unwrap();
        assert_eq!(safe_backup(&g, "pc").unwrap().file_name().unwrap(), ".bashrc-pc-safeBackup-0001");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
