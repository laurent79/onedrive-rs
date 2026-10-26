//! `onedrive-rs daemon`: periodic sync passes over every configured drive,
//! controlled over D-Bus.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use odrs_engine::{Stats, SyncMode};
use odrs_graph::{AuthConfig, Graph};
use odrs_ipc::{
    AUTH_ERROR_PREFIX, BoxFuture, Command, DriveManager, DriveStatus, Event, Folder, FolderChoice, Info, Library, OFFLINE_ERROR_PREFIX,
    Progress, Server, Site, State, VerifyFinding,
};
use odrs_store::Store;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::config::{self, Config, DriveConfig};
use crate::verifier::Verifier;
use crate::{Paths, Target};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(2);
/// Retry delay after a pass failed for lack of network.
const OFFLINE_RETRY: Duration = Duration::from_secs(60);
/// Quiet time after a local change before starting a pass.
const DEBOUNCE: Duration = Duration::from_secs(5);
/// Quiet time after an online change notification. Repeated notifications
/// for one change come seconds apart: waiting longer would not merge them.
const REMOTE_DEBOUNCE: Duration = Duration::from_millis(500);
/// Retry delay to find the main drive's id for its notifications (offline at start).
const DRIVE_ID_RETRY: Duration = Duration::from_secs(60);

/// Drive list shared between the sync loop and D-Bus drive management.
pub(crate) struct Drives {
    pub(crate) targets: Vec<Target>,
    /// Drive ids removed since the last pass: their database rows are dropped.
    removed: Vec<String>,
    /// Drives to resync before the next pass (names).
    resync: Vec<String>,
    /// Drives whose new folder selection also deletes the local copies of
    /// what is no longer synced (names), until a pass of the drive succeeds.
    delete_excluded: HashSet<String>,
    /// Bumped on every change, so the loop knows to rebuild its watchers.
    version: u64,
}

