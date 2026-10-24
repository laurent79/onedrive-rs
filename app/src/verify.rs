//! `verify`: hash the files recorded in the database and compare them with the
//! local folders, drive by drive (see [`odrs_engine::verify`]).

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use odrs_engine::verify::{self, Counters, Outcome};
use odrs_store::Store;

use crate::Target;

/// One local tree to check against the database rows of one drive (`None`: all
/// drives of the database, for a D client database given with `--db`).
pub struct Tree {
    pub name: String,
    pub drive_id: Option<String>,
    pub dir: PathBuf,
}

/// Match each configured target with its drive in the database.
pub fn trees(store: &Store, targets: &[Target], libraries: &[String]) -> Result<Vec<Tree>> {
    let mut out = Vec::new();
    for t in targets {
        match drive_id_of(store, t, libraries)? {
            Some(id) => out.push(Tree { name: t.name.clone(), drive_id: Some(id), dir: t.opts.sync_dir.clone() }),
            None => eprintln!("{}: not in the database yet, skipped", t.name),
        }
    }
    Ok(out)
}

/// Database drive id of a target, `None` if it was never synced. The main
/// drive's id is not in the config: it is the one root left once the
/// `libraries` are set aside (no network access needed).
pub fn drive_id_of(store: &Store, t: &Target, libraries: &[String]) -> Result<Option<String>> {
    let roots = store.roots()?;
    Ok(match &t.opts.drive_id {
        Some(id) => roots.iter().find(|r| r.drive_id.eq_ignore_ascii_case(id)).map(|r| r.drive_id.clone()),
        None => {
            let main: Vec<_> = roots.iter().filter(|r| !libraries.iter().any(|l| l.eq_ignore_ascii_case(&r.drive_id))).collect();
            if main.len() > 1 {
                bail!("cannot tell the main drive apart in the database ({} unknown drives)", main.len());
            }
            main.first().map(|r| r.drive_id.clone())
        }
    })
}

/// Verify every tree; fails if a file is missing, modified or unreadable.
pub fn run(store: &Store, trees: &[Tree], limit: Option<usize>, threads: usize) -> Result<()> {
    let mut problems = 0;
    for tree in trees {
        let jobs = verify::jobs(store, tree.drive_id.as_deref(), limit)?;
        let counters = Counters::default();
        let findings = Mutex::new(Vec::new());
        let done = AtomicBool::new(false);
        let started = Instant::now();
        std::thread::scope(|s| {
            // Progress on stderr while the workers run (results go to stdout).
            s.spawn(|| {
                let mut last = Instant::now();
                while !done.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                    if last.elapsed() >= Duration::from_secs(10) {
                        last = Instant::now();
                        eprintln!(
                            "{}: {}/{} files, {:.1} GiB hashed, {:.0?}",
                            tree.name,
                            counters.done.load(Ordering::Relaxed),
                            jobs.len(),
                            counters.bytes.load(Ordering::Relaxed) as f64 / (1u64 << 30) as f64,
                            started.elapsed()
                        );
                    }
                }
            });
            verify::run(&tree.dir, &jobs, threads, &counters, &AtomicBool::new(false), |f| findings.lock().unwrap().push(f));
            done.store(true, Ordering::Relaxed);
        });
        // A sync pass may have touched these files meanwhile.
        for f in findings.into_inner().unwrap() {
            match verify::recheck(store, &tree.dir, &f)? {
                Some(outcome) => {
                    let label = match outcome {
                        Outcome::Modified => "MODIFIED",
                        Outcome::Missing => "MISSING ",
                        _ => "ERROR   ",
                    };
                    let detail = if f.detail.is_empty() { String::new() } else { format!(": {}", f.detail) };
                    println!("{label}  {}{detail}", verify::path(&tree.dir, &f).display());
                }
                None => counters.reclassify(f.outcome, Outcome::Identical),
            }
        }
        let g = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
        println!(
            "{}: {} identical, {} modified, {} missing, {} unreadable, {} with the recorded mtime but other content, {} without QuickXorHash",
            tree.name,
            g(&counters.identical),
            g(&counters.modified),
            g(&counters.missing),
            g(&counters.errors),
            g(&counters.same_mtime),
            g(&counters.no_hash)
        );
        problems += counters.problems();
    }
    if problems > 0 {
        bail!("{problems} file(s) missing, modified or unreadable");
    }
    Ok(())
}
