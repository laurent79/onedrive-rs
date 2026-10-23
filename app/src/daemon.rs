//! `onedrive-rs daemon`: periodic sync passes, controlled over D-Bus.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use odrs_engine::{Stats, SyncMode, SyncOptions};
use odrs_graph::{AuthConfig, Graph};
use odrs_ipc::{Command, Info, Progress, Server, State};
use odrs_store::Store;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::Paths;

const PROGRESS_INTERVAL: Duration = Duration::from_secs(2);
/// Quiet time after a local change before starting a pass.
const DEBOUNCE: Duration = Duration::from_secs(5);

pub async fn run(paths: &Paths, auth: AuthConfig, opts: SyncOptions, mode: SyncMode, interval: Duration) -> Result<()> {
    let _lock = crate::lock(paths)?;
    let graph = Arc::new(Graph::new(odrs_graph::http_client()?, auth, paths.token_store())?);
    let store = Store::open(&paths.db())?;
    let sync_dir = opts.sync_dir.clone();

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
    tracing::info!(interval = ?interval, ?mode, sync_dir = %sync_dir.display(), "daemon started");

    // Local changes only matter when they are uploaded.
    let (_watcher, mut local_rx) = if mode == SyncMode::Bidirectional {
        let skip = opts.skip_file.clone();
        let relevant: odrs_watch::Filter = Box::new(move |p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !skip.matches(n))
        });
        let (w, rx) = odrs_watch::Watcher::new(&sync_dir, relevant).context("cannot watch the sync folder")?;
        (Some(w), Some(rx))
    } else {
        (None, None)
    };

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
            r = odrs_engine::sync(graph.clone(), &store, opts.clone(), stats.clone(), mode) => Some(r),
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
            Some(Ok(())) if stats.errors.load(Ordering::Relaxed) == 0 => {
                server.finish_pass(Ok(now_rfc3339())).await?;
                server.set_state(State::Idle).await?;
            }
            Some(Ok(())) => {
                let n = stats.errors.load(Ordering::Relaxed);
                server.finish_pass(Err(format!("{n} item(s) failed, see the daemon log"))).await?;
                server.set_state(State::Error).await?;
            }
            Some(Err(e)) => {
                tracing::error!(error = %e, "sync pass failed");
                server.finish_pass(Err(e.to_string())).await?;
                server.set_state(State::Error).await?;
            }
        }
        next = Instant::now() + interval;
    }
    tracing::info!("daemon stopping");
    Ok(())
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
