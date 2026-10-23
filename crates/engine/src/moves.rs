//! Local move/rename detection (pure part).
//!
//! A local move looks like a tracked item missing at its old path plus an
//! untracked entry at the new one. Pairing them lets the upload side issue a
//! single online move instead of re-uploading and deleting, and keeps a moved
//! large folder from tripping the big-delete guard. Works for changes made
//! while the daemon was not running, unlike inotify move events.

use std::collections::{BTreeSet, HashMap};

use odrs_store::{Item, ItemType};

/// A tracked item missing locally.
#[derive(Debug, Clone)]
pub struct Missing {
    pub item: Item,
    pub rel: String,
    /// Names of its DB children (folders only).
    pub children: BTreeSet<String>,
    /// DB mtime as Unix seconds (files only).
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFile {
    pub rel: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDir {
    pub rel: String,
    /// Names of its local entries.
    pub children: BTreeSet<String>,
}

/// Folder moves: a missing folder and a new folder with exactly the same,
/// non-empty set of child names, when that pairing is unambiguous.
/// Returns (missing index, new dir index).
pub fn match_dirs(missing: &[Missing], new_dirs: &[NewDir]) -> Vec<(usize, usize)> {
    let mut by_children: HashMap<&BTreeSet<String>, Vec<usize>> = HashMap::new();
    for (i, d) in new_dirs.iter().enumerate() {
        if !d.children.is_empty() {
            by_children.entry(&d.children).or_default().push(i);
        }
    }
    let mut missing_by_children: HashMap<&BTreeSet<String>, usize> = HashMap::new();
    for m in missing.iter().filter(|m| m.item.item_type == ItemType::Dir && !m.children.is_empty()) {
        *missing_by_children.entry(&m.children).or_default() += 1;
    }
    missing
        .iter()
        .enumerate()
        .filter(|(_, m)| m.item.item_type == ItemType::Dir && !m.children.is_empty())
        .filter_map(|(i, m)| match (by_children.get(&m.children).map(Vec::as_slice), missing_by_children[&m.children]) {
            (Some([only]), 1) => Some((i, *only)),
            _ => None,
        })
        .collect()
}

/// File move candidates: new files with the same size and mtime as a missing
/// file. The caller confirms with the content hash. Returns
/// (missing index, candidate new file indexes).
pub fn file_candidates(missing: &[Missing], new_files: &[NewFile]) -> Vec<(usize, Vec<usize>)> {
    let mut by_key: HashMap<(u64, i64), Vec<usize>> = HashMap::new();
    for (i, f) in new_files.iter().enumerate() {
        by_key.entry((f.size, f.mtime)).or_default().push(i);
    }
    missing
        .iter()
        .enumerate()
        .filter(|(_, m)| m.item.item_type == ItemType::File && m.item.quick_xor_hash.is_some())
        .filter_map(|(i, m)| {
            let size = u64::try_from(m.item.size?).ok()?;
            by_key.get(&(size, m.mtime)).map(|c| (i, c.clone()))
        })
        .collect()
}

/// `a/b/c` is `a/b` or below it.
pub fn is_within(rel: &str, dir: &str) -> bool {
    rel == dir || rel.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(t: ItemType, size: Option<i64>, hash: Option<&str>) -> Item {
        Item {
            drive_id: "D".into(),
            id: "X".into(),
            name: "x".into(),
            remote_name: None,
            item_type: t,
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
            size,
            reloc_drive_id: None,
            reloc_parent_id: None,
        }
    }

    fn names(n: &[&str]) -> BTreeSet<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dirs_match_on_identical_children() {
        let missing = vec![
            Missing { item: item(ItemType::Dir, None, None), rel: "Old".into(), children: names(&["a", "b"]), mtime: 0 },
            Missing { item: item(ItemType::Dir, None, None), rel: "Empty".into(), children: names(&[]), mtime: 0 },
        ];
        let new = vec![
            NewDir { rel: "Archive/New".into(), children: names(&["a", "b"]) },
            NewDir { rel: "Other".into(), children: names(&["a"]) },
            NewDir { rel: "Empty2".into(), children: names(&[]) },
        ];
        assert_eq!(match_dirs(&missing, &new), vec![(0, 0)]);
        // Ambiguous: two new folders with the same content.
        let mut twice = new.clone();
        twice.push(NewDir { rel: "Copy".into(), children: names(&["a", "b"]) });
        assert!(match_dirs(&missing, &twice).is_empty());
    }

    #[test]
    fn files_match_on_size_and_mtime() {
        let missing = vec![
            Missing { item: item(ItemType::File, Some(10), Some("h")), rel: "a.txt".into(), children: names(&[]), mtime: 5 },
            Missing { item: item(ItemType::File, Some(10), None), rel: "nohash".into(), children: names(&[]), mtime: 5 },
        ];
        let new = vec![
            NewFile { rel: "b.txt".into(), size: 10, mtime: 5 },
            NewFile { rel: "c.txt".into(), size: 10, mtime: 6 },
        ];
        assert_eq!(file_candidates(&missing, &new), vec![(0, vec![0])]);
    }

    #[test]
    fn within() {
        assert!(is_within("a/b/c", "a/b"));
        assert!(is_within("a/b", "a/b"));
        assert!(!is_within("a/bc", "a/b"));
    }
}
