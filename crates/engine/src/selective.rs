//! Filter changes (`sync_list`, `skip_*`). D requires a `--resync` after
//! one; here the change is detected (a fingerprint of the rules is kept per
//! drive) and handled at the start of the next pass:
//!
//! - items now excluded are forgotten (database rows only: the local copy is
//!   kept, and being excluded it is neither uploaded nor deleted online);
//! - the sync cursor is dropped, so the pass enumerates the whole drive and
//!   brings in what the rules now include (identical local files are only
//!   recorded, as in a resync).

use std::path::Path;

use odrs_store::{Item, ItemType, Store};

use crate::Error;
use crate::filter::Filters;

/// Compare the rules with those of the previous pass of this drive; on a
/// change, forget the excluded items and drop the sync cursor. The first time
/// (nothing recorded yet), the current rules are only recorded.
pub(crate) fn apply_filter_change(store: &Store, root: &Item, filters: &Filters, state_dir: &Path) -> Result<(), Error> {
    let file = state_dir.join(format!("{:016x}", crate::local::fnv1a(&root.drive_id)));
    let current = filters.fingerprint();
    let previous = match std::fs::read_to_string(&file) {
        Ok(p) => Some(p.trim().to_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if previous.as_deref() == Some(current.as_str()) {
        return Ok(());
    }
    if previous.is_some() {
        store.begin()?;
        let forgotten = forget_excluded(store, root, filters)?;
        store.set_delta_link(&root.drive_id, &root.id, None)?;
        store.commit()?;
        tracing::info!(
            drive = %root.drive_id,
            forgotten,
            "filters changed: excluded items forgotten (local copies kept), enumerating the whole drive"
        );
    }
    crate::local::create_dir_all(state_dir, 0o700)?;
    std::fs::write(&file, format!("{current}\n"))?;
    Ok(())
}

/// Drop the database rows of the items `filters` exclude (with everything
/// below them); returns how many excluded files and folders were found.
pub(crate) fn forget_excluded(store: &Store, root: &Item, filters: &Filters) -> Result<u64, Error> {
    let mut forgotten = 0;
    let mut stack = vec![(root.clone(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for child in store.children(&dir.drive_id, &dir.id)? {
            let child_rel = if rel.is_empty() { child.name.clone() } else { format!("{rel}/{}", child.name) };
            let is_dir = child.item_type == ItemType::Dir;
            if filters.excludes(&child.name, &child_rel, is_dir) {
                tracing::debug!(path = %child_rel, "now excluded, forgotten");
                store.delete(&child.drive_id, &child.id)?;
                forgotten += 1;
            } else if is_dir {
                stack.push((child, child_rel));
            }
        }
    }
    Ok(forgotten)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::SyncList;

    fn item(id: &str, name: &str, t: ItemType, parent: Option<&str>) -> Item {
        Item {
            drive_id: "d".into(),
            id: id.into(),
            name: name.into(),
            remote_name: None,
            item_type: t,
            e_tag: None,
            c_tag: None,
            mtime: "2026-01-01T00:00:00Z".into(),
            parent_id: parent.map(Into::into),
            quick_xor_hash: None,
            sha256_hash: None,
            remote_drive_id: None,
            remote_parent_id: None,
            remote_id: None,
            remote_type: None,
            sync_status: Some("Y".into()),
            size: None,
            reloc_drive_id: None,
            reloc_parent_id: None,
        }
    }

    #[test]
    fn rule_changes_forget_excluded_items() {
        let dir = std::env::temp_dir().join(format!("odrs-selective-{}", std::process::id()));
        let store = Store::open_in_memory().unwrap();
        let root = item("r", "root", ItemType::Root, None);
        for i in [
            root.clone(),
            item("a", "Documents", ItemType::Dir, Some("r")),
            item("a1", "report.docx", ItemType::File, Some("a")),
            item("b", "Pictures", ItemType::Dir, Some("r")),
            item("b1", "cat.jpg", ItemType::File, Some("b")),
            item("c", "top.txt", ItemType::File, Some("r")),
        ] {
            store.upsert(&i).unwrap();
        }
        store.set_delta_link("d", "r", Some("https://delta")).unwrap();
        let state = dir.join("filters");

        // First pass: the rules are only recorded.
        let only_docs = Filters { sync_list: SyncList::parse("/Documents").0, ..Default::default() };
        apply_filter_change(&store, &root, &only_docs, &state).unwrap();
        assert!(store.get("d", "b1").unwrap().is_some());
        assert!(store.delta_link("d", "r").unwrap().is_some());

        // Same rules: nothing to do.
        store.delete("d", "c").unwrap();
        apply_filter_change(&store, &root, &only_docs, &state).unwrap();
        assert!(store.delta_link("d", "r").unwrap().is_some());

        // New rules: Pictures (and what it contains) forgotten, full enumeration.
        let with_root_files = Filters { sync_root_files: true, ..only_docs };
        apply_filter_change(&store, &root, &with_root_files, &state).unwrap();
        assert!(store.get("d", "b").unwrap().is_none());
        assert!(store.get("d", "b1").unwrap().is_none());
        assert!(store.get("d", "a1").unwrap().is_some());
        assert!(store.delta_link("d", "r").unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
