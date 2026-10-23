//! `onedrive-rs daemon`: periodic sync passes, controlled over D-Bus.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use odrs_engine::{Stats, SyncMode};
use odrs_graph::{AuthConfig, Graph};
use odrs_ipc::{Command, Info, Progress, Server, State};
use odrs_store::Store;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::{Paths, Target};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(2);
/// Retry delay after a pass failed for lack of network.
const OFFLINE_RETRY: Duration = Duration::from_secs(60);
/// Prefix of `LastError` when the login expired (see `odrs_ipc::AUTH_ERROR_PREFIX`).
use odrs_ipc::AUTH_ERROR_PREFIX;
/// Quiet time after a local change before starting a pass.
const DEBOUNCE: Duration = Duration::from_secs(5);

pub async fn run(paths: &Paths, auth: AuthConfig, targets: Vec<Target>, mode: SyncMode, interval: Duration) -> Result<()> {
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
    let (tx, mut rx) = mpsc::unbounded_channel();
    let info = Info { account, sync_dir: sync_dir.display().to_string() };
    let server = Server::start(info, tx).await.context("cannot claim io.github.onedrive_rs.OneDrive1 on the session bus (daemon already running?)")?;
    for t in &targets {
        tracing::info!(drive = %t.name, sync_dir = %t.opts.sync_dir.display(), "sync target");
    }
    tracing::info!(interval = ?interval, ?mode, "daemon started");

    // Local changes only matter when they are uploaded. Any change triggers a
    // pass over all targets; a target without changes costs one /delta call
    // and a local walk.
    let mut watchers = Vec::new();
    let mut local_rx = None;
    if mode == SyncMode::Bidirectional {
        let (tx, rx) = mpsc::unbounded_channel();
        for t in &targets {
            let skip = t.opts.filters.skip_file.clone();
            let relevant: odrs_watch::Filter = Box::new(move |p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !skip.matches(n))
            });
            let w = odrs_watch::Watcher::new(&t.opts.sync_dir, relevant, tx.clone())
                .with_context(|| format!("cannot watch {}", t.opts.sync_dir.display()))?;
            watchers.push(w);
        }
        local_rx = Some(rx);
    }

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut paused = false;
    let mut next = Instant::now();
    loop {
        // Wait for the next pass, a command or a shutdown signal.
        let wake = tokio::time::sleep_until(next.into());
        tokio::select! {
            _ = wake, if !paused => {}
            Some(path) = local_change(&mut local_rx) => {
                tracing::debug!(path = %path.display(), "local change");
                next = next.min(Instant::now() + DEBOUNCE);
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

        server.set_state(State::Syncing).await?;
        let stats = Arc::new(Stats::default());
        let ticker = tokio::spawn({
            let (server, stats) = (server.clone(), stats.clone());
            async move {
                let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
                loop {
                    tick.tick().await;
                    let _ = server.set_progress(progress(&stats)).await;
                }
            }
        });
        let started = Instant::now();
        let outcome = tokio::select! {
            r = run_pass(&graph, &store, &targets, &stats, mode) => Some(r),
            () = until_pause(&mut rx) => None,
            _ = tokio::signal::ctrl_c() => { ticker.abort(); break }
            _ = sigterm.recv() => { ticker.abort(); break }
        };
        ticker.abort();
        server.set_progress(progress(&stats)).await?;
        crate::print_stats(&stats, started, "pass");

        match outcome {
            None => {
                tracing::info!("pass cancelled, paused");
                paused = true;
                server.set_state(State::Paused).await?;
            }
            Some(failures) if failures.is_empty() && stats.errors.load(Ordering::Relaxed) == 0 => {
                server.finish_pass(Ok(now_rfc3339())).await?;
                server.set_state(State::Idle).await?;
            }
            Some(failures) if failures.is_empty() => {
                let n = stats.errors.load(Ordering::Relaxed);
                server.finish_pass(Err(format!("{n} item(s) failed, see the daemon log"))).await?;
                server.set_state(State::Error).await?;
            }
            Some(failures) => {
                let mut msg = failures.iter().map(|(name, e)| format!("{name}: {e}")).collect::<Vec<_>>().join("; ");
                if failures.iter().any(|(_, e)| e.is_auth()) {
                    // Recognised by the GUI, which then offers to sign in again.
                    msg = format!("{AUTH_ERROR_PREFIX} {msg}");
                }
                server.finish_pass(Err(msg)).await?;
                server.set_state(State::Error).await?;
                if failures.iter().any(|(_, e)| e.is_network()) {
                    next = Instant::now() + OFFLINE_RETRY;
                    continue;
                }
            }
        }
        next = Instant::now() + interval;
        if stats.deferred.load(Ordering::Relaxed) > 0 {
            // Files that were still being written: pick them up shortly.
            next = next.min(Instant::now() + 2 * DEBOUNCE);
        }
    }
    tracing::info!("daemon stopping");
    Ok(())
}

/// One pass over every target, in order; a failing target does not stop the others.
async fn run_pass(graph: &Arc<Graph>, store: &Store, targets: &[Target], stats: &Arc<Stats>, mode: SyncMode) -> Vec<(String, odrs_engine::Error)> {
    let mut failures = Vec::new();
    for t in targets {
        if let Err(e) = odrs_engine::sync(graph.clone(), store, t.opts.clone(), stats.clone(), mode).await {
            tracing::error!(drive = %t.name, error = %e, "sync pass failed");
            failures.push((t.name.clone(), e));
        }
    }
    failures
}

/// Next local change, or never when not watching.
async fn local_change(rx: &mut Option<mpsc::UnboundedReceiver<PathBuf>>) -> Option<PathBuf> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Resolves when a `Pause` command arrives; other commands are ignored while a pass runs.
async fn until_pause(rx: &mut mpsc::UnboundedReceiver<Command>) {
    loop {
        match rx.recv().await {
            Some(Command::Pause) => return,
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
    }
}

fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    odrs_engine::convert::format_db_time(time::OffsetDateTime::from_unix_timestamp(secs).unwrap_or(time::OffsetDateTime::UNIX_EPOCH))
}
