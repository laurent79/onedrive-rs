//! Render one window of the GUI with sample data and save its pixels, to check
//! a layout without looking at the desktop. Run it in a headless compositor:
//!
//! `kwin_wayland --virtual --socket odrs-snap --exit-with-session "cargo run -p odrs-gui --example snapshot -- resync /tmp/w.rgba"`
//!
//! It prints `WIDTHxHEIGHT`; convert with `magick -size WxH -depth 8 rgba:/tmp/w.rgba w.png`.

use std::rc::Rc;
use std::time::Duration;

use odrs_gui::{DriveRow, MainWindow, ReportWindow, ResyncWindow, Status};
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
                DriveRow { verify_state: "done".into(), verify_mins: 12, ..row("main", "OneDrive", "idle") },
                DriveRow {
                    verify_state: "running".into(),
                    verify_done: 4200,
                    verify_total: 9000,
                    verify_fraction: 0.47,
                    ..row("Engineering", "Engineering", "idle")
                },
                DriveRow { verify_state: "done".into(), verify_problems: 2, verify_mins: 90, ..row("Sales", "Sales", "syncing") },
            ];
            w.set_drives(ModelRc::from(Rc::new(VecModel::from(rows))));
            w.show().unwrap();
            let weak = w.as_weak();
            slint::Timer::single_shot(delay, move || snapshot(weak.unwrap().window(), out));
            slint::run_event_loop().unwrap();
        }
        other => panic!("unknown window {other:?} (main, resync, report)"),
    }
}
