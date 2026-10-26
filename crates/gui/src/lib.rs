//! Desktop UI: a status window listing the synchronised drives, a window to add
//! a SharePoint library, a folder chooser (selective sync), and a tray icon
//! (StatusNotifierItem), all driven over D-Bus by the daemon. The UI never
//! performs sync I/O itself.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use odrs_ipc::{Client, DriveStatus, Event, FolderChoice, Snapshot, VerifyFinding, VerifyStatus};
use slint::{ComponentHandle as _, Model as _, ModelRc, SharedString, StandardListViewItem, VecModel};
use tokio::sync::mpsc;

use crate::folders::FolderTree;
use crate::notify::{Action, Change, Note, Notifier, Watcher};

mod folders;
mod notify;

thread_local! {
    /// The folder chooser's state, on the UI thread.
    static FOLDERS: RefCell<FolderTree> = RefCell::new(FolderTree::default());
}

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
    Resync(String),
    /// Subfolders of a folder for the folder chooser (empty path: the top).
    ListFolders { drive: String, path: String },
    CustomRules(String),
    SetSelection { drive: String, choices: Vec<FolderChoice>, delete_local: bool },
    ResolveConflict { drive: String, backup: String, keep_local: bool },
}

/// Run the UI on the current thread until the user quits from the tray menu.
/// `rt` hosts the D-Bus client; it must not be the thread calling `run`.
pub fn run(rt: tokio::runtime::Handle, opts: Options) -> Result<(), Error> {
    // Creating a component initialises the Slint platform.
    let tray = Tray::new()?;
    // Before any window exists. Matches contrib/desktop/io.github.onedrive_rs.desktop,
    // from which Wayland compositors take the window icon.
    slint::set_xdg_app_id(APP_ID)?;
    let (tx, rx) = mpsc::unbounded_channel();
    let ctx = Rc::new(Ctx { tx, rt: rt.clone(), start_daemon: opts.start_daemon, login: opts.login });
    tray.set_status(to_status(None));
    wire_logic(&tray.global::<Logic>(), &ctx);
    UI.with(|u| {
        let mut u = u.borrow_mut();
        u.ctx = Some(ctx);
        u.tray = Some(tray);
    });

    rt.spawn(poll(rx));

    if let Some(w) = (!opts.hidden).then(main_window).flatten() {
        w.show()?;
    }
    // Closing the window only hides it; the tray keeps the app alive.
    slint::run_event_loop_until_quit()?;
    Ok(())
}

/// What the windows' callbacks need.
struct Ctx {
    tx: mpsc::UnboundedSender<UiCommand>,
    rt: tokio::runtime::Handle,
    /// Launch the background service when it is not running.
    start_daemon: Box<dyn Fn() -> std::io::Result<()>>,
    login: Box<dyn Fn() -> std::io::Result<std::process::Child>>,
}

impl Ctx {
    fn send(&self, cmd: UiCommand) {
        let _ = self.tx.send(cmd);
    }
}

/// The UI thread's state. Windows are created when first opened: KDE Plasma
/// lists a created Wayland window in the task bar even before it is shown, as
/// an empty entry. Closing one destroys its surface, not the component.
#[derive(Default)]
struct Ui {
    ctx: Option<Rc<Ctx>>,
    tray: Option<Tray>,
    main: Option<MainWindow>,
    add: Option<AddLibraryWindow>,
    report: Option<ReportWindow>,
    resync: Option<ResyncWindow>,
    folders: Option<FoldersWindow>,
    activity: Option<ActivityWindow>,
    problems: Option<ProblemsWindow>,
    /// Latest daemon state, for a main window opened later.
    status: Status,
    /// Latest drive statuses, for the "to resolve" window.
    drive_statuses: Vec<DriveStatus>,
    /// Conflicts settled from that window (their safeBackup paths), hidden
    /// until the daemon's next pass of the drive no longer lists them.
    resolved: std::collections::HashSet<String>,
    /// Latest daemon events, oldest first, for the activity window.
    events: Vec<Event>,
    /// Updated in place, so rows (and their pending "confirm removal") survive refreshes.
    drives: Rc<VecModel<DriveRow>>,
}

thread_local! {
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
}

fn ctx() -> Rc<Ctx> {
    UI.with(|u| u.borrow().ctx.clone()).expect("UI context set by run()")
}

