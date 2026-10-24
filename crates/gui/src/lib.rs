//! Desktop UI: a status window listing the synchronised drives, a window to add
//! a SharePoint library, and a tray icon (StatusNotifierItem), all driven over
//! D-Bus by the daemon. The UI never performs sync I/O itself.

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use odrs_ipc::{Client, DriveStatus, Snapshot, VerifyFinding, VerifyStatus};
use slint::{ComponentHandle as _, Model as _, ModelRc, SharedString, StandardListViewItem, VecModel};
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

#[derive(Debug, Clone)]
enum UiCommand {
    SyncNow,
    TogglePause,
    Refresh,
    RemoveDrive(String),
    SearchSites(String),
    ListLibraries(String),
    AddDrive { name: String, drive_id: String, sync_dir: String },
    Verify(String),
    CancelVerify(String),
    /// Fetch the findings of a drive's last check and show them.
    ShowReport(String),
}

/// Run the UI on the current thread until the user quits from the tray menu.
/// `rt` hosts the D-Bus client; it must not be the thread calling `run`.
pub fn run(rt: tokio::runtime::Handle, opts: Options) -> Result<(), Error> {
    let window = MainWindow::new()?;
    // Before the window is shown. Matches contrib/desktop/io.github.onedrive_rs.desktop,
    // from which Wayland compositors take the window icon.
    slint::set_xdg_app_id(APP_ID)?;
    let tray = Tray::new()?;
    let add = AddLibraryWindow::new()?;
    let report = ReportWindow::new()?;
    let initial = to_status(None);
    window.set_status(initial.clone());
    tray.set_status(initial);
    // Updated in place, so rows (and their pending "confirm removal") survive refreshes.
    window.set_drives(ModelRc::from(Rc::new(VecModel::<DriveRow>::default())));

    let (tx, rx) = mpsc::unbounded_channel();
    let start_daemon = Rc::new(opts.start_daemon);
    let login = Rc::new(opts.login);

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
            logic.on_open_folder(|path| open_folder(&path));
            let t = tx.clone();
            logic.on_remove_drive(move |name| {
                let _ = t.send(UiCommand::RemoveDrive(name.into()));
            });
            let t = tx.clone();
            logic.on_verify(move |name| {
                let _ = t.send(UiCommand::Verify(name.into()));
            });
            let t = tx.clone();
            logic.on_cancel_verify(move |name| {
                let _ = t.send(UiCommand::CancelVerify(name.into()));
            });
            let t = tx.clone();
            logic.on_show_report(move |name| {
                let _ = t.send(UiCommand::ShowReport(name.into()));
            });
            let (a, t) = (add.as_weak(), tx.clone());
            logic.on_add_library(move || {
                if let Some(a) = a.upgrade() {
                    open_add_window(&a, &t);
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
    wire_add_window(&add, &tx);
    wire_report_window(&report, &tx, &rt);

    rt.spawn(poll(rx, window.as_weak(), tray.as_weak(), add.as_weak(), report.as_weak()));

    if !opts.hidden {
        window.show()?;
    }
    // Closing the window only hides it; the tray keeps the app alive.
    slint::run_event_loop_until_quit()?;
    Ok(())
}

fn open_add_window(a: &AddLibraryWindow, tx: &mpsc::UnboundedSender<UiCommand>) {
    a.set_query(SharedString::new());
    a.set_name(SharedString::new());
    a.set_folder(SharedString::new());
    a.set_site_index(-1);
    a.set_library_index(-1);
    a.set_libraries(ModelRc::default());
    a.set_message(SharedString::new());
    a.set_no_results(false);
    a.set_busy(true);
    let _ = a.show();
    // Start with every site the account can see.
    let _ = tx.send(UiCommand::SearchSites(String::new()));
}

fn wire_add_window(add: &AddLibraryWindow, tx: &mpsc::UnboundedSender<UiCommand>) {
    let (a, t) = (add.as_weak(), tx.clone());
    add.on_search(move |query| {
        if let Some(a) = a.upgrade() {
            a.set_busy(true);
            a.set_site_index(-1);
            a.set_library_index(-1);
            a.set_libraries(ModelRc::default());
        }
        let _ = t.send(UiCommand::SearchSites(query.into()));
    });
    let (a, t) = (add.as_weak(), tx.clone());
    add.on_site_selected(move |i| {
        let Some(a) = a.upgrade() else { return };
        let Some(id) = usize::try_from(i).ok().and_then(|i| a.get_site_ids().row_data(i)) else { return };
        a.set_library_index(-1);
        a.set_libraries(ModelRc::default());
        a.set_busy(true);
        let _ = t.send(UiCommand::ListLibraries(id.into()));
    });
    let a = add.as_weak();
    add.on_library_selected(move |i| {
        let Some(a) = a.upgrade() else { return };
        let (Ok(i), Ok(s)) = (usize::try_from(i), usize::try_from(a.get_site_index())) else { return };
        let library = a.get_libraries().row_data(i).map(|l| l.text).unwrap_or_default();
        let site = a.get_sites().row_data(s).map(|l| l.text).unwrap_or_default();
        let name = default_name(&site, &library);
        a.set_folder(default_folder(&name).into());
        a.set_name(name.into());
    });
    let (a, t) = (add.as_weak(), tx.clone());
    add.on_add(move |name, folder| {
        let Some(a) = a.upgrade() else { return };
        let Some(drive_id) = usize::try_from(a.get_library_index()).ok().and_then(|i| a.get_library_ids().row_data(i)) else {
            return;
        };
        a.set_busy(true);
        a.set_message(SharedString::new());
        let _ = t.send(UiCommand::AddDrive { name: name.into(), drive_id: drive_id.into(), sync_dir: folder.into() });
    });
    let a = add.as_weak();
    add.on_cancel(move || {
        if let Some(a) = a.upgrade() {
            let _ = a.hide();
        }
    });
}

fn wire_report_window(report: &ReportWindow, tx: &mpsc::UnboundedSender<UiCommand>, rt: &tokio::runtime::Handle) {
    let (r, rt) = (report.as_weak(), rt.clone());
    report.on_show_item(move |i| {
        let Some(r) = r.upgrade() else { return };
        if let Some(path) = usize::try_from(i).ok().and_then(|i| r.get_paths().row_data(i)) {
            rt.spawn(show_in_folder(path.into()));
        }
    });
    let (r, t) = (report.as_weak(), tx.clone());
    report.on_check_again(move || {
        let Some(r) = r.upgrade() else { return };
        let _ = t.send(UiCommand::Verify(r.get_drive_name().into()));
        let _ = r.hide();
    });
    let r = report.as_weak();
    report.on_dismiss(move || {
        if let Some(r) = r.upgrade() {
            let _ = r.hide();
        }
    });
}

/// Select the file in the file manager (Dolphin, Nautilus... through
/// `org.freedesktop.FileManager1`), or open its folder; a missing file's folder.
async fn show_in_folder(path: String) {
    let p = std::path::Path::new(&path);
    if p.exists() {
        let shown = async {
            let conn = zbus::Connection::session().await?;
            conn.call_method(
                Some("org.freedesktop.FileManager1"),
                "/org/freedesktop/FileManager1",
                Some("org.freedesktop.FileManager1"),
                "ShowItems",
                &(vec![file_uri(p)], ""),
            )
            .await?;
            zbus::Result::Ok(())
        };
        match shown.await {
            Ok(()) => return,
            Err(e) => tracing::debug!(error = %e, "FileManager1.ShowItems failed"),
        }
    }
    // The nearest folder that still exists.
    if let Some(dir) = p.ancestors().skip(1).find(|d| d.is_dir()) {
        open_folder(&dir.display().to_string());
    }
}

/// `file://` URI with the path percent-encoded (RFC 8089).
fn file_uri(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// "Documents" is the default library of every site: name it after the site.
fn default_name(site: &str, library: &str) -> String {
    let generic = ["documents", "shared documents", "documents partagés", "documents partages", "gedeelde documenten"];
    let name = if generic.contains(&library.to_lowercase().as_str()) || library.is_empty() { site } else { library };
    name.replace(['/', '\\', '"', '[', ']'], "-").trim().to_owned()
}

fn default_folder(name: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/SharePoint/{name}")
}

/// Refresh the UI from the daemon every [`REFRESH`] and forward user commands.
async fn poll(
    mut rx: mpsc::UnboundedReceiver<UiCommand>,
    window: slint::Weak<MainWindow>,
    tray: slint::Weak<Tray>,
    add: slint::Weak<AddLibraryWindow>,
    report: slint::Weak<ReportWindow>,
) {
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
    let mut rates = Rates::default();
    let mut tick = tokio::time::interval(REFRESH);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { return };
                handle(&client, cmd, last.as_ref(), &add, &report).await;
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
        let rows: Vec<DriveRow> = snapshot
            .as_ref()
            .map(|s| {
                s.drives
                    .iter()
                    .map(|d| to_row(d, rates.sample(d), s.verifications.iter().find(|v| v.name == d.name)))
                    .collect()
            })
            .unwrap_or_default();
        last = snapshot;
        let s = status.clone();
        let _ = window.upgrade_in_event_loop(move |w| {
            if w.get_status() != s {
                w.set_status(s);
            }
            if let Some(model) = w.get_drives().as_any().downcast_ref::<VecModel<DriveRow>>() {
                sync_rows(model, rows);
            }
        });
        let _ = tray.upgrade_in_event_loop(move |t| {
            if t.get_status() != status {
                t.set_status(status);
            }
        });
    }
}

async fn handle(
    client: &Client,
    cmd: UiCommand,
    last: Option<&Snapshot>,
    add: &slint::Weak<AddLibraryWindow>,
    report_window: &slint::Weak<ReportWindow>,
) {
    let d = client.daemon();
    match cmd {
        UiCommand::SyncNow => report(d.sync_now().await, "sync now"),
        UiCommand::TogglePause if last.is_some_and(|s| s.state == "paused") => report(d.resume().await, "resume"),
        UiCommand::TogglePause => report(d.pause().await, "pause"),
        UiCommand::Refresh => {}
        UiCommand::RemoveDrive(name) => report(d.remove_drive(&name).await, "remove drive"),
        UiCommand::Verify(name) => report(d.verify(&name).await, "verify"),
        UiCommand::CancelVerify(name) => report(d.cancel_verify(&name).await, "cancel verify"),
        UiCommand::ShowReport(name) => {
            let title = if name == "main" { "OneDrive".to_owned() } else { name.clone() };
            let findings = match d.verify_report(&name).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot read the verification report");
                    return;
                }
            };
            let _ = report_window.upgrade_in_event_loop(move |r| {
                r.set_drive_name(name.into());
                r.set_drive_title(title.into());
                r.set_current(-1);
                let labels: Vec<String> = findings.iter().map(|f| finding_label(&r, f)).collect();
                r.set_items(list(labels.iter().map(String::as_str)));
                r.set_paths(strings(findings.iter().map(|f| f.path.as_str())));
                let _ = r.show();
            });
        }
        UiCommand::SearchSites(query) => {
            let result = d.search_sites(&query).await;
            let _ = add.upgrade_in_event_loop(move |a| {
                a.set_busy(false);
                match result {
                    Ok(sites) => {
                        a.set_no_results(sites.is_empty());
                        a.set_message(SharedString::new());
                        a.set_message_is_error(false);
                        a.set_sites(list(sites.iter().map(|s| s.name.as_str())));
                        a.set_site_ids(strings(sites.iter().map(|s| s.id.as_str())));
                    }
                    Err(e) => show_error(&a, &e),
                }
            });
        }
        UiCommand::ListLibraries(site_id) => {
            let result = d.list_libraries(&site_id).await;
            let _ = add.upgrade_in_event_loop(move |a| {
                a.set_busy(false);
                match result {
                    Ok(libs) => {
                        a.set_libraries(list(libs.iter().map(|l| l.name.as_str())));
                        a.set_library_ids(strings(libs.iter().map(|l| l.drive_id.as_str())));
                        if libs.len() == 1 {
                            // Most sites only have "Documents": select it.
                            a.set_library_index(0);
                            a.invoke_library_selected(0);
                        }
                    }
                    Err(e) => show_error(&a, &e),
                }
            });
        }
        UiCommand::AddDrive { name, drive_id, sync_dir } => {
            let result = d.add_drive(&name, &drive_id, &sync_dir).await;
            let _ = add.upgrade_in_event_loop(move |a| {
                a.set_busy(false);
                match result {
                    Ok(()) => {
                        let _ = a.hide();
                    }
                    Err(e) => show_error(&a, &e),
                }
            });
        }
    }
}

fn finding_label(r: &ReportWindow, f: &VerifyFinding) -> String {
    let kind = r.global::<Fmt>().invoke_finding_kind(f.kind.as_str().into());
    let detail = if f.kind == "error" && !f.detail.is_empty() { format!(" ({})", f.detail) } else { String::new() };
    format!("{kind} — {}{detail}", f.path)
}

fn report(result: zbus::Result<()>, what: &str) {
    if let Err(e) = result {
        tracing::warn!(error = %e, what, "daemon command failed");
    }
}

fn show_error(a: &AddLibraryWindow, e: &zbus::Error) {
    // Daemon-side failures arrive as org.freedesktop.DBus.Error.Failed with a readable message.
    let msg = match e {
        zbus::Error::MethodError(_, Some(detail), _) => detail.clone(),
        other => other.to_string(),
    };
    a.set_no_results(false);
    a.set_message(msg.into());
    a.set_message_is_error(true);
}

fn list<'a>(items: impl Iterator<Item = &'a str>) -> ModelRc<StandardListViewItem> {
    let v: Vec<StandardListViewItem> = items.map(StandardListViewItem::from).collect();
    ModelRc::from(Rc::new(VecModel::from(v)))
}

