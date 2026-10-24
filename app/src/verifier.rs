//! Integrity checks requested over D-Bus (`Verify`), run by the daemon one
//! drive at a time next to the sync passes. Results are kept in
//! `verify.json` in the config dir, so they survive a restart.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use odrs_engine::verify::{self, Counters, Outcome};
use odrs_ipc::{Server, VerifyFinding, VerifyStatus};
use odrs_store::Store;
use tokio::sync::Notify;

use crate::daemon::Drives;

/// Hashing threads: enough to keep a disk busy without starving the sync.
const THREADS: usize = 2;
const PUBLISH_INTERVAL: Duration = Duration::from_secs(1);
/// Findings kept per drive; the counters stay exact.
const MAX_FINDINGS: usize = 1000;

#[derive(Default)]
struct Inner {
    /// One entry per drive that was ever checked.
    entries: Vec<(VerifyStatus, Vec<VerifyFinding>)>,
    queue: VecDeque<String>,
    /// Drive being checked and its cancel flag.
    running: Option<(String, Arc<AtomicBool>)>,
    shutdown: bool,
}

impl Inner {
    fn entry(&mut self, name: &str) -> &mut (VerifyStatus, Vec<VerifyFinding>) {
        let i = match self.entries.iter().position(|(s, _)| s.name == name) {
            Some(i) => i,
            None => {
                self.entries.push((VerifyStatus { name: name.to_owned(), ..Default::default() }, Vec::new()));
                self.entries.len() - 1
            }
        };
        &mut self.entries[i]
    }
}

/// Shared between the D-Bus methods (queue, cancel, report) and the worker task.
#[derive(Clone)]
pub struct Verifier {
    inner: Arc<Mutex<Inner>>,
    wake: Arc<Notify>,
    db: PathBuf,
    file: PathBuf,
    drives: Arc<Mutex<Drives>>,
}

impl Verifier {
    pub fn new(db: PathBuf, file: PathBuf, drives: Arc<Mutex<Drives>>) -> Self {
        let mut inner = Inner::default();
        match std::fs::read(&file) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(entries) => inner.entries = entries,
                Err(e) => tracing::warn!(error = %e, file = %file.display(), "ignoring unreadable verification results"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(error = %e, file = %file.display(), "cannot read verification results"),
        }
        Self { inner: Arc::new(Mutex::new(inner)), wake: Arc::new(Notify::new()), db, file, drives }
    }

    fn names(&self, name: &str) -> Result<Vec<String>, String> {
        let drives = self.drives.lock().unwrap();
        if name.is_empty() {
            return Ok(drives.targets.iter().map(|t| t.name.clone()).collect());
        }
        match drives.targets.iter().find(|t| t.name.eq_ignore_ascii_case(name)) {
            Some(t) => Ok(vec![t.name.clone()]),
            None => Err(format!("no drive named \"{name}\"")),
        }
    }

    /// Queue a check of `name` (empty: every drive); already queued or running ones are left alone.
    pub fn request(&self, name: &str) -> Result<(), String> {
        let names = self.names(name)?;
        let mut inner = self.inner.lock().unwrap();
        for name in names {
            let busy = inner.queue.contains(&name) || inner.running.as_ref().is_some_and(|(n, _)| *n == name);
            if !busy {
                let (status, _) = inner.entry(&name);
                *status = VerifyStatus { name: name.clone(), state: "queued".into(), ..Default::default() };
                inner.queue.push_back(name);
            }
        }
        drop(inner);
        self.wake.notify_one();
        Ok(())
    }

    /// Stop the check of `name` (empty: all), queued or running.
    pub fn cancel(&self, name: &str) -> Result<(), String> {
        let names = if name.is_empty() { Vec::new() } else { self.names(name)? };
        let wanted = |n: &str| names.is_empty() || names.iter().any(|x| x == n);
        let mut inner = self.inner.lock().unwrap();
        if let Some((n, flag)) = &inner.running
            && wanted(n)
        {
            flag.store(true, Ordering::Relaxed);
        }
        let dropped: Vec<String> = inner.queue.iter().filter(|n| wanted(n)).cloned().collect();
        inner.queue.retain(|n| !wanted(n));
        for n in dropped {
            let (status, _) = inner.entry(&n);
            status.state = "cancelled".into();
            status.finished = crate::daemon::now_rfc3339();
        }
        drop(inner);
        self.wake.notify_one();
        Ok(())
    }

    pub fn report(&self, name: &str) -> Result<Vec<VerifyFinding>, String> {
        let name = self.names(name)?.into_iter().next().ok_or("no drive name given")?;
        let inner = self.inner.lock().unwrap();
        Ok(inner.entries.iter().find(|(s, _)| s.name == name).map(|(_, f)| f.clone()).unwrap_or_default())
    }

    /// Forget a removed drive (cancelling its check).
    pub fn forget(&self, name: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some((n, flag)) = &inner.running
            && n == name
        {
            flag.store(true, Ordering::Relaxed);
        }
        inner.queue.retain(|n| n != name);
        inner.entries.retain(|(s, _)| s.name != name);
        drop(inner);
        self.save();
        self.wake.notify_one();
    }

