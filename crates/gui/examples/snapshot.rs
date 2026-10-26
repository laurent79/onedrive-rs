//! Render one window of the GUI with sample data and save its pixels, to check
//! a layout without looking at the desktop. Run it in a headless compositor:
//!
//! `kwin_wayland --virtual --socket odrs-snap --exit-with-session "cargo run -p odrs-gui --example snapshot -- resync /tmp/w.rgba"`
//!
//! It prints `WIDTHxHEIGHT`; convert with `magick -size WxH -depth 8 rgba:/tmp/w.rgba w.png`.

use std::rc::Rc;
use std::time::Duration;

use odrs_gui::{
    ActivityRow, ActivityWindow, DriveRow, FolderRow, FoldersWindow, MainWindow, ProblemRow, ProblemsWindow, ReportWindow, ResyncWindow, SettingsWindow, Status,
};
use slint::{ComponentHandle, ModelRc, StandardListViewItem, VecModel};

fn snapshot(window: &slint::Window, out: String) {
    let shot = window.take_snapshot().expect("snapshot");
    std::fs::write(&out, shot.as_bytes()).expect("write snapshot");
    println!("{}x{}", shot.width(), shot.height());
    let _ = slint::quit_event_loop();
}

fn main() {
    let mut args = std::env::args().skip(1);
    let which = args.next().unwrap_or_else(|| "resync".into());
    let out = args.next().unwrap_or_else(|| "snapshot.rgba".into());
    let delay = Duration::from_millis(800);
    match which.as_str() {
        "resync" => {
            let w = ResyncWindow::new().unwrap();
            w.set_drive_title("Engineering".into());
            w.show().unwrap();
            let weak = w.as_weak();
            slint::Timer::single_shot(delay, move || snapshot(weak.unwrap().window(), out));
            slint::run_event_loop().unwrap();
        }
        "report" => {
            let w = ReportWindow::new().unwrap();
            w.set_drive_title("OneDrive".into());
            let items: Vec<StandardListViewItem> =
                ["Modified — /home/u/OneDrive/Documents/report.docx", "Missing — /home/u/OneDrive/Pictures/holiday.jpg"]
                    .into_iter()
                    .map(StandardListViewItem::from)
                    .collect();
            w.set_items(ModelRc::from(Rc::new(VecModel::from(items))));
            w.show().unwrap();
            let weak = w.as_weak();
            slint::Timer::single_shot(delay, move || snapshot(weak.unwrap().window(), out));
            slint::run_event_loop().unwrap();
        }
        "settings" => {
            let w = SettingsWindow::new().unwrap();
            w.set_skip_file("*.bak|*.iso".into());
            w.set_skip_dir("node_modules".into());
            w.set_autostart(true);
            w.show().unwrap();
            let weak = w.as_weak();
            slint::Timer::single_shot(delay, move || snapshot(weak.unwrap().window(), out));
            slint::run_event_loop().unwrap();
        }
        "problems" => {
            let w = ProblemsWindow::new().unwrap();
            w.set_drive_title("Sales".into());
            let conflict = |name: &str, backup: &str| ProblemRow {
                kind: "conflict".into(),
                name: name.into(),
                detail: backup.into(),
                path: format!("/home/u/Sales/{name}").into(),
                backup: format!("/home/u/Sales/{backup}").into(),
            };
            let rows = vec![
                conflict("offer.docx", "offer-pc-safeBackup-0001.docx"),
                conflict("prices.xlsx", "prices-pc-safeBackup-0002.xlsx"),
                ProblemRow {
                    kind: "error".into(),
                    name: "locked.pdf".into(),
                    detail: "I/O error: Permission denied (os error 13)".into(),
                    path: "/home/u/Sales/locked.pdf".into(),
                    backup: "".into(),
                },
            ];
            w.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
            w.set_current(0);
            w.show().unwrap();
            let weak = w.as_weak();
            slint::Timer::single_shot(delay, move || snapshot(weak.unwrap().window(), out));
            slint::run_event_loop().unwrap();
        }
        "activity" => {
            let w = ActivityWindow::new().unwrap();
            let row = |kind: &str, name: &str, drive: &str, mins| ActivityRow {
                kind: kind.into(),
                name: name.into(),
                drive: drive.into(),
                mins,
                path: format!("/home/u/{drive}/{name}").into(),
                ..Default::default()
            };
            let rows = vec![
                row("uploaded", "report.docx", "OneDrive", 0),
                row("downloaded", "budget.xlsx", "Sales", 2),
                ActivityRow { backup: "plan-pc-safeBackup-0001.odt".into(), ..row("conflict", "plan.odt", "Engineering", 5) },
                row("deleted", "old-notes.txt", "OneDrive", 12),
                row("deleted-online", "draft.md", "OneDrive", 30),
                ActivityRow { count: 128, ..row("more", "", "Engineering", 75) },
                ActivityRow { path: "".into(), ..row("initial-sync", "", "Engineering", 75) },
            ];
            w.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
            w.set_current(1);
            w.show().unwrap();
            let weak = w.as_weak();
            slint::Timer::single_shot(delay, move || snapshot(weak.unwrap().window(), out));
            slint::run_event_loop().unwrap();
        }
        "folders" => {
            let w = FoldersWindow::new().unwrap();
            w.set_drive_title("OneDrive".into());
            let row = |path: &str, depth, check, expanded, has_children| FolderRow {
                path: path.into(),
                name: path.rsplit('/').next().unwrap().into(),
                depth,
                check,
                expanded,
                has_children,
                loading: false,
            };
            let rows = vec![
                row("Documents", 0, 2, true, true),
                row("Documents/Invoices", 1, 1, false, false),
                row("Documents/Old", 1, 0, false, true),
                row("Music", 0, 1, false, true),
                row("Pictures", 0, 0, false, true),
                row("Teams", 0, 2, false, true),
            ];
            w.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
            w.set_custom_rules(2);
            w.set_delete_local(true);
            w.show().unwrap();
            let weak = w.as_weak();
            slint::Timer::single_shot(delay, move || snapshot(weak.unwrap().window(), out));
            slint::run_event_loop().unwrap();
        }
        "main" => {
            let w = MainWindow::new().unwrap();
            w.set_status(Status { state: "syncing".into(), account: "someone@example.com".into(), ..Default::default() });
            let row = |name: &str, title: &str, state: &str| DriveRow {
                name: name.into(),
                title: title.into(),
                sync_dir: format!("/home/u/{title}").into(),
                state: state.into(),
                last_sync_mins: 3,
                removable: name != "main",
                ..Default::default()
            };
            let rows = vec![
                DriveRow { verify_state: "done".into(), verify_mins: 12, ..row("main", "OneDrive", "paused") },
                DriveRow {
                    verify_state: "running".into(),
                    verify_done: 4200,
                    verify_total: 9000,
                    verify_fraction: 0.47,
                    ..row("Engineering", "Engineering", "idle")
                },
                DriveRow {
                    verify_state: "done".into(),
                    verify_problems: 2,
                    verify_mins: 90,
                    conflicts: 2,
                    failures: 1,
                    show_upload: true,
                    upload_fraction: 0.35,
                    upload_done: 1,
                    upload_files: 3,
                    upload_sent_mb: 105.0,
                    upload_total_mb: 300.0,
                    upload_rate_mbs: 4.2,
                    ..row("Sales", "Sales", "syncing")
                },
            ];
            w.set_drives(ModelRc::from(Rc::new(VecModel::from(rows))));
            // Tall enough for every card.
            w.window().set_size(slint::LogicalSize::new(660.0, 600.0));
            w.show().unwrap();
            let weak = w.as_weak();
            slint::Timer::single_shot(delay, move || snapshot(weak.unwrap().window(), out));
            slint::run_event_loop().unwrap();
        }
        other => panic!("unknown window {other:?} (main, resync, report, folders, activity, problems, settings)"),
    }
}
