//! Fake daemon publishing `io.github.onedrive_rs.OneDrive1` with simulated progress, to
//! develop clients (GUI) without touching an account:
//! `cargo run -p odrs-ipc --example mock_daemon`

use std::sync::{Arc, Mutex};
use std::time::Duration;

use odrs_ipc::{BoxFuture, Command, DriveManager, DriveStatus, Info, Library, Progress, Server, Site, State, VerifyFinding, VerifyStatus};
use tokio::sync::mpsc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> odrs_ipc::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let info = Info { account: "someone@example.com".into(), sync_dir: std::env::temp_dir().display().to_string() };
    let manager = Arc::new(Mock::default());
    let server = Server::start(info, tx, manager.clone()).await?;
    let mut progress = Progress::default();
    let mut paused = false;
    server.set_state(State::Syncing).await?;
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(Command::Pause) => {
                    paused = true;
                    server.set_state(State::Paused).await?;
                }
                Some(Command::Resume | Command::SyncNow) => {
                    paused = false;
                    progress = Progress::default();
                    server.set_state(State::Syncing).await?;
                }
                None => return Ok(()),
            },
            _ = tokio::time::sleep(Duration::from_millis(500)), if paused => {
                server.set_verifications(manager.advance_verifications()).await?;
            }
            _ = tokio::time::sleep(Duration::from_millis(500)), if !paused => {
                server.set_verifications(manager.advance_verifications()).await?;
                progress.items += 37;
                progress.downloaded += 11;
                progress.unchanged += 26;
                progress.bytes += 48 << 20;
                server.set_progress(progress).await?;
                server.set_drives(manager.statuses(progress)).await?;
                if progress.items >= 2000 {
                    server.finish_pass(Ok("2026-10-23T12:00:00Z".into())).await?;
                    server.set_state(State::Idle).await?;
                    paused = true; // idle until SyncNow
                }
            }
        }
    }
}

/// Fake drive list: `main` plus whatever was added through the GUI.
#[derive(Default)]
struct Mock {
    added: Mutex<Vec<(String, String, String)>>,
    verifications: Mutex<Vec<VerifyStatus>>,
}

/// `main` always ends with a few problems, to show the report.
fn fake_findings() -> Vec<VerifyFinding> {
    vec![
        VerifyFinding { kind: "modified".into(), path: "/tmp/OneDrive/Documents/report.docx".into(), detail: "local AAAA, database BBBB".into() },
        VerifyFinding { kind: "missing".into(), path: "/tmp/OneDrive/Pictures/2024/holiday.jpg".into(), detail: String::new() },
        VerifyFinding { kind: "error".into(), path: "/tmp/OneDrive/Private/locked.pdf".into(), detail: "Permission denied (os error 13)".into() },
    ]
}

impl Mock {
    fn names(&self) -> Vec<String> {
        std::iter::once("main".to_owned()).chain(self.added.lock().unwrap().iter().map(|(n, ..)| n.clone())).collect()
    }

    /// One step of the simulated checks: the first queued one runs.
    fn advance_verifications(&self) -> Vec<VerifyStatus> {
        let mut v = self.verifications.lock().unwrap();
        if !v.iter().any(|s| s.state == "running")
            && let Some(s) = v.iter_mut().find(|s| s.state == "queued")
        {
            s.state = "running".into();
        }
        if let Some(s) = v.iter_mut().find(|s| s.state == "running") {
            s.done = (s.done + 450).min(s.total);
            s.bytes += 900 << 20;
            s.identical = s.done;
            if s.done == s.total {
                s.state = "done".into();
                s.finished = "2026-10-24T21:00:00Z".into();
                if s.name == "main" {
                    (s.modified, s.missing, s.errors, s.same_mtime) = (1, 1, 1, 2);
                    s.identical -= 5;
                }
            }
        }
        v.clone()
    }

    fn statuses(&self, progress: Progress) -> Vec<DriveStatus> {
        let mut out = vec![DriveStatus {
            name: "main".into(),
            sync_dir: "/tmp/OneDrive".into(),
            state: "syncing".into(),
            progress,
            ..Default::default()
        }];
        for (name, drive_id, sync_dir) in self.added.lock().unwrap().iter() {
            out.push(DriveStatus {
                name: name.clone(),
                drive_id: drive_id.clone(),
                sync_dir: sync_dir.clone(),
                state: "idle".into(),
                last_sync: "2026-10-23T12:00:00Z".into(),
                ..Default::default()
            });
        }
        out
    }
}

impl DriveManager for Mock {
    fn search_sites(&self, query: String) -> BoxFuture<'_, Result<Vec<Site>, String>> {
        Box::pin(async move {
            let all = ["Engineering", "Sales", "Management", "HR"];
            Ok(all
                .iter()
                .filter(|n| query.is_empty() || n.to_lowercase().contains(&query.to_lowercase()))
                .map(|n| Site { id: format!("site-{n}"), name: n.to_string(), web_url: format!("https://example.sharepoint.com/sites/{n}") })
                .collect())
        })
    }

    fn list_libraries(&self, site_id: String) -> BoxFuture<'_, Result<Vec<Library>, String>> {
        Box::pin(async move {
            Ok(vec![
                Library { drive_id: format!("{site_id}-docs"), name: "Documents".into(), web_url: String::new() },
                Library { drive_id: format!("{site_id}-archive"), name: "Archive".into(), web_url: String::new() },
            ])
        })
    }

    fn add_drive(&self, name: String, drive_id: String, sync_dir: String) -> Result<(), String> {
        let mut added = self.added.lock().unwrap();
        if name.eq_ignore_ascii_case("main") || added.iter().any(|(n, ..)| n.eq_ignore_ascii_case(&name)) {
            return Err(format!("a drive named \"{name}\" already exists"));
        }
        added.push((name, drive_id, sync_dir));
        Ok(())
    }

    fn remove_drive(&self, name: String) -> Result<(), String> {
        self.added.lock().unwrap().retain(|(n, ..)| n != &name);
        self.verifications.lock().unwrap().retain(|s| s.name != name);
        Ok(())
    }

    fn verify(&self, name: String) -> Result<(), String> {
        let names = if name.is_empty() { self.names() } else { vec![name] };
        let mut v = self.verifications.lock().unwrap();
        for name in names {
            v.retain(|s| s.name != name);
            v.push(VerifyStatus { name, state: "queued".into(), total: 6000, ..Default::default() });
        }
        Ok(())
    }

    fn cancel_verify(&self, name: String) -> Result<(), String> {
        for s in self.verifications.lock().unwrap().iter_mut() {
            if (name.is_empty() || s.name == name) && (s.state == "queued" || s.state == "running") {
                s.state = "cancelled".into();
                s.finished = "2026-10-24T21:00:00Z".into();
            }
        }
        Ok(())
    }

    fn verify_report(&self, name: String) -> Result<Vec<VerifyFinding>, String> {
        let v = self.verifications.lock().unwrap();
        Ok(if name == "main" && v.iter().any(|s| s.name == "main" && s.state == "done") { fake_findings() } else { Vec::new() })
    }
}
