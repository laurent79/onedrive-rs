//! Fake daemon publishing `io.github.onedrive_rs.OneDrive1` with simulated progress, to
//! develop clients (GUI) without touching an account:
//! `cargo run -p odrs-ipc --example mock_daemon`

use std::time::Duration;

use odrs_ipc::{Command, Info, Progress, Server, State};
use tokio::sync::mpsc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> odrs_ipc::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let info = Info { account: "someone@example.com".into(), sync_dir: std::env::temp_dir().display().to_string() };
    let server = Server::start(info, tx).await?;
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
                if progress.items >= 2000 {
                    server.finish_pass(Ok("2026-10-23T12:00:00Z".into())).await?;
                    server.set_state(State::Idle).await?;
                    paused = true; // idle until SyncNow
                }
            }
        }
    }
}
