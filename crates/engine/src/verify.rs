//! Integrity check: hash the files recorded in the database and compare them
//! with a local tree. Read-only on both sides, so it may run next to a sync
//! pass; [`recheck`] then drops the differences that the pass explains.
//!
//! Blocking: callers run it on threads of their own.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use odrs_store::Store;

use crate::convert::parse_graph_time;
use crate::local::{LocalState, stat};

/// What a file looks like compared with its database row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Identical,
    /// Other content but the recorded mtime: in sync for the D client
    /// (`isItemSynced`), typically an Office file enriched by SharePoint after upload.
    SameMtime,
    /// Other content and other mtime: a local change not uploaded yet, or damage.
    Modified,
    Missing,
    NoHash,
    /// Could not be read.
    Error,
}

impl Outcome {
    /// Whether the file needs attention.
    pub fn is_problem(self) -> bool {
        matches!(self, Outcome::Modified | Outcome::Missing | Outcome::Error)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Identical => "identical",
            Outcome::SameMtime => "same-mtime",
            Outcome::Modified => "modified",
            Outcome::Missing => "missing",
            Outcome::NoHash => "no-hash",
            Outcome::Error => "error",
        }
    }
}

/// One file to check, from its database row.
#[derive(Debug, Clone)]
pub struct Job {
    pub drive_id: String,
    pub id: String,
    /// Path relative to the tree root.
    pub rel: String,
    pub hash: Option<String>,
    /// Recorded mtime, Unix seconds.
    pub mtime: i64,
}

/// A file that needs attention.
#[derive(Debug, Clone)]
pub struct Finding {
    pub job: Job,
    pub outcome: Outcome,
    /// Hashes for a modified file, the error for an unreadable one.
    pub detail: String,
}

/// Live counters, readable while [`run`] works.
#[derive(Debug, Default)]
pub struct Counters {
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub bytes: AtomicU64,
    pub identical: AtomicU64,
    pub same_mtime: AtomicU64,
    pub modified: AtomicU64,
    pub missing: AtomicU64,
    pub errors: AtomicU64,
    pub no_hash: AtomicU64,
}

impl Counters {
    fn counter(&self, o: Outcome) -> &AtomicU64 {
        match o {
            Outcome::Identical => &self.identical,
            Outcome::SameMtime => &self.same_mtime,
            Outcome::Modified => &self.modified,
            Outcome::Missing => &self.missing,
            Outcome::NoHash => &self.no_hash,
            Outcome::Error => &self.errors,
        }
    }

    /// Move a finding that [`recheck`] cleared to the outcome found the second time.
    pub fn reclassify(&self, from: Outcome, to: Outcome) {
        self.counter(from).fetch_sub(1, Ordering::Relaxed);
        self.counter(to).fetch_add(1, Ordering::Relaxed);
    }

    pub fn problems(&self) -> u64 {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        g(&self.modified) + g(&self.missing) + g(&self.errors)
    }
}

/// The files of one drive (`None`: every drive in the database), at most `limit`.
/// Collected up front: hashing takes long and must not hold a read
/// transaction (it would stop the sync's WAL checkpoints).
pub fn jobs(store: &Store, drive_id: Option<&str>, limit: Option<usize>) -> Result<Vec<Job>, odrs_store::Error> {
    let mut items = Vec::new();
    store.for_each_file(drive_id, |item| items.push(item))?;
    let mut jobs = Vec::with_capacity(items.len().min(limit.unwrap_or(usize::MAX)));
    for item in items.into_iter().take(limit.unwrap_or(usize::MAX)) {
        // Unreachable rows (an ancestor is missing) are not synced anywhere.
        let Some(rel) = store.path_of(&item.drive_id, &item.id)? else { continue };
        let mtime = parse_graph_time(&item.mtime).map_or(i64::MIN, |t| t.unix_timestamp());
        jobs.push(Job { drive_id: item.drive_id, id: item.id, rel, hash: item.quick_xor_hash, mtime });
    }
    Ok(jobs)
}

/// Check `jobs` under `dir` on `threads` threads, updating `counters`;
/// `on_finding` sees each problem. Stops early when `cancel` is set.
pub fn run(dir: &Path, jobs: &[Job], threads: usize, counters: &Counters, cancel: &AtomicBool, on_finding: impl Fn(Finding) + Sync) {
    counters.total.store(jobs.len() as u64, Ordering::Relaxed);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                while !cancel.load(Ordering::Relaxed)
                    && let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed))
                {
                    let (outcome, detail) = check(dir, job, &counters.bytes);
                    counters.counter(outcome).fetch_add(1, Ordering::Relaxed);
                    counters.done.fetch_add(1, Ordering::Relaxed);
                    if outcome.is_problem() {
                        on_finding(Finding { job: job.clone(), outcome, detail });
                    }
                }
            });
        }
    });
}

