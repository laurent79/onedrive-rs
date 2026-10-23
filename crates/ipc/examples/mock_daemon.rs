//! Fake daemon publishing `io.github.onedrive_rs.OneDrive1` with simulated progress, to
//! develop clients (GUI) without touching an account:
//! `cargo run -p odrs-ipc --example mock_daemon`

use std::sync::{Arc, Mutex};
use std::time::Duration;

use odrs_ipc::{BoxFuture, Command, DriveManager, DriveStatus, Info, Library, Progress, Server, Site, State};
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
            _ = tokio::time::sleep(Duration::from_millis(500)), if !paused => {
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
}

impl Mock {
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
        Ok(())
    }
}