pub async fn run(paths: &Paths, auth: AuthConfig, cfg: Config, targets: Vec<Target>, mode: SyncMode, interval: Duration) -> Result<()> {
    let _lock = crate::lock(paths)?;
    let graph = Arc::new(Graph::new(odrs_graph::http_client()?, auth, paths.token_store())?);
    let store = Store::open(&paths.db())?;
    // The account's own OneDrive comes first; shown by clients.
    let sync_dir = targets[0].opts.sync_dir.clone();

    let account = match graph.me_drive().await {
        Ok(d) => d.owner.and_then(|o| o.user).and_then(|u| u.email.or(u.display_name)).unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %e, "could not read the account (offline?)");
            String::new()
        }
    };
    let drives =
        Arc::new(Mutex::new(Drives { targets, removed: Vec::new(), resync: Vec::new(), delete_excluded: HashSet::new(), version: 0 }));
    let verifier = Verifier::new(paths.db(), paths.config_dir.join("verify.json"), drives.clone());
    let realtime = cfg.disable_websocket_support != Some(true);
    let manager = Arc::new(Manager {
        graph: graph.clone(),
        config_path: paths.config(),
        cfg,
        drives: drives.clone(),
        verifier: verifier.clone(),
    });
    let (tx, mut rx) = mpsc::unbounded_channel();
    let info = Info { account, sync_dir: sync_dir.display().to_string() };
    let server = Server::start(info, tx, manager)
        .await
        .context("cannot claim io.github.onedrive_rs.OneDrive1 on the session bus (daemon already running?)")?;
    tracing::info!(interval = ?interval, ?mode, "daemon started");
    let verifier_task = tokio::spawn(verifier.clone().run(server.clone()));
    // Event ids keep increasing across restarts (see odrs_ipc::Event::id).
    let event_ids = AtomicU64::new(
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
    );

    let mut statuses: Vec<DriveStatus> = Vec::new();
    let mut watchers: Vec<odrs_watch::Watcher> = Vec::new();
    // Online change notifications, one listener per drive.
    let mut listeners: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let (remote_tx, mut remote_rx) = mpsc::unbounded_channel();
    let mut watched_version = None;
    let (local_tx, local_rx) = mpsc::unbounded_channel();
    let mut local_rx = (mode == SyncMode::Bidirectional).then_some(local_rx);

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut paused = false;
    // Next full pass (every drive), and an earlier one for drives notified of online changes.
    let mut next = Instant::now();
    let mut remote_next: Option<Instant> = None;
    let mut notified: HashSet<String> = HashSet::new();
    loop {
        // Apply drive list changes (startup, AddDrive, RemoveDrive).
        let (targets, version, removed, resync) = {
            let mut d = drives.lock().unwrap();
            (d.targets.clone(), d.version, std::mem::take(&mut d.removed), std::mem::take(&mut d.resync))
        };
        for drive_id in removed {
            if let Some(root) = store.root(&drive_id)? {
                store.delete(&root.drive_id, &root.id)?;
                tracing::info!(drive_id, "drive removed from the database (local files kept)");
            }
        }
        for name in resync {
            reset_drive(&store, &targets, &name);
            // Shown as waiting for its first pass, like a new drive.
            if let (Some(s), Some(t)) = (statuses.iter_mut().find(|s| s.name == name), targets.iter().find(|t| t.name == name)) {
                *s = status_for(t, &[]);
            }
            server.set_drives(statuses.clone()).await?;
        }
        if watched_version != Some(version) {
            for t in &targets {
                tracing::info!(drive = %t.name, sync_dir = %t.opts.sync_dir.display(), "sync target");
            }
            if local_rx.is_some() {
                // Local changes only matter when they are uploaded. Any change triggers a
                // pass over all drives; a drive without changes costs one /delta call and a walk.
                watchers.clear();
                for t in &targets {
                    if let Err(e) = std::fs::create_dir_all(&t.opts.sync_dir) {
                        tracing::warn!(drive = %t.name, error = %e, "cannot create the sync folder");
                    }
                    let skip = t.opts.filters.skip_file.clone();
                    let relevant: odrs_watch::Filter =
                        Box::new(move |p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !skip.matches(n)));
                    match odrs_watch::Watcher::new(&t.opts.sync_dir, relevant, local_tx.clone()) {
                        Ok(w) => watchers.push(w),
                        Err(e) => tracing::error!(drive = %t.name, error = %e, "cannot watch the sync folder"),
                    }
                }
            }
            if realtime {
                for l in listeners.drain(..) {
                    l.abort();
                }
                listeners = targets.iter().map(|t| spawn_listener(graph.clone(), t, remote_tx.clone())).collect();
            }
            statuses = targets.iter().map(|t| status_for(t, &statuses)).collect();
            server.set_drives(statuses.clone()).await?;
            watched_version = Some(version);
        }

        // Wait for the next pass, a command or a shutdown signal.
        let wake = tokio::time::sleep_until(remote_next.map_or(next, |r| r.min(next)).into());
        tokio::select! {
            _ = wake, if !paused => {}
            Some(path) = local_change(&mut local_rx) => {
                tracing::debug!(path = %path.display(), "local change");
                next = next.min(Instant::now() + DEBOUNCE);
                continue;
            }
            Some((drive, signal)) = remote_rx.recv() => {
                match signal {
                    odrs_graph::realtime::Signal::Changed => {
                        tracing::info!(drive, "online change notified");
                        let at = Instant::now() + REMOTE_DEBOUNCE;
                        remote_next = Some(remote_next.map_or(at, |r| r.min(at)));
                        notified.insert(drive);
                    }
                    odrs_graph::realtime::Signal::Connected => tracing::info!(drive, "online changes now notified in near real time"),
                    odrs_graph::realtime::Signal::Disconnected => tracing::debug!(drive, "online change notifications interrupted"),
                }
                continue;
            }
            cmd = rx.recv() => {
                match cmd {
                    Some(Command::SyncNow) if !paused => next = Instant::now(),
                    Some(Command::Pause) => {
                        paused = true;
                        server.set_state(State::Paused).await?;
                    }
                    Some(Command::Resume) if paused => {
                        paused = false;
                        next = Instant::now();
                        server.set_state(State::Idle).await?;
                    }
                    Some(_) => {}
                    None => break,
                }
                continue;
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
        }

        // A pass brought forward by notifications only covers the notified drives.
        let full = Instant::now() >= next;
        let only = if full {
            notified.clear();
            None
        } else {
            Some(std::mem::take(&mut notified))
        };
        remote_next = None;

        // A folder selection may have changed while waiting.
        let mut targets = targets;
        let delete_excluded = {
            let d = drives.lock().unwrap();
            for t in &mut targets {
                if let Some(current) = d.targets.iter().find(|c| c.name == t.name) {
                    t.opts.filters = current.opts.filters.clone();
                }
                t.opts.delete_excluded = d.delete_excluded.contains(&t.name);
            }
            d.delete_excluded.clone()
        };

        server.set_state(State::Syncing).await?;
        let shared = Arc::new(Mutex::new(statuses.clone()));
        let started = Instant::now();
        let mut sync_requested = false;
        let outcome = tokio::select! {
            r = run_pass(&graph, &store, selected(&targets, only.as_ref()), mode, &server, &shared, &event_ids) => Some(r),
            () = until_pause(&mut rx, &mut sync_requested) => None,
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
        };
        statuses = shared.lock().unwrap().clone();

        let Some(outcome) = outcome else {
            tracing::info!("pass cancelled, paused");
            for s in statuses.iter_mut().filter(|s| s.state == "syncing") {
                s.state = "pending".into();
            }
            server.set_drives(statuses.clone()).await?;
            paused = true;
            server.set_state(State::Paused).await?;
            continue;
        };
        tracing::info!(elapsed = ?started.elapsed(), drives = only.as_ref().map_or(targets.len(), HashSet::len), "pass finished");
        // Local copies deleted along with a folder selection: done once the drive's pass went through.
        let done: Vec<&String> = delete_excluded
            .iter()
            .filter(|n| only.as_ref().is_none_or(|o| o.contains(*n)) && !outcome.failures.iter().any(|(f, _)| f == *n))
            .collect();
        if !done.is_empty() {
            let mut d = drives.lock().unwrap();
            for name in done {
                d.delete_excluded.remove(name);
            }
        }
        server.set_progress(outcome.total).await?;
        if outcome.failures.is_empty() && outcome.total.errors == 0 && only.is_some() && statuses.iter().any(|s| s.state == "error") {
            // Another drive, not part of this pass, is still in error: keep showing it.
        } else if outcome.failures.is_empty() && outcome.total.errors == 0 {
            server.finish_pass(Ok(now_rfc3339())).await?;
            server.set_state(State::Idle).await?;
        } else if outcome.failures.is_empty() {
            let n = outcome.total.errors;
            server.finish_pass(Err(format!("{n} item(s) failed, see the daemon log"))).await?;
            server.set_state(State::Error).await?;
        } else {
            let mut msg = outcome.failures.iter().map(|(name, e)| format!("{name}: {e}")).collect::<Vec<_>>().join("; ");
            if outcome.failures.iter().any(|(_, e)| e.is_auth()) {
                // Recognised by the GUI, which then offers to sign in again.
                msg = format!("{AUTH_ERROR_PREFIX} {msg}");
            } else if outcome.failures.iter().all(|(_, e)| e.is_network()) {
                msg = format!("{OFFLINE_ERROR_PREFIX} {msg}");
            }
            server.finish_pass(Err(msg)).await?;
            server.set_state(State::Error).await?;
            if outcome.failures.iter().any(|(_, e)| e.is_network()) {
                next = if full { Instant::now() + OFFLINE_RETRY } else { next.min(Instant::now() + OFFLINE_RETRY) };
                continue;
            }
        }
        if full {
            next = Instant::now() + interval;
        }
        if outcome.deferred > 0 {
            // Files that were still being written: pick them up shortly.
            next = next.min(Instant::now() + 2 * DEBOUNCE);
        }
        if sync_requested || drives.lock().unwrap().version != version {
            // Asked during the pass (e.g. a drive was added): do not wait for the interval.
            next = Instant::now();
        }
    }
    tracing::info!("daemon stopping");
    verifier.shutdown();
    let _ = verifier_task.await;
    Ok(())
}

