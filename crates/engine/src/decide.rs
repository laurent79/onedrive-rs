//! Pure decisions for download-only reconciliation of a single file.

use odrs_store::Item;

use crate::local::LocalState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    /// Local content already matches online: only record it (fixing the mtime if needed).
    Record { fix_mtime: bool },
    /// Fetch the online content. `backup` keeps the current local file as a safeBackup first.
    Download { backup: bool },
    /// Something that is not a regular file occupies the path.
    Blocked,
}

/// Decision that can be taken from metadata alone, without hashing the local file.
/// `None` means the local file has to be hashed (see [`decide_with_hash`]).
pub fn decide_without_hash(prev: Option<&Item>, new: &Item, local: LocalState, new_mtime: i64) -> Option<FileAction> {
    match local {
        LocalState::Absent => Some(FileAction::Download { backup: false }),
        LocalState::Dir | LocalState::Other => Some(FileAction::Blocked),
        LocalState::File { size, mtime } => {
            let same_meta = Some(size as i64) == new.size && mtime == new_mtime;
            // Online content unchanged since we recorded it and the local file still
            // looks exactly as we left it: trust it.
            let unchanged_online = prev.is_some_and(|p| {
                p.quick_xor_hash.is_some() && p.quick_xor_hash == new.quick_xor_hash && p.size == new.size
            });
            if same_meta && (unchanged_online || new.quick_xor_hash.is_none()) {
                return Some(FileAction::Record { fix_mtime: false });
            }
            None
        }
    }
}

/// Decision once the local file's QuickXorHash is known.
pub fn decide_with_hash(prev: Option<&Item>, new: &Item, local: LocalState, new_mtime: i64, local_hash: &str) -> FileAction {
    let LocalState::File { size, mtime } = local else {
        return decide_without_hash(prev, new, local, new_mtime).unwrap_or(FileAction::Blocked);
    };
    if new.quick_xor_hash.as_deref() == Some(local_hash) && Some(size as i64) == new.size {
        return FileAction::Record { fix_mtime: mtime != new_mtime };
    }
    // The local file is the previously synced version: safe to overwrite.
    if prev.is_some_and(|p| p.quick_xor_hash.as_deref() == Some(local_hash)) {
        return FileAction::Download { backup: false };
    }
    FileAction::Download { backup: true }
}

/// An online deletion may remove the local file only if it is still the synced version.
pub fn may_delete_local(prev: &Item, local: LocalState, prev_mtime: i64, local_hash: Option<&str>) -> bool {
    let LocalState::File { size, mtime } = local else { return false };
    match (&prev.quick_xor_hash, local_hash) {
        (Some(expected), Some(actual)) => expected == actual,
        (None, _) => Some(size as i64) == prev.size && mtime == prev_mtime,
        (Some(_), None) => false,
    }
}

/// State of a tracked file found locally during the upload-side walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalChange {
    Unchanged,
    /// Same content, only the local mtime differs: put the recorded mtime back.
    MtimeOnly,
    /// Content differs from what was last synced: upload it.
    Modified,
}

/// Cheap check (D's `isItemSynced`: whole-second mtime first). `None` means
/// the local file must be hashed.
pub fn local_change_without_hash(db: &Item, size: u64, mtime: i64, db_mtime: i64) -> Option<LocalChange> {
    if mtime == db_mtime && Some(size as i64) == db.size {
        return Some(LocalChange::Unchanged);
    }
    if db.quick_xor_hash.is_none() || Some(size as i64) != db.size {
        // Without a comparable hash, or with another size, the content did change.
        return Some(LocalChange::Modified);
    }
    None
}

pub fn local_change_with_hash(db: &Item, local_hash: &str) -> LocalChange {
    if db.quick_xor_hash.as_deref() == Some(local_hash) { LocalChange::MtimeOnly } else { LocalChange::Modified }
}

#[cfg(test)]
mod tests {
    use super::*;
    use odrs_store::ItemType;

    fn file(hash: Option<&str>, size: i64) -> Item {
        Item {
            drive_id: "D".into(),
            id: "F".into(),
            name: "f".into(),
            remote_name: None,
            item_type: ItemType::File,
            e_tag: None,
            c_tag: None,
            mtime: "2019-03-05T03:15:20Z".into(),
            parent_id: Some("R".into()),
            quick_xor_hash: hash.map(Into::into),
            sha256_hash: None,
            remote_drive_id: None,
            remote_parent_id: None,
            remote_id: None,
            remote_type: None,
            sync_status: Some("Y".into()),
            size: Some(size),
            reloc_drive_id: None,
            reloc_parent_id: None,
        }
    }

    const T: i64 = 1_551_755_720;

    #[test]
    fn absent_or_blocked() {
        let new = file(Some("h"), 3);
        assert_eq!(decide_without_hash(None, &new, LocalState::Absent, T), Some(FileAction::Download { backup: false }));
        assert_eq!(decide_without_hash(None, &new, LocalState::Dir, T), Some(FileAction::Blocked));
    }

    #[test]
    fn fast_path_needs_known_unchanged_content() {
        let new = file(Some("h"), 3);
        let local = LocalState::File { size: 3, mtime: T };
        assert_eq!(decide_without_hash(Some(&file(Some("h"), 3)), &new, local, T), Some(FileAction::Record { fix_mtime: false }));
        // Unknown to the DB, or content changed online: must hash.
        assert_eq!(decide_without_hash(None, &new, local, T), None);
        assert_eq!(decide_without_hash(Some(&file(Some("old"), 3)), &new, local, T), None);
        // Local mtime changed: must hash.
        assert_eq!(decide_without_hash(Some(&file(Some("h"), 3)), &new, LocalState::File { size: 3, mtime: T + 1 }, T), None);
    }

    #[test]
    fn with_hash() {
        let new = file(Some("new"), 3);
        let prev = file(Some("old"), 3);
        let local = LocalState::File { size: 3, mtime: T + 5 };
        assert_eq!(decide_with_hash(None, &new, local, T, "new"), FileAction::Record { fix_mtime: true });
        assert_eq!(decide_with_hash(Some(&prev), &new, local, T, "old"), FileAction::Download { backup: false });
        assert_eq!(decide_with_hash(Some(&prev), &new, local, T, "edited"), FileAction::Download { backup: true });
        assert_eq!(decide_with_hash(None, &new, local, T, "other"), FileAction::Download { backup: true });
    }

    #[test]
    fn local_changes() {
        let db = file(Some("h"), 3);
        assert_eq!(local_change_without_hash(&db, 3, T, T), Some(LocalChange::Unchanged));
        assert_eq!(local_change_without_hash(&db, 4, T + 1, T), Some(LocalChange::Modified));
        assert_eq!(local_change_without_hash(&db, 3, T + 1, T), None);
        assert_eq!(local_change_with_hash(&db, "h"), LocalChange::MtimeOnly);
        assert_eq!(local_change_with_hash(&db, "x"), LocalChange::Modified);
        assert_eq!(local_change_without_hash(&file(None, 3), 3, T + 1, T), Some(LocalChange::Modified));
    }

    #[test]
    fn deletion() {
        let prev = file(Some("h"), 3);
        let local = LocalState::File { size: 3, mtime: T };
        assert!(may_delete_local(&prev, local, T, Some("h")));
        assert!(!may_delete_local(&prev, local, T, Some("x")));
        assert!(!may_delete_local(&prev, LocalState::Dir, T, None));
        let no_hash = file(None, 3);
        assert!(may_delete_local(&no_hash, local, T, None));
        assert!(!may_delete_local(&no_hash, LocalState::File { size: 4, mtime: T }, T, None));
    }
}
