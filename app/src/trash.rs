//! The desktop trash (freedesktop.org Trash specification, home trash), for
//! files the user chose to discard: they stay recoverable from the file manager.

use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use time::OffsetDateTime;

/// Move `path` to the trash; returns where it went. Fails rather than delete
/// when the trash is on another filesystem.
pub fn trash(path: &Path) -> io::Result<PathBuf> {
    let home = dirs::data_dir().ok_or_else(|| io::Error::other("no XDG data directory"))?.join("Trash");
    trash_in(&home, path)
}

fn trash_in(home: &Path, path: &Path) -> io::Result<PathBuf> {
    let (files, info) = (home.join("files"), home.join("info"));
    for dir in [&files, &info] {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let path = std::path::absolute(path)?;
    let name = path.file_name().ok_or_else(|| io::Error::other("no file name"))?.to_string_lossy().into_owned();
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name.as_str(), ""),
    };
    let now = OffsetDateTime::now_utc();
    let text = format!(
        "[Trash Info]\nPath={}\nDeletionDate={:04}-{:02}-{:02}T{:02}:{:02}:{:02}\n",
        encode(&path),
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    for n in 1..1000 {
        let candidate = if n == 1 { name.clone() } else { format!("{stem}.{n}{ext}") };
        // The info file reserves the name (created exclusively), as the specification requires.
        let info_file = info.join(format!("{candidate}.trashinfo"));
        let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&info_file) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        f.write_all(text.as_bytes())?;
        let target = files.join(&candidate);
        if let Err(e) = std::fs::rename(&path, &target) {
            let _ = std::fs::remove_file(&info_file);
            return Err(e);
        }
        return Ok(target);
    }
    Err(io::Error::other("no free name in the trash"))
}

/// Percent-encode a path for a `.trashinfo` file (RFC 2396 style, `/` kept).
fn encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str()
        .as_bytes()
        .iter()
        .map(|&b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_go_to_the_trash_with_their_origin() {
        let dir = std::env::temp_dir().join(format!("odrs-trash-test-{}", std::process::id()));
        let home = dir.join("Trash");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("plan v2.odt");
        for content in ["one", "two"] {
            std::fs::write(&file, content).unwrap();
            trash_in(&home, &file).unwrap();
        }
        assert!(!file.exists());
        assert_eq!(std::fs::read_to_string(home.join("files/plan v2.odt")).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(home.join("files/plan v2.2.odt")).unwrap(), "two");
        let info = std::fs::read_to_string(home.join("info/plan v2.odt.trashinfo")).unwrap();
        assert!(info.contains(&format!("Path={}/plan%20v2.odt\n", encode(&dir))), "{info}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