/// Resync: drop the database rows of a drive (sync cursor included); the next
/// pass enumerates it again and matches the local files.
pub(crate) fn reset_drive(store: &Store, targets: &[Target], name: &str) {
    let Some(t) = targets.iter().find(|t| t.name == name) else { return };
    let libraries: Vec<String> = targets.iter().filter_map(|t| t.opts.drive_id.clone()).collect();
    match crate::verify::drive_id_of(store, t, &libraries) {
        Ok(Some(id)) => match store.delete_drive(&id) {
            Ok(n) => tracing::info!(drive = name, rows = n, "resync: database rows dropped, the next pass rebuilds them"),
            Err(e) => tracing::error!(drive = name, error = %e, "resync failed"),
        },
        Ok(None) => tracing::info!(drive = name, "resync: nothing recorded yet"),
        Err(e) => tracing::error!(drive = name, error = %e, "resync failed"),
    }
}

/// Forward a drive's online change notifications, tagged with its name. The
/// main drive's id is not in the config: ask Graph (retried while offline).
fn spawn_listener(
    graph: Arc<Graph>,
    t: &Target,
    out: mpsc::UnboundedSender<(String, odrs_graph::realtime::Signal)>,
) -> tokio::task::JoinHandle<()> {
    let (name, drive_id) = (t.name.clone(), t.opts.drive_id.clone());
    tokio::spawn(async move {
        let drive_id = match drive_id {
            Some(id) => id,
            None => loop {
                match graph.me_drive().await {
                    Ok(d) => break d.id,
                    Err(e) => {
                        tracing::debug!(error = %e, "main drive id unknown; realtime notifications retried later");
                        tokio::time::sleep(DRIVE_ID_RETRY).await;
                    }
                }
            },
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let listener = odrs_graph::realtime::listen(graph, drive_id, tx);
        tokio::pin!(listener);
        loop {
            tokio::select! {
                () = &mut listener => return,
                Some(signal) = rx.recv() => {
                    if out.send((name.clone(), signal)).is_err() {
                        return;
                    }
                }
            }
        }
    })
}

/// Keep what is known about a drive across drive list changes.
fn status_for(t: &Target, previous: &[DriveStatus]) -> DriveStatus {
    match previous.iter().find(|s| s.name == t.name) {
        Some(s) => DriveStatus { sync_dir: t.opts.sync_dir.display().to_string(), ..s.clone() },
        None => DriveStatus {
            name: t.name.clone(),
            drive_id: t.opts.drive_id.clone().unwrap_or_default(),
            sync_dir: t.opts.sync_dir.display().to_string(),
            state: "pending".into(),
            ..Default::default()
        },
    }
}

struct PassOutcome {
    failures: Vec<(String, odrs_engine::Error)>,
    total: Progress,
    deferred: u64,
}

/// The drives of a pass with their index in the statuses: all, or `only` those.
fn selected<'a>(targets: &'a [Target], only: Option<&HashSet<String>>) -> Vec<(usize, &'a Target)> {
    targets.iter().enumerate().filter(|(_, t)| only.is_none_or(|o| o.contains(&t.name))).collect()
}