/// The window in `slot` if it was created.
fn existing<T: slint::ComponentHandle>(slot: fn(&Ui) -> &Option<T>) -> Option<T> {
    UI.with(|u| slot(&u.borrow()).as_ref().map(T::clone_strong))
}

/// The window in `slot`, created (and wired) by `create` the first time.
fn lazy<T: slint::ComponentHandle>(
    slot: fn(&mut Ui) -> &mut Option<T>,
    create: fn(&Rc<Ctx>) -> Result<T, slint::PlatformError>,
) -> Option<T> {
    if let Some(w) = UI.with(|u| slot(&mut u.borrow_mut()).as_ref().map(T::clone_strong)) {
        return Some(w);
    }
    match create(&ctx()) {
        Ok(w) => {
            UI.with(|u| *slot(&mut u.borrow_mut()) = Some(w.clone_strong()));
            Some(w)
        }
        Err(e) => {
            tracing::error!(error = %e, "cannot create a window");
            None
        }
    }
}

fn main_window() -> Option<MainWindow> {
    lazy(
        |u| &mut u.main,
        |ctx| {
            let w = MainWindow::new()?;
            let (status, drives) = UI.with(|u| {
                let u = u.borrow();
                (u.status.clone(), u.drives.clone())
            });
            w.set_status(status);
            w.set_drives(ModelRc::from(drives));
            wire_logic(&w.global::<Logic>(), ctx);
            Ok(w)
        },
    )
}

fn add_window() -> Option<AddLibraryWindow> {
    lazy(
        |u| &mut u.add,
        |ctx| {
            let w = AddLibraryWindow::new()?;
            wire_add_window(&w, &ctx.tx);
            Ok(w)
        },
    )
}

fn report_window() -> Option<ReportWindow> {
    lazy(
        |u| &mut u.report,
        |ctx| {
            let w = ReportWindow::new()?;
            wire_report_window(&w, &ctx.tx, &ctx.rt);
            Ok(w)
        },
    )
}

fn resync_window() -> Option<ResyncWindow> {
    lazy(
        |u| &mut u.resync,
        |ctx| {
            let w = ResyncWindow::new()?;
            let (r, t) = (w.as_weak(), ctx.tx.clone());
            w.on_confirm(move || {
                let Some(r) = r.upgrade() else { return };
                let _ = t.send(UiCommand::Resync(r.get_drive_name().into()));
                let _ = r.hide();
            });
            let r = w.as_weak();
            w.on_cancel(move || {
                if let Some(r) = r.upgrade() {
                    let _ = r.hide();
                }
            });
            Ok(w)
        },
    )
}

fn folders_window() -> Option<FoldersWindow> {
    lazy(
        |u| &mut u.folders,
        |ctx| {
            let w = FoldersWindow::new()?;
            w.set_rows(ModelRc::from(Rc::new(VecModel::<FolderRow>::default())));
            wire_folders_window(&w, &ctx.tx);
            Ok(w)
        },
    )
}

fn activity_window() -> Option<ActivityWindow> {
    lazy(
        |u| &mut u.activity,
        |ctx| {
            let w = ActivityWindow::new()?;
            w.set_rows(ModelRc::from(Rc::new(VecModel::<ActivityRow>::default())));
            let (a, rt) = (w.as_weak(), ctx.rt.clone());
            w.on_show_item(move |i| {
                let Some(a) = a.upgrade() else { return };
                if let Some(row) = usize::try_from(i).ok().and_then(|i| a.get_rows().row_data(i)).filter(|r| !r.path.is_empty()) {
                    rt.spawn(show_in_folder(row.path.into()));
                }
            });
            let a = w.as_weak();
            w.on_dismiss(move || {
                if let Some(a) = a.upgrade() {
                    let _ = a.hide();
                }
            });
            Ok(w)
        },
    )
}

