//! `verify`: hash the files recorded in the database and compare them with the
//! local folders, drive by drive. Read-only on both sides, works while the
//! daemon runs.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use odrs_engine::convert::parse_graph_time;
use odrs_engine::local::{LocalState, stat};
use odrs_store::Store;

use crate::Target;

/// One local tree to check against the database rows of one drive (`None`: all
/// drives of the database, for a D client database given with `--db`).
pub struct Tree {
    pub name: String,
    pub drive_id: Option<String>,
    pub dir: PathBuf,
}

/// Match each configured target with its drive in the database. The main
/// drive's id is not in the config: it is the one root left once the
/// libraries are set aside (no network access needed).
pub fn trees(store: &Store, targets: &[Target], libraries: &[String]) -> Result<Vec<Tree>> {
    let roots = store.roots()?;
    let mut out = Vec::new();
    for t in targets {
        let drive_id = match &t.opts.drive_id {
            Some(id) => roots.iter().find(|r| r.drive_id.eq_ignore_ascii_case(id)).map(|r| r.drive_id.clone()),
            None => {
                let main: Vec<_> =
                    roots.iter().filter(|r| !libraries.iter().any(|l| l.eq_ignore_ascii_case(&r.drive_id))).collect();
                if main.len() > 1 {
                    bail!("cannot tell the main drive apart in the database ({} unknown drives)", main.len());
                }
                main.first().map(|r| r.drive_id.clone())
            }
        };
        match drive_id {
            Some(id) => out.push(Tree { name: t.name.clone(), drive_id: Some(id), dir: t.opts.sync_dir.clone() }),
            None => eprintln!("{}: not in the database yet, skipped", t.name),
        }
    }
    Ok(out)
}

/// What a file looks like compared with its database row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Identical,
    /// Other content but the recorded mtime: in sync for the D client
    /// (`isItemSynced`), typically an Office file enriched by SharePoint after upload.
    SameMtime,
    /// Other content and other mtime: a local change not uploaded yet, or damage.
    Modified,
    Missing,
    NoHash,
    Error,
}

#[derive(Default)]
struct Counts {
    identical: u64,
    same_mtime: u64,
    modified: u64,
    missing: u64,
    no_hash: u64,
    errors: u64,
}

impl Counts {
    fn add(&mut self, o: Outcome) {
        *match o {
            Outcome::Identical => &mut self.identical,
            Outcome::SameMtime => &mut self.same_mtime,
            Outcome::Modified => &mut self.modified,
            Outcome::Missing => &mut self.missing,
            Outcome::NoHash => &mut self.no_hash,
            Outcome::Error => &mut self.errors,
        } += 1;
    }
    fn problems(&self) -> u64 {
        self.modified + self.missing + self.errors
    }
}

struct Job {
    rel: String,
    hash: Option<String>,
    mtime: i64,
}

/// Verify every tree; fails if a file is missing, modified or unreadable.
pub fn run(store: &Store, trees: &[Tree], limit: Option<usize>, threads: usize) -> Result<()> {
    let mut total = Counts::default();
    for tree in trees {
        let counts = verify_tree(store, tree, limit, threads)?;
        println!(
            "{}: {} identical, {} modified, {} missing, {} unreadable, {} with the recorded mtime but other content, {} without QuickXorHash",
            tree.name, counts.identical, counts.modified, counts.missing, counts.errors, counts.same_mtime, counts.no_hash
        );
        total.modified += counts.modified;
        total.missing += counts.missing;
        total.errors += counts.errors;
    }
    if total.problems() > 0 {
        bail!("{} modified, {} missing, {} unreadable", total.modified, total.missing, total.errors);
    }
    Ok(())
}