/// One pass over every drive, in order; a failing drive does not stop the
/// others. `statuses` is updated live and published with the progress.
async fn run_pass(
    graph: &Arc<Graph>,
    store: &Store,
    targets: Vec<(usize, &Target)>,
    mode: SyncMode,
    server: &Server,
    statuses: &Arc<Mutex<Vec<DriveStatus>>>,
    event_ids: &AtomicU64,
) -> PassOutcome {
    let mut out = PassOutcome { failures: Vec::new(), total: Progress::default(), deferred: 0 };
    for (i, t) in targets {
        let stats = Arc::new(Stats::default());
        let started = Instant::now();
        set_status(statuses, i, |s| {
            s.state = "syncing".into();
            s.progress = Progress::default();
        });
        publish(server, statuses).await;
        let ticker = tokio::spawn({
            let (server, stats, statuses) = (server.clone(), stats.clone(), statuses.clone());
            async move {
                let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
                loop {
                    tick.tick().await;
                    let p = progress(&stats);
                    set_status(&statuses, i, |s| s.progress = p);
                    let _ = server.set_progress(p).await;
                    publish(&server, &statuses).await;
                }
            }
        });
        let result = odrs_engine::sync(graph.clone(), store, t.opts.clone(), stats.clone(), mode).await;
        ticker.abort();
        let p = progress(&stats);
        crate::print_stats(&stats, started, &t.name);
        out.deferred += stats.deferred.load(Ordering::Relaxed);
        add(&mut out.total, &p);
        set_status(statuses, i, |s| {
            s.progress = p;
            match &result {
                Ok(()) if p.errors == 0 => {
                    s.state = "idle".into();
                    s.last_sync = now_rfc3339();
                    s.last_error.clear();
                }
                Ok(()) => {
                    s.state = "error".into();
                    s.last_error = format!("{} item(s) failed, see the daemon log", p.errors);
                }
                Err(e) => {
                    s.state = "error".into();
                    s.last_error = if e.is_auth() {
                        format!("{AUTH_ERROR_PREFIX} {e}")
                    } else if e.is_network() {
                        format!("{OFFLINE_ERROR_PREFIX} {e}")
                    } else {
                        e.to_string()
                    };
                }
            }
        });
        publish(server, statuses).await;
        let events = drive_events(&t.name, &stats, result.is_ok() && p.errors == 0, event_ids);
        if let Err(e) = server.push_events(events).await {
            tracing::debug!(error = %e, "cannot publish events");
        }
        if let Err(e) = result {
            tracing::error!(drive = %t.name, error = %e, "sync pass failed");
            out.failures.push((t.name.clone(), e));
        }
    }
    out
}

