//! The upload walk in dry-run mode never touches the network, so its
//! decisions can be checked against a temporary tree and database.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use odrs_engine::{Stats, SyncOptions, UploadSync};
use odrs_graph::{AuthConfig, Graph, TokenStore};
use odrs_store::{Item, ItemType, Store};

const T: &str = "2019-03-05T03:15:20Z";
const T_UNIX: i64 = 1_551_755_720;

fn item(id: &str, name: &str, t: ItemType, parent: Option<&str>) -> Item {
    Item {
        drive_id: "D".into(),
        id: id.into(),
        name: name.into(),
        remote_name: None,
        item_type: t,
        e_tag: Some(format!("etag-{id}")),
        c_tag: None,
        mtime: T.into(),
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

/// A tracked file whose DB row matches `content` written locally with mtime T.
fn tracked(store: &Store, dir: &Path, id: &str, rel: &str, parent: &str, content: &[u8]) {
    let path = dir.join(rel);
    std::fs::write(&path, content).unwrap();
    filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(T_UNIX, 0)).unwrap();
    let mut f = item(id, path.file_name().unwrap().to_str().unwrap(), ItemType::File, Some(parent));
    f.size = Some(content.len() as i64);
    f.quick_xor_hash = Some(odrs_quickxor::hash_file(&path).unwrap());
    store.upsert(&f).unwrap();
}

#[tokio::test]
async fn dry_run_reports_local_changes() {
    let tmp = std::env::temp_dir().join(format!("odrs-upload-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let sync_dir = tmp.join("sync");
    std::fs::create_dir_all(sync_dir.join("Docs")).unwrap();
    std::fs::create_dir_all(sync_dir.join("Gone")).unwrap();

    let store = Store::open_in_memory().unwrap();
    store.upsert(&item("R", "root", ItemType::Root, None)).unwrap();
    store.upsert(&item("A", "Docs", ItemType::Dir, Some("R"))).unwrap();
    store.upsert(&item("G", "Gone", ItemType::Dir, Some("R"))).unwrap();
    tracked(&store, &sync_dir, "F1", "Docs/same.txt", "A", b"unchanged");
    tracked(&store, &sync_dir, "F2", "Docs/edited.txt", "A", b"before");
    tracked(&store, &sync_dir, "F3", "Docs/touched.txt", "A", b"touched");
    tracked(&store, &sync_dir, "F4", "Docs/removed.txt", "A", b"bye");
    tracked(&store, &sync_dir, "F5", "Gone/inside.txt", "G", b"x");

    // Local changes since the last sync.
    std::fs::write(sync_dir.join("Docs/edited.txt"), b"after, longer").unwrap();
    filetime::set_file_mtime(sync_dir.join("Docs/touched.txt"), filetime::FileTime::from_unix_time(T_UNIX + 60, 0)).unwrap();
    std::fs::remove_file(sync_dir.join("Docs/removed.txt")).unwrap();
    std::fs::remove_dir_all(sync_dir.join("Gone")).unwrap();
    std::fs::write(sync_dir.join("Docs/new.txt"), b"new file").unwrap();
    std::fs::create_dir_all(sync_dir.join("New folder/Sub")).unwrap();
    std::fs::write(sync_dir.join("New folder/a.bin"), vec![0u8; 1000]).unwrap();
    std::fs::write(sync_dir.join("New folder/Sub/b.bin"), vec![0u8; 500]).unwrap();
    // Never uploaded.
    std::fs::write(sync_dir.join("Docs/~$lock.docx"), b"office lock").unwrap();
    std::fs::write(sync_dir.join("Docs/x.partial"), b"partial").unwrap();
    std::fs::write(sync_dir.join("Docs/bad:name.txt"), b"invalid").unwrap();

    // Never contacted in dry-run mode, but needs a (fake) stored token to exist.
    std::fs::write(tmp.join("refresh_token"), "fake").unwrap();
    let graph = Arc::new(Graph::new(odrs_graph::http_client().unwrap(), AuthConfig::default(), TokenStore::new(tmp.join("refresh_token"))).unwrap());
    let mut opts = SyncOptions::new(sync_dir.clone());
    opts.dry_run = true;
    let stats = Arc::new(Stats::default());
    UploadSync::new(graph, &store, opts, stats.clone()).run("D").await.unwrap();

    let g = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    // edited.txt (modified) + new.txt + the two files of the new folder tree.
    assert_eq!(g(&stats.uploaded), 4);
    assert_eq!(g(&stats.uploaded_bytes), 13 + 8 + 1000 + 500);
    assert_eq!(g(&stats.folders_created_online), 2);
    // removed.txt and the Gone folder.
    assert_eq!(g(&stats.deleted_online), 2);
    assert_eq!(g(&stats.skipped), 3);
    assert_eq!(g(&stats.errors), 0);
    // Dry run: the touched file keeps its new mtime, the DB is unchanged.
    let touched = std::fs::metadata(sync_dir.join("Docs/touched.txt")).unwrap();
    assert_eq!(filetime::FileTime::from_last_modification_time(&touched).unix_seconds(), T_UNIX + 60);
    assert!(store.get("D", "F4").unwrap().is_some());

    std::fs::remove_dir_all(&tmp).unwrap();
}

#[tokio::test]
async fn big_delete_is_refused() {
    let tmp = std::env::temp_dir().join(format!("odrs-bigdelete-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let sync_dir = tmp.join("sync");
    std::fs::create_dir_all(&sync_dir).unwrap();
    let store = Store::open_in_memory().unwrap();
    store.upsert(&item("R", "root", ItemType::Root, None)).unwrap();
    store.upsert(&item("A", "Big", ItemType::Dir, Some("R"))).unwrap();
    for i in 0..20 {
        let mut f = item(&format!("F{i}"), &format!("f{i}"), ItemType::File, Some("A"));
        f.size = Some(1);
        store.upsert(&f).unwrap();
    }
    std::fs::write(tmp.join("refresh_token"), "fake").unwrap();
    let graph = Arc::new(Graph::new(odrs_graph::http_client().unwrap(), AuthConfig::default(), TokenStore::new(tmp.join("refresh_token"))).unwrap());
    let mut opts = SyncOptions::new(sync_dir);
    opts.dry_run = true;
    opts.big_delete_threshold = 10;
    let stats = Arc::new(Stats::default());
    UploadSync::new(graph, &store, opts, stats.clone()).run("D").await.unwrap();
    assert_eq!(stats.deleted_online.load(Ordering::Relaxed), 0);
    assert_eq!(stats.errors.load(Ordering::Relaxed), 1);
    std::fs::remove_dir_all(&tmp).unwrap();
}

#[tokio::test]
async fn moves_are_detected() {
    let tmp = std::env::temp_dir().join(format!("odrs-moves-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let sync_dir = tmp.join("sync");
    std::fs::create_dir_all(sync_dir.join("Docs")).unwrap();
    let store = Store::open_in_memory().unwrap();
    store.upsert(&item("R", "root", ItemType::Root, None)).unwrap();
    store.upsert(&item("A", "Docs", ItemType::Dir, Some("R"))).unwrap();
    tracked(&store, &sync_dir, "F1", "Docs/one.txt", "A", b"one");
    tracked(&store, &sync_dir, "F2", "Docs/two.txt", "A", b"two");
    tracked(&store, &sync_dir, "F3", "lonely.txt", "R", b"lonely content");

    // Folder moved into a new folder and renamed; a file renamed in place.
    std::fs::create_dir_all(sync_dir.join("Archive")).unwrap();
    std::fs::rename(sync_dir.join("Docs"), sync_dir.join("Archive/Docs 2025")).unwrap();
    std::fs::rename(sync_dir.join("lonely.txt"), sync_dir.join("renamed.txt")).unwrap();
    // A genuinely new file with the same size and mtime but other content is not a move.
    std::fs::write(sync_dir.join("decoy.txt"), b"other content!").unwrap();
    filetime::set_file_mtime(sync_dir.join("decoy.txt"), filetime::FileTime::from_unix_time(T_UNIX, 0)).unwrap();

    std::fs::write(tmp.join("refresh_token"), "fake").unwrap();
    let graph = Arc::new(Graph::new(odrs_graph::http_client().unwrap(), AuthConfig::default(), TokenStore::new(tmp.join("refresh_token"))).unwrap());
    let mut opts = SyncOptions::new(sync_dir.clone());
    opts.dry_run = true;
    let stats = Arc::new(Stats::default());
    UploadSync::new(graph, &store, opts, stats.clone()).run("D").await.unwrap();

    let g = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    assert_eq!(g(&stats.moved_online), 2);
    assert_eq!(g(&stats.deleted_online), 0);
    assert_eq!(g(&stats.uploaded), 1, "only decoy.txt is uploaded");
    assert_eq!(g(&stats.folders_created_online), 1, "Archive");
    std::fs::remove_dir_all(&tmp).unwrap();
}