fn strings<'a>(items: impl Iterator<Item = &'a str>) -> ModelRc<SharedString> {
    let v: Vec<SharedString> = items.map(SharedString::from).collect();
    ModelRc::from(Rc::new(VecModel::from(v)))
}

/// Update rows in place: unchanged rows keep their UI state.
fn sync_rows(model: &VecModel<DriveRow>, rows: Vec<DriveRow>) {
    for (i, row) in rows.iter().enumerate() {
        match model.row_data(i) {
            Some(current) if current == *row => {}
            Some(current) if current.name == row.name => model.set_row_data(i, row.clone()),
            Some(_) => {
                // Another drive now at this position (one was added or removed): rebuild.
                model.set_vec(rows);
                return;
            }
            None => model.push(row.clone()),
        }
    }
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
}

/// Download rate per drive, measured over a sliding window so that pauses
/// imposed by SharePoint throttling do not make the estimate jump around.
#[derive(Default)]
struct Rates {
    samples: HashMap<String, VecDeque<(Instant, u64)>>,
}

const RATE_WINDOW: Duration = Duration::from_secs(30);

impl Rates {
    /// Bytes per second for this drive, if measurable.
    fn sample(&mut self, d: &DriveStatus) -> Option<f64> {
        let now = Instant::now();
        let q = self.samples.entry(d.name.clone()).or_default();
        let bytes = d.progress.synced_bytes;
        if d.state != "syncing" || q.back().is_some_and(|&(_, b)| b > bytes) {
            // Not syncing, or a new pass started: start measuring again.
            q.clear();
        }
        if d.state != "syncing" {
            return None;
        }
        q.push_back((now, bytes));
        while q.front().is_some_and(|&(t, _)| now - t > RATE_WINDOW) {
            q.pop_front();
        }
        let &(t0, b0) = q.front()?;
        let dt = (now - t0).as_secs_f64();
        (dt >= 5.0).then(|| (bytes - b0) as f64 / dt)
    }
}