/// What clients may want to notify about a drive's pass: each safeBackup made,
/// and the end of a sync that enumerated the whole drive.
fn drive_events(drive: &str, stats: &Stats, succeeded: bool, ids: &AtomicU64) -> Vec<Event> {
    let time = now_rfc3339();
    let event = |kind: &str, path: String, detail: String| Event {
        id: ids.fetch_add(1, Ordering::Relaxed),
        time: time.clone(),
        kind: kind.into(),
        drive: drive.into(),
        path,
        detail,
    };
    let mut out: Vec<Event> = std::mem::take(&mut *stats.backups.lock().unwrap())
        .into_iter()
        .map(|b| event("conflict", b.path.display().to_string(), b.backup.display().to_string()))
        .collect();
    if succeeded && stats.full_enumeration.load(Ordering::Relaxed) {
        out.push(event("initial-sync", String::new(), String::new()));
    }
    out
}

async fn publish(server: &Server, statuses: &Mutex<Vec<DriveStatus>>) {
    let snapshot = statuses.lock().unwrap().clone();
    if let Err(e) = server.set_drives(snapshot).await {
        tracing::debug!(error = %e, "cannot publish drive statuses");
    }
}

fn set_status(statuses: &Mutex<Vec<DriveStatus>>, i: usize, f: impl FnOnce(&mut DriveStatus)) {
    if let Some(s) = statuses.lock().unwrap().get_mut(i) {
        f(s);
    }
}

fn add(total: &mut Progress, p: &Progress) {
    total.items += p.items;
    total.downloaded += p.downloaded;
    total.bytes += p.bytes;
    total.unchanged += p.unchanged;
    total.deleted += p.deleted;
    total.skipped += p.skipped;
    total.errors += p.errors;
    total.uploaded += p.uploaded;
    total.deleted_online += p.deleted_online;
    total.drive_bytes += p.drive_bytes;
    total.synced_bytes += p.synced_bytes;
}

/// Next local change, or never when not watching.
async fn local_change(rx: &mut Option<mpsc::UnboundedReceiver<PathBuf>>) -> Option<PathBuf> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Resolves when a `Pause` command arrives. A `SyncNow` during the pass is
/// remembered so that another pass follows right away.
async fn until_pause(rx: &mut mpsc::UnboundedReceiver<Command>, sync_requested: &mut bool) {
    loop {
        match rx.recv().await {
            Some(Command::Pause) => return,
            Some(Command::SyncNow) => *sync_requested = true,
            Some(_) => {}
            None => std::future::pending().await,
        }
    }
}

fn progress(s: &Stats) -> Progress {
    let g = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    Progress {
        items: g(&s.items),
        downloaded: g(&s.downloaded),
        bytes: g(&s.bytes),
        unchanged: g(&s.unchanged),
        deleted: g(&s.deleted),
        skipped: g(&s.skipped),
        uploaded: g(&s.uploaded),
        deleted_online: g(&s.deleted_online),
        errors: g(&s.errors),
        drive_bytes: g(&s.drive_bytes),
        synced_bytes: (g(&s.synced_bytes_at_start) + g(&s.recorded_bytes) + g(&s.bytes)).min(g(&s.drive_bytes)),
        upload_files: g(&s.upload_queued),
        upload_bytes: g(&s.upload_queued_bytes),
        upload_sent: g(&s.upload_sent_bytes).min(g(&s.upload_queued_bytes)),
    }
}

pub(crate) fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    odrs_engine::convert::format_db_time(time::OffsetDateTime::from_unix_timestamp(secs).unwrap_or(time::OffsetDateTime::UNIX_EPOCH))
}

/// D-Bus drive management: Graph lookups answer directly; drive list changes
/// are written to the config file and picked up by the loop before its next pass.
struct Manager {
    graph: Arc<Graph>,
    config_path: PathBuf,
    /// Global options (filters, permissions...) applied to new drives.
    cfg: Config,
    drives: Arc<Mutex<Drives>>,
    verifier: Verifier,
}