fn verify_tree(store: &Store, tree: &Tree, limit: Option<usize>, threads: usize) -> Result<Counts> {
    // Collect first: hashing takes long and must not hold a read transaction
    // (it would stop the daemon's WAL checkpoints).
    let mut items = Vec::new();
    store.for_each_file(tree.drive_id.as_deref(), |item| items.push(item))?;
    let mut jobs = Vec::with_capacity(items.len().min(limit.unwrap_or(usize::MAX)));
    for item in items.into_iter().take(limit.unwrap_or(usize::MAX)) {
        // Unreachable rows (an ancestor is missing) are not synced anywhere.
        let Some(rel) = store.path_of(&item.drive_id, &item.id)? else { continue };
        let mtime = parse_graph_time(&item.mtime).map_or(i64::MIN, |t| t.unix_timestamp());
        jobs.push(Job { rel, hash: item.quick_xor_hash, mtime });
    }

    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let bytes = AtomicU64::new(0);
    let counts = Mutex::new(Counts::default());
    let started = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let outcome = check(&tree.dir, job, &bytes);
                    counts.lock().unwrap().add(outcome);
                    done.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        // Progress on stderr while the workers run (results go to stdout).
        let mut last = Instant::now();
        while done.load(Ordering::Relaxed) < jobs.len() {
            std::thread::sleep(Duration::from_millis(200));
            if last.elapsed() >= Duration::from_secs(10) {
                last = Instant::now();
                eprintln!(
                    "{}: {}/{} files, {:.1} GiB hashed, {:.0?}",
                    tree.name,
                    done.load(Ordering::Relaxed),
                    jobs.len(),
                    bytes.load(Ordering::Relaxed) as f64 / (1u64 << 30) as f64,
                    started.elapsed()
                );
            }
        }
    });
    Ok(counts.into_inner().unwrap())
}

fn check(dir: &Path, job: &Job, bytes: &AtomicU64) -> Outcome {
    let path = dir.join(&job.rel);
    let local_mtime = match stat(&path) {
        Ok(LocalState::File { size, mtime }) => {
            bytes.fetch_add(size, Ordering::Relaxed);
            mtime
        }
        Ok(_) => {
            println!("MISSING   {}", path.display());
            return Outcome::Missing;
        }
        Err(e) => {
            println!("ERROR     {}: {e}", path.display());
            return Outcome::Error;
        }
    };
    let Some(expected) = &job.hash else { return Outcome::NoHash };
    match odrs_quickxor::hash_file(&path) {
        Ok(h) if &h == expected => Outcome::Identical,
        Ok(h) if local_mtime == job.mtime => {
            tracing::debug!(path = %path.display(), local = h, db = expected, "same mtime, other content");
            Outcome::SameMtime
        }
        Ok(h) => {
            println!("MODIFIED  {}: local {h}, database {expected}", path.display());
            Outcome::Modified
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("MISSING   {}", path.display());
            Outcome::Missing
        }
        Err(e) => {
            println!("ERROR     {}: {e}", path.display());
            Outcome::Error
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_files() {
        let dir = std::env::temp_dir().join(format!("odrs-verify-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, b"hello").unwrap();
        let hash = odrs_quickxor::hash_file(&path).unwrap();
        let LocalState::File { mtime, .. } = stat(&path).unwrap() else { panic!() };
        let job = |rel: &str, hash: &str, mtime| Job { rel: rel.into(), hash: Some(hash.into()), mtime };
        let bytes = AtomicU64::new(0);

        assert_eq!(check(&dir, &job("a.txt", &hash, mtime), &bytes), Outcome::Identical);
        assert_eq!(check(&dir, &job("a.txt", "other", mtime), &bytes), Outcome::SameMtime);
        assert_eq!(check(&dir, &job("a.txt", "other", mtime - 60), &bytes), Outcome::Modified);
        assert_eq!(check(&dir, &job("b.txt", &hash, mtime), &bytes), Outcome::Missing);
        assert_eq!(check(&dir, &Job { hash: None, ..job("a.txt", "", mtime) }, &bytes), Outcome::NoHash);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