fn problems_window() -> Option<ProblemsWindow> {
    lazy(
        |u| &mut u.problems,
        |ctx| {
            let w = ProblemsWindow::new()?;
            w.set_rows(ModelRc::from(Rc::new(VecModel::<ProblemRow>::default())));
            let (p, t) = (w.as_weak(), ctx.tx.clone());
            w.on_keep(move |i, keep_local| {
                let Some(p) = p.upgrade() else { return };
                let Some(row) = usize::try_from(i).ok().and_then(|i| p.get_rows().row_data(i)) else { return };
                p.set_busy(true);
                p.set_message(SharedString::new());
                let drive = p.get_drive_name().into();
                let _ = t.send(UiCommand::ResolveConflict { drive, backup: row.backup.into(), keep_local });
            });
            let (p, rt) = (w.as_weak(), ctx.rt.clone());
            w.on_show_item(move |i| {
                let Some(p) = p.upgrade() else { return };
                if let Some(row) = usize::try_from(i).ok().and_then(|i| p.get_rows().row_data(i)) {
                    // A conflict's copy sits next to the file: show it, the file is beside it.
                    let path = if row.backup.is_empty() { row.path } else { row.backup };
                    rt.spawn(show_in_folder(path.into()));
                }
            });
            let t = ctx.tx.clone();
            w.on_retry(move || {
                let _ = t.send(UiCommand::SyncNow);
            });
            let p = w.as_weak();
            w.on_dismiss(move || {
                if let Some(p) = p.upgrade() {
                    let _ = p.hide();
                }
            });
            Ok(w)
        },
    )
}

/// Show the conflicts, then the failed items, of the window's drive.
fn refresh_problems(w: &ProblemsWindow) {
    let drive = w.get_drive_name();
    let file_name = |p: &str| std::path::Path::new(p).file_name().map_or(p.to_owned(), |n| n.to_string_lossy().into_owned());
    let rows: Vec<ProblemRow> = UI.with(|u| {
        let u = u.borrow();
        let Some(d) = u.drive_statuses.iter().find(|d| d.name == drive.as_str()) else { return Vec::new() };
        let conflicts = d.conflicts.iter().filter(|c| !u.resolved.contains(&c.backup)).map(|c| ProblemRow {
            kind: "conflict".into(),
            name: file_name(&c.path).into(),
            detail: file_name(&c.backup).into(),
            path: c.path.as_str().into(),
            backup: c.backup.as_str().into(),
        });
        let failures = d.failures.iter().map(|f| ProblemRow {
            kind: "error".into(),
            name: file_name(&f.path).into(),
            detail: f.message.as_str().into(),
            path: f.path.as_str().into(),
            backup: SharedString::new(),
        });
        conflicts.chain(failures).collect()
    });
    let model = w.get_rows();
    if let Some(model) = model.as_any().downcast_ref::<VecModel<ProblemRow>>()
        && model.iter().ne(rows.iter().cloned())
    {
        let selected = usize::try_from(w.get_current()).ok().and_then(|i| model.row_data(i));
        model.set_vec(rows);
        // Keep the selection on the same line if it is still there.
        let current = selected.and_then(|s| model.iter().position(|r| r == s)).map_or(-1, |i| i32::try_from(i).unwrap_or(-1));
        w.set_current(current);
    }
}

/// Show the latest events in the activity window, newest first (only if they
/// changed, so that the selection stays).
fn refresh_activity(w: &ActivityWindow) {
    let rows: Vec<ActivityRow> = UI.with(|u| u.borrow().events.iter().rev().map(activity_row).collect());
    let model = w.get_rows();
    if let Some(model) = model.as_any().downcast_ref::<VecModel<ActivityRow>>()
        && model.iter().ne(rows.iter().cloned())
    {
        model.set_vec(rows);
    }
}

fn activity_row(e: &Event) -> ActivityRow {
    let file_name = |p: &str| std::path::Path::new(p).file_name().map_or(p.to_owned(), |n| n.to_string_lossy().into_owned());
    ActivityRow {
        kind: e.kind.as_str().into(),
        name: if e.path.is_empty() { SharedString::new() } else { file_name(&e.path).into() },
        drive: drive_title(&e.drive).into(),
        mins: minutes_since(&e.time).unwrap_or(0),
        path: e.path.as_str().into(),
        backup: if e.kind == "conflict" { file_name(&e.detail).into() } else { SharedString::new() },
        count: if e.kind == "more" { e.detail.parse().unwrap_or(0) } else { 0 },
    }
}

/// Run `f` on the UI thread with the window `get` returns, if any.
fn in_ui<T: 'static>(get: fn() -> Option<T>, f: impl FnOnce(T) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = get() {
            f(w);
        }
    });
}