fn to_row(d: &DriveStatus, rate: Option<f64>, v: Option<&VerifyStatus>) -> DriveRow {
    let int = |v: u64| i32::try_from(v).unwrap_or(i32::MAX);
    let gb = |v: u64| (v as f64 / 1e9) as f32;
    let p = &d.progress;
    let remaining = p.drive_bytes.saturating_sub(p.synced_bytes);
    // Only worth a bar when a noticeable part of the drive is still missing.
    let show_progress = d.state == "syncing" && p.drive_bytes > 0 && remaining > p.drive_bytes / 1000;
    let eta = match rate {
        Some(r) if r > 1.0 => i32::try_from((remaining as f64 / r / 60.0) as i64).unwrap_or(i32::MAX),
        _ => -1,
    };
    DriveRow {
        show_progress,
        fraction: if p.drive_bytes > 0 { (p.synced_bytes as f64 / p.drive_bytes as f64) as f32 } else { 0.0 },
        synced_gb: gb(p.synced_bytes),
        total_gb: gb(p.drive_bytes),
        rate_mbs: rate.map_or(-1.0, |r| (r / 1e6) as f32),
        eta_mins: eta,
        title: if d.name == "main" { "OneDrive".into() } else { d.name.as_str().into() },
        removable: d.name != "main",
        name: d.name.as_str().into(),
        sync_dir: d.sync_dir.as_str().into(),
        state: d.state.as_str().into(),
        last_error: d.last_error.trim_start_matches(odrs_ipc::AUTH_ERROR_PREFIX).trim().into(),
        last_sync_mins: minutes_since(&d.last_sync).unwrap_or(-1),
        items: int(d.progress.items),
        downloaded: int(d.progress.downloaded),
        uploaded: int(d.progress.uploaded),
        verify_state: v.map_or("", |v| v.state.as_str()).into(),
        verify_done: v.map_or(0, |v| int(v.done)),
        verify_total: v.map_or(0, |v| int(v.total)),
        verify_fraction: v.filter(|v| v.total > 0).map_or(0.0, |v| (v.done as f64 / v.total as f64) as f32),
        verify_problems: v.map_or(0, |v| int(v.problems())),
        verify_mins: v.and_then(|v| minutes_since(&v.finished)).unwrap_or(-1),
        verify_message: v.map_or("", |v| v.message.as_str()).into(),
    }
}

fn to_status(s: Option<&Snapshot>) -> Status {
    let Some(s) = s else {
        return Status { state: "stopped".into(), ..Default::default() };
    };
    Status {
        state: s.state.as_str().into(),
        account: s.account.as_str().into(),
        sync_dir: s.sync_dir.as_str().into(),
        last_error: s.last_error.trim_start_matches(odrs_ipc::AUTH_ERROR_PREFIX).trim().into(),
        needs_login: s.last_error.starts_with(odrs_ipc::AUTH_ERROR_PREFIX),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_names() {
        assert_eq!(default_name("Engineering", "Documents"), "Engineering");
        assert_eq!(default_name("Direction", "Contrats"), "Contrats");
        assert_eq!(default_name("A/B", "Documents partagés"), "A-B");
    }
}