/// Look at a finding again once the run is over. `None` when it no longer
/// holds: the database row changed (a sync pass handled the file) or the file
/// now matches; otherwise the current outcome.
pub fn recheck(store: &Store, dir: &Path, finding: &Finding) -> Result<Option<Outcome>, odrs_store::Error> {
    let job = &finding.job;
    let Some(item) = store.get(&job.drive_id, &job.id)? else { return Ok(None) };
    let mtime = parse_graph_time(&item.mtime).map_or(i64::MIN, |t| t.unix_timestamp());
    if item.quick_xor_hash != job.hash || mtime != job.mtime {
        return Ok(None);
    }
    let (outcome, _) = check(dir, job, &AtomicU64::new(0));
    Ok(outcome.is_problem().then_some(outcome))
}

fn check(dir: &Path, job: &Job, bytes: &AtomicU64) -> (Outcome, String) {
    let path = dir.join(&job.rel);
    let local_mtime = match stat(&path) {
        Ok(LocalState::File { size, mtime }) => {
            bytes.fetch_add(size, Ordering::Relaxed);
            mtime
        }
        Ok(_) => return (Outcome::Missing, String::new()),
        Err(e) => return (Outcome::Error, e.to_string()),
    };
    let Some(expected) = &job.hash else { return (Outcome::NoHash, String::new()) };
    match odrs_quickxor::hash_file(&path) {
        Ok(h) if &h == expected => (Outcome::Identical, String::new()),
        Ok(h) if local_mtime == job.mtime => {
            tracing::debug!(path = %path.display(), local = h, db = expected, "same mtime, other content");
            (Outcome::SameMtime, String::new())
        }
        Ok(h) => (Outcome::Modified, format!("local {h}, database {expected}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Outcome::Missing, String::new()),
        Err(e) => (Outcome::Error, e.to_string()),
    }
}

/// Absolute path of a finding.
pub fn path(dir: &Path, finding: &Finding) -> PathBuf {
    dir.join(&finding.job.rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use odrs_store::{Item, ItemType};

    fn item(id: &str, name: &str, t: ItemType, parent: Option<&str>) -> Item {
        Item {
            drive_id: "D".into(),
            id: id.into(),
            name: name.into(),
            remote_name: None,
            item_type: t,
            e_tag: None,
            c_tag: None,
            mtime: "2019-03-05T03:15:20Z".into(),
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
    fn classifies_files() {
        let dir = std::env::temp_dir().join(format!("odrs-verify-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, b"hello").unwrap();
        let hash = odrs_quickxor::hash_file(&path).unwrap();
        let LocalState::File { mtime, .. } = stat(&path).unwrap() else { panic!() };
        let job = |rel: &str, hash: &str, mtime| Job {
            drive_id: "D".into(),
            id: "F".into(),
            rel: rel.into(),
            hash: Some(hash.into()),
            mtime,
        };
        let bytes = AtomicU64::new(0);
        let outcome = |j: &Job| check(&dir, j, &bytes).0;

        assert_eq!(outcome(&job("a.txt", &hash, mtime)), Outcome::Identical);
        assert_eq!(outcome(&job("a.txt", "other", mtime)), Outcome::SameMtime);
        assert_eq!(outcome(&job("a.txt", "other", mtime - 60)), Outcome::Modified);
        assert_eq!(outcome(&job("b.txt", &hash, mtime)), Outcome::Missing);
        assert_eq!(outcome(&Job { hash: None, ..job("a.txt", "", mtime) }), Outcome::NoHash);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_and_recheck() {
        let dir = std::env::temp_dir().join(format!("odrs-verify-run-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Docs")).unwrap();
        std::fs::write(dir.join("Docs/a.txt"), b"hello").unwrap();
        let hash = odrs_quickxor::hash_file(&dir.join("Docs/a.txt")).unwrap();

        let store = Store::open_in_memory().unwrap();
        store.upsert(&item("R", "root", ItemType::Root, None)).unwrap();
        store.upsert(&item("A", "Docs", ItemType::Dir, Some("R"))).unwrap();
        let a = Item { quick_xor_hash: Some(hash.clone()), ..item("F", "a.txt", ItemType::File, Some("A")) };
        store.upsert(&a).unwrap();
        let b = Item { quick_xor_hash: Some(hash), ..item("G", "b.txt", ItemType::File, Some("A")) };
        store.upsert(&b).unwrap();

        let jobs = jobs(&store, Some("D"), None).unwrap();
        assert_eq!(jobs.len(), 2);
        let counters = Counters::default();
        let findings = std::sync::Mutex::new(Vec::new());
        run(&dir, &jobs, 2, &counters, &AtomicBool::new(false), |f| findings.lock().unwrap().push(f));
        let findings = findings.into_inner().unwrap();
        assert_eq!((counters.identical.load(Ordering::Relaxed), counters.missing.load(Ordering::Relaxed)), (1, 1));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].job.rel, "Docs/b.txt");
        assert_eq!(recheck(&store, &dir, &findings[0]).unwrap(), Some(Outcome::Missing));

        // A sync pass changed the row meanwhile: the finding is dropped.
        store.upsert(&Item { quick_xor_hash: Some("new".into()), ..b }).unwrap();
        assert_eq!(recheck(&store, &dir, &findings[0]).unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
