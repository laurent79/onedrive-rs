//! Filter changes (`sync_list`, `skip_*`). D requires a `--resync` after
//! one; here the change is detected (a fingerprint of the rules is kept per
//! drive) and handled at the start of the next pass:
//!
//! - items now excluded are forgotten (database rows only: the local copy is
//!   kept, and being excluded it is neither uploaded nor deleted online),
//!   unless asked to delete it (only what is unchanged since its sync);
//! - the sync cursor is dropped, so the pass enumerates the whole drive and
//!   brings in what the rules now include (identical local files are only
//!   recorded, as in a resync).

use std::path::Path;

use odrs_store::{Item, ItemType, Store};

use crate::Error;
use crate::convert::parse_graph_time;
use crate::filter::Filters;
use crate::local::{self, LocalState};

/// Compare the rules with those of the previous pass of this drive; on a
/// change, forget the excluded items and drop the sync cursor. The first time
/// (nothing recorded yet), the current rules are only recorded. `delete_in`:
/// the sync folder, to delete the local copies of excluded items as well.
pub(crate) fn apply_filter_change(
    store: &Store,
    root: &Item,
    filters: &Filters,
    state_dir: &Path,
    delete_in: Option<&Path>,
) -> Result<(), Error> {
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
        let forgotten = forget_excluded(store, root, filters, delete_in)?;
        store.set_delta_link(&root.drive_id, &root.id, None)?;
        store.commit()?;
        tracing::info!(
            drive = %root.drive_id,
            forgotten,
            local_copies = if delete_in.is_some() { "deleted when unchanged" } else { "kept" },
            "filters changed: excluded items forgotten, enumerating the whole drive"
        );
    }
    crate::local::create_dir_all(state_dir, 0o700)?;
    std::fs::write(&file, format!("{current}\n"))?;
    Ok(())
}

/// Drop the database rows of the items `filters` exclude (with everything
/// below them), and their local copies in `delete_in`; returns how many
/// excluded files and folders were found.
pub(crate) fn forget_excluded(store: &Store, root: &Item, filters: &Filters, delete_in: Option<&Path>) -> Result<u64, Error> {
    let mut forgotten = 0;
    let mut stack = vec![(root.clone(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for child in store.children(&dir.drive_id, &dir.id)? {
            let child_rel = if rel.is_empty() { child.name.clone() } else { format!("{rel}/{}", child.name) };
            let is_dir = child.item_type == ItemType::Dir;
            if filters.excludes(&child.name, &child_rel, is_dir) {
                tracing::debug!(path = %child_rel, "now excluded, forgotten");
                if let Some(dir) = delete_in {
                    delete_local(store, &child, &dir.join(&child_rel))?;
                }
                store.delete(&child.drive_id, &child.id)?;
                forgotten += 1;
            } else if is_dir {
                stack.push((child, child_rel));
            }
        }
    }
    Ok(forgotten)
}

/// Delete the local copy of a synced item: files whose mtime is still the
/// synced one (D's `isItemSynced`), then folders left empty. Anything else
/// (changes, files never synced) stays.
fn delete_local(store: &Store, item: &Item, path: &Path) -> Result<(), Error> {
    match (item.item_type, local::stat(path)?) {
        (ItemType::File, LocalState::File { mtime, .. }) => {
            if parse_graph_time(&item.mtime).is_some_and(|t| t.unix_timestamp() == mtime) {
                std::fs::remove_file(path)?;
            } else {
                tracing::info!(path = %path.display(), "excluded but changed locally: kept");
            }
        }
        (ItemType::Dir, LocalState::Dir) => {
            for child in store.children(&item.drive_id, &item.id)? {
                delete_local(store, &child, &path.join(&child.name))?;
            }
            match std::fs::remove_dir(path) {
                Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    tracing::info!(path = %path.display(), "excluded folder holds other files: kept");
                }
                r => r?,
            }
        }
        _ => {}
    }
    Ok(())
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
        apply_filter_change(&store, &root, &only_docs, &state, None).unwrap();
        assert!(store.get("d", "b1").unwrap().is_some());
        assert!(store.delta_link("d", "r").unwrap().is_some());

        // Same rules: nothing to do.
        store.delete("d", "c").unwrap();
        apply_filter_change(&store, &root, &only_docs, &state, None).unwrap();
        assert!(store.delta_link("d", "r").unwrap().is_some());

        // New rules: Pictures (and what it contains) forgotten, full enumeration.
        let with_root_files = Filters { sync_root_files: true, ..only_docs };
        apply_filter_change(&store, &root, &with_root_files, &state, None).unwrap();
        assert!(store.get("d", "b").unwrap().is_none());
        assert!(store.get("d", "b1").unwrap().is_none());
        assert!(store.get("d", "a1").unwrap().is_some());
        assert!(store.delta_link("d", "r").unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn local_copies_deleted_on_request() {
        let dir = std::env::temp_dir().join(format!("odrs-selective-delete-{}", std::process::id()));
        let sync_dir = dir.join("sync");
        std::fs::create_dir_all(sync_dir.join("Pictures/Trip")).unwrap();
        std::fs::create_dir_all(sync_dir.join("Music")).unwrap();
        for f in ["Pictures/cat.jpg", "Pictures/dog.jpg", "Pictures/Trip/a.jpg", "Music/song.mp3", "Music/mine.mp3"] {
            std::fs::write(sync_dir.join(f), f).unwrap();
        }
        let mtime = |f: &str| {
            let LocalState::File { mtime, .. } = local::stat(&sync_dir.join(f)).unwrap() else { panic!() };
            crate::convert::format_db_time(time::OffsetDateTime::from_unix_timestamp(mtime).unwrap())
        };
        let file = |id, name, parent, synced: &str| Item { mtime: mtime(synced), ..item(id, name, ItemType::File, Some(parent)) };
        let store = Store::open_in_memory().unwrap();
        let root = item("r", "root", ItemType::Root, None);
        for i in [
            root.clone(),
            item("p", "Pictures", ItemType::Dir, Some("r")),
            file("p1", "cat.jpg", "p", "Pictures/cat.jpg"),
            // Changed locally since its sync.
            Item { mtime: "2001-01-01T00:00:00Z".into(), ..item("p2", "dog.jpg", ItemType::File, Some("p")) },
            item("t", "Trip", ItemType::Dir, Some("p")),
            file("t1", "a.jpg", "t", "Pictures/Trip/a.jpg"),
            item("m", "Music", ItemType::Dir, Some("r")),
            file("m1", "song.mp3", "m", "Music/song.mp3"),
        ] {
            store.upsert(&i).unwrap();
        }
        let state = dir.join("filters");
        apply_filter_change(&store, &root, &Filters::default(), &state, None).unwrap();
        let only_music = Filters { sync_list: SyncList::parse("/Documents").0, ..Default::default() };
        apply_filter_change(&store, &root, &only_music, &state, Some(&sync_dir)).unwrap();
        // Unchanged synced files and the emptied folder go; the changed file and
        // the file never synced stay, with their folders.
        assert!(!sync_dir.join("Pictures/cat.jpg").exists() && !sync_dir.join("Pictures/Trip").exists());
        assert!(sync_dir.join("Pictures/dog.jpg").exists());
        assert!(!sync_dir.join("Music/song.mp3").exists() && sync_dir.join("Music/mine.mp3").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