/// Globals are per component instance: the main window and the tray are wired alike.
fn wire_logic(logic: &Logic<'_>, ctx: &Rc<Ctx>) {
    let c = ctx.clone();
    logic.on_sync_now(move || c.send(UiCommand::SyncNow));
    let c = ctx.clone();
    logic.on_toggle_pause(move || c.send(UiCommand::TogglePause));
    logic.on_open_folder(|path| open_folder(&path));
    let c = ctx.clone();
    logic.on_remove_drive(move |name| c.send(UiCommand::RemoveDrive(name.into())));
    let c = ctx.clone();
    logic.on_verify(move |name| c.send(UiCommand::Verify(name.into())));
    let c = ctx.clone();
    logic.on_cancel_verify(move |name| c.send(UiCommand::CancelVerify(name.into())));
    let c = ctx.clone();
    logic.on_show_report(move |name| c.send(UiCommand::ShowReport(name.into())));
    logic.on_resync(move |name| {
        let Some(r) = resync_window() else { return };
        r.set_drive_title(drive_title(&name).into());
        r.set_drive_name(name);
        let _ = r.show();
    });
    let c = ctx.clone();
    logic.on_choose_folders(move |name| {
        if let Some(f) = folders_window() {
            open_folders_window(&f, &name, &c.tx);
        }
    });
    let c = ctx.clone();
    logic.on_add_library(move || {
        if let Some(a) = add_window() {
            open_add_window(&a, &c.tx);
        }
    });
    let c = ctx.clone();
    logic.on_start_daemon(move || {
        if let Err(e) = (c.start_daemon)() {
            tracing::error!(error = %e, "could not start the daemon");
        }
        c.send(UiCommand::Refresh);
    });
    let c = ctx.clone();
    logic.on_reconnect(move || match (c.login)() {
        Ok(mut child) => {
            let t = c.tx.clone();
            std::thread::spawn(move || {
                if child.wait().is_ok_and(|s| s.success()) {
                    let _ = t.send(UiCommand::SyncNow);
                }
            });
        }
        Err(e) => tracing::error!(error = %e, "could not start the login"),
    });
    logic.on_show_problems(|name| {
        if let Some(w) = problems_window() {
            UI.with(|u| u.borrow_mut().resolved.clear());
            w.set_drive_title(drive_title(&name).into());
            w.set_drive_name(name);
            w.set_current(-1);
            w.set_busy(false);
            w.set_message(SharedString::new());
            refresh_problems(&w);
            let _ = w.show();
        }
    });
    logic.on_show_activity(|| {
        if let Some(w) = activity_window() {
            w.set_current(-1);
            refresh_activity(&w);
            let _ = w.show();
        }
    });
    logic.on_show_window(|| {
        if let Some(w) = main_window() {
            let _ = w.show();
        }
    });
    logic.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
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

fn open_folders_window(f: &FoldersWindow, name: &str, tx: &mpsc::UnboundedSender<UiCommand>) {
    FOLDERS.with(|t| *t.borrow_mut() = FolderTree::new(name));
    f.set_drive_name(name.into());
    f.set_drive_title(drive_title(name).into());
    f.set_delete_local(false);
    f.set_saving(false);
    f.set_custom_rules(0);
    f.set_message(SharedString::new());
    refresh_folders(f);
    let _ = f.show();
    let _ = tx.send(UiCommand::ListFolders { drive: name.to_owned(), path: String::new() });
    let _ = tx.send(UiCommand::CustomRules(name.to_owned()));
}

/// Show the folder chooser's current lines.
fn refresh_folders(f: &FoldersWindow) {
    let (rows, listed, selected) = FOLDERS.with(|t| {
        let mut t = t.borrow_mut();
        let rows: Vec<FolderRow> = t
            .lines()
            .into_iter()
            .map(|l| FolderRow {
                path: l.path.into(),
                name: l.name.into(),
                depth: i32::try_from(l.depth).unwrap_or(0),
                check: match l.check {
                    Some(false) => 0,
                    Some(true) => 1,
                    None => 2,
                },
                expanded: l.expanded,
                has_children: l.has_children,
                loading: l.loading,
            })
            .collect();
        (rows, t.is_listed(), t.anything_selected())
    });
    if let Some(model) = f.get_rows().as_any().downcast_ref::<VecModel<FolderRow>>() {
        model.set_vec(rows);
    }
    f.set_loading(!listed);
    f.set_nothing_selected(listed && !selected);
}

fn wire_folders_window(folders: &FoldersWindow, tx: &mpsc::UnboundedSender<UiCommand>) {
    let f = folders.as_weak();
    folders.on_toggle(move |i| {
        let Some(f) = f.upgrade() else { return };
        FOLDERS.with(|t| t.borrow_mut().toggle(usize::try_from(i).unwrap_or(usize::MAX)));
        refresh_folders(&f);
    });
    let (f, t) = (folders.as_weak(), tx.clone());
    folders.on_expand(move |i| {
        let Some(f) = f.upgrade() else { return };
        let (drive, listing) = FOLDERS.with(|tree| {
            let mut tree = tree.borrow_mut();
            (tree.drive.clone(), tree.expand(usize::try_from(i).unwrap_or(usize::MAX)))
        });
        if let Some(path) = listing {
            let _ = t.send(UiCommand::ListFolders { drive, path });
        }
        refresh_folders(&f);
    });
    let (f, t) = (folders.as_weak(), tx.clone());
    folders.on_save(move || {
        let Some(f) = f.upgrade() else { return };
        let (drive, choices) = FOLDERS.with(|t| {
            let t = t.borrow();
            (t.drive.clone(), t.choices())
        });
        f.set_saving(true);
        f.set_message(SharedString::new());
        let _ = t.send(UiCommand::SetSelection { drive, choices, delete_local: f.get_delete_local() });
    });
    let f = folders.as_weak();
    folders.on_cancel(move || {
        if let Some(f) = f.upgrade() {
            let _ = f.hide();
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
async fn poll(mut rx: mpsc::UnboundedReceiver<UiCommand>) {
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
    let mut watcher = Watcher::default();
    // Notes are translated on the UI thread, then come back here to be sent.
    let (notes_tx, mut notes_rx) = mpsc::unbounded_channel::<Vec<Note>>();
    let mut notifier = match Notifier::new().await {
        Ok(n) => Some(n),
        Err(e) => {
            tracing::warn!(error = %e, "desktop notifications unavailable");
            None
        }
    };
    let mut clicks = match &notifier {
        Some(n) => n.clicks().await.ok(),
        None => None,
    };
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { return };
                handle(&client, cmd, last.as_ref()).await;
            }
            Some(notes) = notes_rx.recv() => {
                if let Some(n) = notifier.as_mut() {
                    for note in notes {
                        n.send(note).await;
                    }
                }
                continue;
            }
            Some(click) = next_click(&mut clicks) => {
                let action = click.args().ok().and_then(|a| notifier.as_mut()?.action(a.id, &a.action_key));
                match action {
                    Some(Action::ShowWindow) => in_ui(main_window, |w| {
                        let _ = w.show();
                    }),
                    Some(Action::Reconnect) => in_tray(|t| t.global::<Logic>().invoke_reconnect()),
                    Some(Action::ShowReport(drive)) => handle(&client, UiCommand::ShowReport(drive), last.as_ref()).await,
                    Some(Action::ShowItem(path)) => show_in_folder(path).await,
                    Some(Action::OpenFolder(path)) => open_folder(&path),
                    None => {}
                }
                continue;
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
        let changes = watcher.changes(snapshot.as_ref());
        if !changes.is_empty() {
            let notes_tx = notes_tx.clone();
            in_tray(move |t| {
                let fmt = t.global::<Fmt>();
                let _ = notes_tx.send(changes.iter().map(|c| to_note(&fmt, c)).collect());
            });
        }
        // Kept while the daemon is stopped.
        let events = snapshot.as_ref().map(|s| s.events.clone());
        let drive_statuses = snapshot.as_ref().map(|s| s.drives.clone());
        last = snapshot;
        let _ = slint::invoke_from_event_loop(move || {
            let (main, drives, activity, problems) = UI.with(|u| {
                let mut u = u.borrow_mut();
                u.status = status.clone();
                if let Some(events) = events {
                    u.events = events;
                }
                if let Some(drives) = drive_statuses {
                    u.drive_statuses = drives;
                }
                if let Some(t) = u.tray.as_ref().filter(|t| t.get_status() != status) {
                    t.set_status(status.clone());
                }
                (
                    u.main.as_ref().map(MainWindow::clone_strong),
                    u.drives.clone(),
                    u.activity.as_ref().map(ActivityWindow::clone_strong),
                    u.problems.as_ref().map(ProblemsWindow::clone_strong),
                )
            });
            sync_rows(&drives, rows);
            if let Some(w) = main.filter(|w| w.get_status() != status) {
                w.set_status(status);
            }
            if let Some(w) = activity.filter(|w| w.window().is_visible()) {
                refresh_activity(&w);
            }
            if let Some(w) = problems.filter(|w| w.window().is_visible()) {
                refresh_problems(&w);
            }
        });
    }
}

async fn next_click(clicks: &mut Option<notify::ActionInvokedStream>) -> Option<notify::ActionInvoked> {
    match clicks {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

/// Texts of a notification, translated through the Slint catalogue.
fn to_note(fmt: &Fmt<'_>, change: &Change) -> Note {
    let title = |kind: &str, drive: &str| fmt.invoke_note_title(kind.into(), drive_title(drive).into()).to_string();
    let body = |kind: &str, a: &str, b: &str, n: u64| {
        fmt.invoke_note_body(kind.into(), a.into(), b.into(), i32::try_from(n).unwrap_or(i32::MAX)).to_string()
    };
    let button = |kind: &str, action: Action| {
        let label = fmt.invoke_note_button(kind.into());
        (!label.is_empty()).then(|| (label.to_string(), action))
    };
    let file_name = |p: &str| std::path::Path::new(p).file_name().map_or(p.to_owned(), |n| n.to_string_lossy().into_owned());
    let note = |kind: &str, drive: &str, body: String, button: Option<(String, Action)>| Note {
        title: title(kind, drive),
        body,
        button,
        urgent: kind == "login",
    };
    match change {
        Change::LoginExpired => note("login", "", body("login", "", "", 0), button("login", Action::Reconnect)),
        Change::DriveError { drive, message } => {
            let message = message.trim_start_matches(odrs_ipc::AUTH_ERROR_PREFIX).trim();
            note("error", drive, body("error", message, "", 0), None)
        }
        Change::Conflict { drive, path, backup } => note(
            "conflict",
            drive,
            body("conflict", &file_name(path), &file_name(backup), 0),
            button("conflict", Action::ShowItem(backup.clone())),
        ),
        Change::Conflicts { drive, count } => note("conflicts", drive, body("conflicts", "", "", *count as u64), None),
        Change::InitialSync { drive, sync_dir } => note(
            "initial-sync",
            drive,
            body("initial-sync", "", "", 0),
            (!sync_dir.is_empty()).then(|| button("initial-sync", Action::OpenFolder(sync_dir.clone()))).flatten(),
        ),
        Change::VerifyDone { drive, problems: 0 } => note("verify-ok", drive, body("verify-ok", "", "", 0), None),
        Change::VerifyDone { drive, problems } => note(
            "verify-problems",
            drive,
            body("verify-problems", "", "", *problems),
            button("verify-problems", Action::ShowReport(drive.clone())),
        ),
    }
}

/// Run `f` on the UI thread with the tray icon (always there).
fn in_tray(f: impl FnOnce(&Tray) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        UI.with(|u| {
            if let Some(t) = &u.borrow().tray {
                f(t);
            }
        });
    });
}

fn existing_add() -> Option<AddLibraryWindow> {
    existing(|u| &u.add)
}

fn existing_problems() -> Option<ProblemsWindow> {
    existing(|u| &u.problems)
}

fn existing_folders() -> Option<FoldersWindow> {
    existing(|u| &u.folders)
}

async fn handle(client: &Client, cmd: UiCommand, last: Option<&Snapshot>) {
    let d = client.daemon();
    match cmd {
        UiCommand::SyncNow => report(d.sync_now().await, "sync now"),
        UiCommand::TogglePause if last.is_some_and(|s| s.state == "paused") => report(d.resume().await, "resume"),
        UiCommand::TogglePause => report(d.pause().await, "pause"),
        UiCommand::Refresh => {}
        UiCommand::RemoveDrive(name) => report(d.remove_drive(&name).await, "remove drive"),
        UiCommand::Verify(name) => report(d.verify(&name).await, "verify"),
        UiCommand::Resync(name) => report(d.resync(&name).await, "resync"),
        UiCommand::CancelVerify(name) => report(d.cancel_verify(&name).await, "cancel verify"),
        UiCommand::ShowReport(name) => {
            let title = drive_title(&name);
            let findings = match d.verify_report(&name).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot read the verification report");
                    return;
                }
            };
            in_ui(report_window, move |r| {
                r.set_drive_name(name.into());
                r.set_drive_title(title.into());
                r.set_current(-1);
                let labels: Vec<String> = findings.iter().map(|f| finding_label(&r, f)).collect();
                r.set_items(list(labels.iter().map(String::as_str)));
                r.set_paths(strings(findings.iter().map(|f| f.path.as_str())));
                let _ = r.show();
            });
        }
        UiCommand::ListFolders { drive, path } => {
            let result = d.list_folders(&drive, &path).await;
            in_ui(existing_folders, move |f| {
                FOLDERS.with(|t| {
                    let mut t = t.borrow_mut();
                    if t.drive != drive {
                        return;
                    }
                    match result {
                        Ok(list) => t.add_children(&path, list),
                        Err(e) => {
                            t.listing_failed(&path);
                            f.set_message(daemon_message(&e).into());
                        }
                    }
                });
                refresh_folders(&f);
            });
        }
        UiCommand::ResolveConflict { drive, backup, keep_local } => {
            let result = d.resolve_conflict(&drive, &backup, keep_local).await;
            in_ui(existing_problems, move |w| {
                w.set_busy(false);
                match result {
                    Ok(()) => {
                        UI.with(|u| u.borrow_mut().resolved.insert(backup));
                        refresh_problems(&w);
                    }
                    Err(e) => w.set_message(daemon_message(&e).into()),
                }
            });
        }
        UiCommand::CustomRules(drive) => {
            let n = d.custom_rules(&drive).await.map_or(0, |r| r.len());
            in_ui(existing_folders, move |f| {
                if f.get_drive_name() == drive {
                    f.set_custom_rules(i32::try_from(n).unwrap_or(i32::MAX));
                }
            });
        }
        UiCommand::SetSelection { drive, choices, delete_local } => {
            let result = d.set_selection(&drive, &choices, delete_local).await;
            in_ui(existing_folders, move |f| {
                f.set_saving(false);
                match result {
                    Ok(()) => {
                        let _ = f.hide();
                    }
                    Err(e) => f.set_message(daemon_message(&e).into()),
                }
            });
        }
        UiCommand::SearchSites(query) => {
            let result = d.search_sites(&query).await;
            in_ui(existing_add, move |a| {
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
            in_ui(existing_add, move |a| {
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
            in_ui(existing_add, move |a| {
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

/// Daemon-side failures arrive as org.freedesktop.DBus.Error.Failed with a readable message.
fn daemon_message(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(detail), _) => detail.clone(),
        other => other.to_string(),
    }
}

fn show_error(a: &AddLibraryWindow, e: &zbus::Error) {
    a.set_no_results(false);
    a.set_message(daemon_message(e).into());
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
    /// Download and upload bytes per second for this drive, if measurable.
    fn sample(&mut self, d: &DriveStatus) -> (Option<f64>, Option<f64>) {
        let syncing = d.state == "syncing";
        let down = self.measure(d.name.clone(), d.progress.synced_bytes, syncing);
        let up = self.measure(format!("{}\0up", d.name), d.progress.upload_sent, syncing);
        (down, up)
    }

    fn measure(&mut self, key: String, bytes: u64, syncing: bool) -> Option<f64> {
        let now = Instant::now();
        let q = self.samples.entry(key).or_default();
        if !syncing || q.back().is_some_and(|&(_, b)| b > bytes) {
            // Not syncing, or a new pass started: start measuring again.
            q.clear();
        }
        if !syncing {
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

/// Below this, uploads are over too quickly for a bar.
const UPLOAD_BAR_MIN_BYTES: u64 = 8 << 20;

fn to_row(d: &DriveStatus, (rate, upload_rate): (Option<f64>, Option<f64>), v: Option<&VerifyStatus>) -> DriveRow {
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
        show_upload: d.state == "syncing" && p.upload_bytes >= UPLOAD_BAR_MIN_BYTES && p.uploaded < p.upload_files,
        upload_fraction: if p.upload_bytes > 0 { (p.upload_sent as f64 / p.upload_bytes as f64) as f32 } else { 0.0 },
        upload_done: int(p.uploaded),
        upload_files: int(p.upload_files),
        upload_sent_mb: (p.upload_sent as f64 / 1e6) as f32,
        upload_total_mb: (p.upload_bytes as f64 / 1e6) as f32,
        upload_rate_mbs: upload_rate.map_or(-1.0, |r| (r / 1e6) as f32),
        title: drive_title(&d.name).into(),
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
        conflicts: int(d.conflicts.len() as u64),
        failures: int(d.failures.len() as u64),
    }
}

/// Shown to the user: the account's own drive is "OneDrive".
fn drive_title(name: &str) -> String {
    if name == "main" { "OneDrive".to_owned() } else { name.to_owned() }
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