    /// Cancel everything; the worker returns.
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.shutdown = true;
        if let Some((_, flag)) = &inner.running {
            flag.store(true, Ordering::Relaxed);
        }
        drop(inner);
        self.wake.notify_one();
    }

    async fn publish(&self, server: &Server) {
        let statuses = self.inner.lock().unwrap().entries.iter().map(|(s, _)| s.clone()).collect();
        if let Err(e) = server.set_verifications(statuses).await {
            tracing::debug!(error = %e, "cannot publish verifications");
        }
    }

    /// Keep finished results only: a check cut short by a restart is shown as cancelled.
    fn save(&self) {
        let entries: Vec<_> = {
            let inner = self.inner.lock().unwrap();
            inner
                .entries
                .iter()
                .map(|(s, f)| {
                    let mut s = s.clone();
                    if s.state == "queued" || s.state == "running" {
                        s.state = "cancelled".into();
                        s.finished = crate::daemon::now_rfc3339();
                    }
                    (s, f.clone())
                })
                .collect()
        };
        let write = || -> std::io::Result<()> {
            let tmp = self.file.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec(&entries)?)?;
            std::fs::rename(&tmp, &self.file)
        };
        if let Err(e) = write() {
            tracing::warn!(error = %e, file = %self.file.display(), "cannot save verification results");
        }
    }

    /// Worker: run queued checks one at a time until [`Self::shutdown`].
    pub async fn run(self, server: Server) {
        loop {
            self.publish(&server).await;
            let next = {
                let mut inner = self.inner.lock().unwrap();
                if inner.shutdown {
                    return;
                }
                inner.queue.pop_front()
            };
            match next {
                Some(name) => self.check(&server, name).await,
                None => self.wake.notified().await,
            }
        }
    }

    async fn check(&self, server: &Server, name: String) {
        let target = self.drives.lock().unwrap().targets.iter().find(|t| t.name == name).cloned();
        let Some(target) = target else { return };
        let libraries: Vec<String> = self.drives.lock().unwrap().targets.iter().filter_map(|t| t.opts.drive_id.clone()).collect();
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut inner = self.inner.lock().unwrap();
            inner.running = Some((name.clone(), cancel.clone()));
            let entry = inner.entry(&name);
            entry.0.state = "running".into();
            entry.1.clear();
        }
        tracing::info!(drive = %name, "verification started");
        let counters = Arc::new(Counters::default());
        let db = self.db.clone();
        let mut task = tokio::task::spawn_blocking({
            let (counters, cancel) = (counters.clone(), cancel.clone());
            move || -> anyhow::Result<Vec<VerifyFinding>> {
                let store = Store::open(&db)?;
                let Some(drive_id) = crate::verify::drive_id_of(&store, &target, &libraries)? else {
                    anyhow::bail!("not synchronised yet");
                };
                let dir = &target.opts.sync_dir;
                let jobs = verify::jobs(&store, Some(&drive_id), None)?;
                let findings = Mutex::new(Vec::new());
                verify::run(dir, &jobs, THREADS, &counters, &cancel, |f| findings.lock().unwrap().push(f));
                let mut out = Vec::new();
                // A sync pass may have touched these files meanwhile.
                for f in findings.into_inner().unwrap() {
                    match verify::recheck(&store, dir, &f)? {
                        Some(outcome) if out.len() < MAX_FINDINGS => out.push(VerifyFinding {
                            kind: outcome.as_str().into(),
                            path: verify::path(dir, &f).display().to_string(),
                            detail: f.detail,
                        }),
                        Some(_) => {}
                        None => counters.reclassify(f.outcome, Outcome::Identical),
                    }
                }
                Ok(out)
            }
        });
        let mut tick = tokio::time::interval(PUBLISH_INTERVAL);
        let result = loop {
            tokio::select! {
                r = &mut task => break r.map_err(anyhow::Error::from).and_then(|r| r),
                _ = tick.tick() => {
                    copy(&counters, &mut self.inner.lock().unwrap().entry(&name).0);
                    self.publish(server).await;
                }
            }
        };
        {
            let mut inner = self.inner.lock().unwrap();
            inner.running = None;
            let (status, findings) = inner.entry(&name);
            copy(&counters, status);
            status.finished = crate::daemon::now_rfc3339();
            match result {
                Ok(_) if cancel.load(Ordering::Relaxed) => status.state = "cancelled".into(),
                Ok(f) => {
                    status.state = "done".into();
                    *findings = f;
                }
                Err(e) => {
                    status.state = "failed".into();
                    status.message = format!("{e:#}");
                }
            }
            tracing::info!(
                drive = %name,
                state = status.state,
                identical = status.identical,
                modified = status.modified,
                missing = status.missing,
                unreadable = status.errors,
                same_mtime = status.same_mtime,
                "verification finished"
            );
            for f in findings.iter() {
                tracing::warn!(drive = %name, kind = f.kind, path = f.path, "{}", f.detail);
            }
        }
        self.save();
    }
}

fn copy(c: &Counters, s: &mut VerifyStatus) {
    let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
    s.total = g(&c.total);
    s.done = g(&c.done);
    s.bytes = g(&c.bytes);
    s.identical = g(&c.identical);
    s.same_mtime = g(&c.same_mtime);
    s.modified = g(&c.modified);
    s.missing = g(&c.missing);
    s.errors = g(&c.errors);
}