impl Manager {
    fn target(&self, name: &str) -> std::result::Result<Target, String> {
        let drives = self.drives.lock().unwrap();
        drives.targets.iter().find(|t| t.name == name).cloned().ok_or_else(|| format!("no drive named \"{name}\""))
    }

    /// The drive's current selective sync file, empty if none.
    fn sync_list_text(t: &Target) -> std::result::Result<String, String> {
        let Some(file) = &t.opts.sync_list_file else { return Ok(String::new()) };
        match std::fs::read_to_string(file) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(format!("{}: {e}", file.display())),
        }
    }
}

impl DriveManager for Manager {
    fn search_sites(&self, query: String) -> BoxFuture<'_, std::result::Result<Vec<Site>, String>> {
        Box::pin(async move {
            let query = if query.trim().is_empty() { "*".to_owned() } else { query.trim().to_owned() };
            let sites = self.graph.search_sites(&query).await.map_err(|e| e.to_string())?;
            let mut out: Vec<Site> = sites
                .into_iter()
                .map(|s| Site {
                    name: s.display_name.or(s.name).unwrap_or_else(|| s.id.clone()),
                    web_url: s.web_url.unwrap_or_default(),
                    id: s.id,
                })
                .collect();
            out.sort_by_key(|s| s.name.to_lowercase());
            Ok(out)
        })
    }

    fn list_libraries(&self, site_id: String) -> BoxFuture<'_, std::result::Result<Vec<Library>, String>> {
        Box::pin(async move {
            let drives = self.graph.site_drives(&site_id).await.map_err(|e| e.to_string())?;
            Ok(drives
                .into_iter()
                .filter(|d| d.drive_type.as_deref() == Some("documentLibrary"))
                .map(|d| Library { name: d.name.unwrap_or_else(|| d.id.clone()), web_url: d.web_url.unwrap_or_default(), drive_id: d.id })
                .collect())
        })
    }

    fn add_drive(&self, name: String, drive_id: String, sync_dir: String) -> std::result::Result<(), String> {
        let name = name.trim().to_owned();
        let sync_dir = config::expand_home(sync_dir.trim());
        let mut drives = self.drives.lock().unwrap();
        let current = configured(&drives.targets);
        let main_dir = drives.targets.first().map(|t| t.opts.sync_dir.clone()).unwrap_or_default();
        config::validate_new_drive(&current, &main_dir, &name, &drive_id, &sync_dir).map_err(|e| e.to_string())?;
        let d = DriveConfig { name: name.clone(), drive_id: drive_id.clone(), sync_dir: sync_dir.clone(), ..Default::default() };
        config::append_drive_section(&self.config_path, &d).map_err(|e| e.to_string())?;
        let mut opts = self.cfg.sync_options(sync_dir, Some(&d));
        opts.drive_id = Some(drive_id);
        if let Some(main) = drives.targets.first() {
            opts.max_transfers = main.opts.max_transfers;
            opts.upload_sessions = main.opts.upload_sessions.clone();
            opts.filter_state = main.opts.filter_state.clone();
        }
        if let Some(dir) = self.config_path.parent() {
            opts.sync_list_file = Some(config::sync_list_path(dir, &name));
        }
        drives.targets.push(Target { name: name.clone(), opts });
        drives.version += 1;
        tracing::info!(drive = %name, "drive added");
        Ok(())
    }

    fn remove_drive(&self, name: String) -> std::result::Result<(), String> {
        if name.eq_ignore_ascii_case("main") {
            return Err("the account's own OneDrive cannot be removed".into());
        }
        let mut drives = self.drives.lock().unwrap();
        let Some(i) = drives.targets.iter().position(|t| t.name == name) else {
            return Err(format!("no drive named \"{name}\""));
        };
        config::remove_drive_section(&self.config_path, &name).map_err(|e| e.to_string())?;
        let removed = drives.targets.remove(i);
        if let Some(id) = removed.opts.drive_id {
            drives.removed.push(id);
        }
        drives.version += 1;
        drop(drives);
        self.verifier.forget(&name);
        tracing::info!(drive = %name, "drive removed (local files kept)");
        Ok(())
    }

    fn resync(&self, name: String) -> std::result::Result<(), String> {
        let mut drives = self.drives.lock().unwrap();
        let Some(t) = drives.targets.iter().find(|t| t.name.eq_ignore_ascii_case(&name)) else {
            return Err(format!("no drive named \"{name}\""));
        };
        let name = t.name.clone();
        if !drives.resync.contains(&name) {
            drives.resync.push(name.clone());
        }
        drop(drives);
        // Its last check describes a database that is about to go.
        self.verifier.forget(&name);
        tracing::info!(drive = %name, "resync requested");
        Ok(())
    }

    fn verify(&self, name: String) -> std::result::Result<(), String> {
        self.verifier.request(&name)
    }

    fn cancel_verify(&self, name: String) -> std::result::Result<(), String> {
        self.verifier.cancel(&name)
    }

    fn verify_report(&self, name: String) -> std::result::Result<Vec<VerifyFinding>, String> {
        self.verifier.report(&name)
    }

    fn list_folders(&self, name: String, path: String) -> BoxFuture<'_, std::result::Result<Vec<Folder>, String>> {
        Box::pin(async move {
            let t = self.target(&name)?;
            let list = odrs_engine::filter::SyncList::parse(&Self::sync_list_text(&t)?).0;
            // Folders the other filters exclude cannot be chosen.
            let skip = odrs_engine::filter::Filters { sync_list: None, ..t.opts.filters.clone() };
            let children = self.graph.child_folders(t.opts.drive_id.as_deref(), &path).await.map_err(|e| e.to_string())?;
            let mut out: Vec<Folder> = children
                .into_iter()
                .filter_map(|c| {
                    let rel = if path.is_empty() { c.name.clone() } else { format!("{path}/{}", c.name) };
                    if skip.excludes(&c.name, &rel, true) {
                        return None;
                    }
                    let state = odrs_engine::filter::SyncList::folder_state(list.as_ref(), &rel);
                    let has_children = c.folder.and_then(|f| f.child_count).is_some_and(|n| n > 0);
                    Some(Folder { name: c.name, path: rel, included: state.included, mixed: state.mixed, has_children })
                })
                .collect();
            out.sort_by_key(|f| f.name.to_lowercase());
            Ok(out)
        })
    }

    fn custom_rules(&self, name: String) -> std::result::Result<Vec<String>, String> {
        let t = self.target(&name)?;
        Ok(odrs_engine::filter::custom_rules(&Self::sync_list_text(&t)?))
    }

    fn set_selection(&self, name: String, choices: Vec<FolderChoice>, delete_local: bool) -> std::result::Result<(), String> {
        let t = self.target(&name)?;
        let file = t.opts.sync_list_file.clone().ok_or("this drive has no selective sync file")?;
        let choices: Vec<odrs_engine::filter::FolderChoice> = choices
            .into_iter()
            .map(|c| odrs_engine::filter::FolderChoice { path: c.path, included: c.included, mixed: c.mixed, expanded: c.expanded })
            .collect();
        let rules = odrs_engine::filter::selection_rules(&Self::sync_list_text(&t)?, &choices).map_err(|e| e.to_string())?;
        match &rules {
            Some(text) => {
                // Like the client for Windows: files at the top of the drive stay synced.
                if !t.opts.filters.sync_root_files {
                    let section = (name != "main").then_some(name.as_str());
                    config::set_value(&self.config_path, section, "sync_root_files", "true").map_err(|e| e.to_string())?;
                }
                let tmp = file.with_extension("tmp");
                std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &file)).map_err(|e| format!("{}: {e}", file.display()))?;
            }
            None => match std::fs::remove_file(&file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("{}: {e}", file.display())),
                _ => {}
            },
        }
        let mut drives = self.drives.lock().unwrap();
        if let Some(t) = drives.targets.iter_mut().find(|t| t.name == name) {
            t.opts.filters.sync_root_files |= rules.is_some();
        }
        if delete_local {
            drives.delete_excluded.insert(name.clone());
        } else {
            drives.delete_excluded.remove(&name);
        }
        tracing::info!(drive = %name, selective = rules.is_some(), delete_local, "folder selection changed");
        Ok(())
    }
}

/// The configured libraries, for validation.
fn configured(targets: &[Target]) -> Config {
    Config {
        drives: targets
            .iter()
            .filter_map(|t| {
                let (name, drive_id, sync_dir) = (t.name.clone(), t.opts.drive_id.clone()?, t.opts.sync_dir.clone());
                Some(DriveConfig { name, drive_id, sync_dir, ..Default::default() })
            })
            .collect(),
        ..Default::default()
    }
}
