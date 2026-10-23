//! Desktop UI: a status window and a tray icon (StatusNotifierItem), both
//! driven over D-Bus by the daemon. The UI never performs sync I/O itself.

use std::time::Duration;

use odrs_ipc::{Client, Snapshot};
use slint::ComponentHandle as _;
use tokio::sync::mpsc;

slint::include_modules!();

pub const APP_ID: &str = "io.github.onedrive_rs";
const REFRESH: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("UI error: {0}")]
    Platform(#[from] slint::PlatformError),
}

pub struct Options {
    /// Start with only the tray icon (e.g. from session autostart).
    pub hidden: bool,
    /// Launch the background service when it is not running.
    pub start_daemon: Box<dyn Fn() -> std::io::Result<()>>,
    /// Start an interactive (browser) login; the UI waits for it, then asks for a sync.
    pub login: Box<dyn Fn() -> std::io::Result<std::process::Child>>,
}

#[derive(Debug, Clone, Copy)]
enum UiCommand {
    SyncNow,
    TogglePause,
    Refresh,
}

/// Run the UI on the current thread until the user quits from the tray menu.
/// `rt` hosts the D-Bus client; it must not be the thread calling `run`.
pub fn run(rt: tokio::runtime::Handle, opts: Options) -> Result<(), Error> {
    let window = MainWindow::new()?;
    // Before the window is shown. Matches contrib/desktop/io.github.onedrive_rs.desktop,
    // from which Wayland compositors take the window icon.
    slint::set_xdg_app_id(APP_ID)?;
    let tray = Tray::new()?;
    let initial = to_status(None);
    window.set_status(initial.clone());
    tray.set_status(initial);

    let (tx, rx) = mpsc::unbounded_channel();
    let start_daemon = std::rc::Rc::new(opts.start_daemon);
    let login = std::rc::Rc::new(opts.login);

    // Globals are per component instance: wire the window and the tray alike.
    macro_rules! wire {
        ($component:expr) => {{
            let logic = $component.global::<Logic>();
            let t = tx.clone();
            logic.on_sync_now(move || {
                let _ = t.send(UiCommand::SyncNow);
            });
            let t = tx.clone();
            logic.on_toggle_pause(move || {
                let _ = t.send(UiCommand::TogglePause);
            });
            let w = window.as_weak();
            logic.on_open_folder(move || {
                if let Some(w) = w.upgrade() {
                    open_folder(&w.get_status().sync_dir);
                }
            });
            let (t, start) = (tx.clone(), start_daemon.clone());
            logic.on_start_daemon(move || {
                if let Err(e) = start() {
                    tracing::error!(error = %e, "could not start the daemon");
                }
                let _ = t.send(UiCommand::Refresh);
            });
            let (t, login) = (tx.clone(), login.clone());
            logic.on_reconnect(move || match login() {
                Ok(mut child) => {
                    let t = t.clone();
                    std::thread::spawn(move || {
                        if child.wait().is_ok_and(|s| s.success()) {
                            let _ = t.send(UiCommand::SyncNow);
                        }
                    });
                }
                Err(e) => tracing::error!(error = %e, "could not start the login"),
            });
            let w = window.as_weak();
            logic.on_show_window(move || {
                if let Some(w) = w.upgrade() {
                    let _ = w.show();
                }
            });
            logic.on_quit(|| {
                let _ = slint::quit_event_loop();
            });
        }};
    }
    wire!(window);
    wire!(tray);

    rt.spawn(poll(rx, window.as_weak(), tray.as_weak()));

    if !opts.hidden {
        window.show()?;
    }
    // Closing the window only hides it; the tray keeps the app alive.
    slint::run_event_loop_until_quit()?;
    Ok(())
}

/// Refresh the UI from the daemon every [`REFRESH`] and forward user commands.
async fn poll(mut rx: mpsc::UnboundedReceiver<UiCommand>, window: slint::Weak<MainWindow>, tray: slint::Weak<Tray>) {
    let client = loop {
        match Client::new().await {
            Ok(c) => break c,
            Err(e) => {
                tracing::warn!(error = %e, "session bus unavailable, retrying");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    };
    let mut last: Option<Snapshot> = None;
    let mut tick = tokio::time::interval(REFRESH);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { return };
                let result = match cmd {
                    UiCommand::SyncNow => client.daemon().sync_now().await,
                    UiCommand::TogglePause if last.as_ref().is_some_and(|s| s.state == "paused") => client.daemon().resume().await,
                    UiCommand::TogglePause => client.daemon().pause().await,
                    UiCommand::Refresh => Ok(()),
                };
                if let Err(e) = result {
                    tracing::warn!(error = %e, ?cmd, "daemon command failed");
                }
            }
        }
        let snapshot = match client.snapshot().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "cannot read daemon state");
                None
            }
        };
        let status = to_status(snapshot.as_ref());
        last = snapshot;
        let s = status.clone();
        let _ = window.upgrade_in_event_loop(move |w| {
            if w.get_status() != s {
                w.set_status(s);
            }
        });
        let _ = tray.upgrade_in_event_loop(move |t| {
            if t.get_status() != status {
                t.set_status(status);
            }
        });
    }
}

fn to_status(s: Option<&Snapshot>) -> Status {
    let Some(s) = s else {
        return Status { state: "stopped".into(), last_sync_mins: -1, ..Default::default() };
    };
    let p = &s.progress;
    let int = |v: u64| i32::try_from(v).unwrap_or(i32::MAX);
    Status {
        state: s.state.as_str().into(),
        account: s.account.as_str().into(),
        sync_dir: s.sync_dir.as_str().into(),
        last_error: s.last_error.trim_start_matches(odrs_ipc::AUTH_ERROR_PREFIX).trim().into(),
        needs_login: s.last_error.starts_with(odrs_ipc::AUTH_ERROR_PREFIX),
        last_sync_mins: minutes_since(&s.last_sync).unwrap_or(-1),
        items: int(p.items),
        downloaded: int(p.downloaded),
        gib: p.bytes as f32 / (1u64 << 30) as f32,
        unchanged: int(p.unchanged),
        uploaded: int(p.uploaded),
        skipped: int(p.skipped),
        errors: int(p.errors),
    }
}

fn minutes_since(rfc3339: &str) -> Option<i32> {
    let t = time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339).ok()?;
    let mins = (time::OffsetDateTime::now_utc() - t).whole_minutes().max(0);
    Some(i32::try_from(mins).unwrap_or(i32::MAX))
}

fn open_folder(path: &str) {
    if path.is_empty() {
        return;
    }
    if let Err(e) = std::process::Command::new("xdg-open").arg(path).spawn() {
        tracing::error!(error = %e, path, "xdg-open failed");
    }
}
