//! D-Bus interface `io.github.onedrive_rs.OneDrive1` between the daemon (server) and its
//! clients (GUI, CLI). The daemon owns all sync I/O; clients only observe and
//! send commands.

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::{OwnedValue, Type, Value};

pub const BUS_NAME: &str = "io.github.onedrive_rs.OneDrive1";
pub const OBJECT_PATH: &str = "/io/github/onedrive_rs/OneDrive1";

pub use zbus::Error;
pub type Result<T> = zbus::Result<T>;

/// Daemon state, exposed as the `State` property (lower-case string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Idle,
    Syncing,
    Paused,
    Error,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Syncing => "syncing",
            State::Paused => "paused",
            State::Error => "error",
        }
    }
}

/// Counters of the current (or last) sync pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct Progress {
    pub items: u64,
    pub downloaded: u64,
    pub bytes: u64,
    pub unchanged: u64,
    pub deleted: u64,
    pub skipped: u64,
    pub errors: u64,
}

/// Commands sent by clients to the daemon loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    SyncNow,
    Pause,
    Resume,
}

/// Static facts about the daemon, set once at startup.
#[derive(Debug, Clone, Default)]
pub struct Info {
    pub account: String,
    pub sync_dir: String,
}

struct Service {
    info: Info,
    state: State,
    last_sync: String,
    last_error: String,
    progress: Progress,
    commands: mpsc::UnboundedSender<Command>,
}

impl Service {
    fn send(&self, cmd: Command) -> zbus::fdo::Result<()> {
        self.commands.send(cmd).map_err(|_| zbus::fdo::Error::Failed("daemon is shutting down".into()))
    }
}

#[zbus::interface(name = "io.github.onedrive_rs.OneDrive1")]
impl Service {
    /// Start a sync pass now (no-op while syncing or paused).
    async fn sync_now(&self) -> zbus::fdo::Result<()> {
        self.send(Command::SyncNow)
    }

    /// Stop the running pass (if any) and stop scheduling new ones.
    async fn pause(&self) -> zbus::fdo::Result<()> {
        self.send(Command::Pause)
    }

    async fn resume(&self) -> zbus::fdo::Result<()> {
        self.send(Command::Resume)
    }

    #[zbus(property)]
    async fn state(&self) -> &str {
        self.state.as_str()
    }

    /// RFC 3339 time of the last successful pass, empty if none.
    #[zbus(property)]
    async fn last_sync(&self) -> &str {
        &self.last_sync
    }

    #[zbus(property)]
    async fn last_error(&self) -> &str {
        &self.last_error
    }

    #[zbus(property)]
    async fn account(&self) -> &str {
        &self.info.account
    }

    #[zbus(property)]
    async fn sync_dir(&self) -> &str {
        &self.info.sync_dir
    }

    #[zbus(property)]
    async fn progress(&self) -> Progress {
        self.progress
    }
}

/// Handle used by the daemon to publish its state. Cheap to clone.
#[derive(Clone)]
pub struct Server {
    // Keeps the bus connection (and the well-known name) alive.
    _conn: zbus::Connection,
    iface: InterfaceRef<Service>,
}

impl Server {
    /// Claim [`BUS_NAME`] on the session bus and serve the interface.
    /// Fails if another daemon already owns the name.
    pub async fn start(info: Info, commands: mpsc::UnboundedSender<Command>) -> Result<Self> {
        let service = Service {
            info,
            state: State::Idle,
            last_sync: String::new(),
            last_error: String::new(),
            progress: Progress::default(),
            commands,
        };
        let conn = zbus::connection::Builder::session()?
            .name(BUS_NAME)?
            .serve_at(OBJECT_PATH, service)?
            .build()
            .await?;
        let iface = conn.object_server().interface::<_, Service>(OBJECT_PATH).await?;
        Ok(Self { _conn: conn, iface })
    }

    fn emitter(&self) -> &SignalEmitter<'static> {
        self.iface.signal_emitter()
    }

    pub async fn set_state(&self, state: State) -> Result<()> {
        let mut s = self.iface.get_mut().await;
        if s.state != state {
            s.state = state;
            s.state_changed(self.emitter()).await?;
        }
        Ok(())
    }

    pub async fn set_progress(&self, progress: Progress) -> Result<()> {
        let mut s = self.iface.get_mut().await;
        if s.progress != progress {
            s.progress = progress;
            s.progress_changed(self.emitter()).await?;
        }
        Ok(())
    }

    /// Record the end of a pass: `Ok(time)` on success, `Err(message)` on failure.
    pub async fn finish_pass(&self, result: std::result::Result<String, String>) -> Result<()> {
        let mut s = self.iface.get_mut().await;
        match result {
            Ok(time) => {
                s.last_sync = time;
                s.last_sync_changed(self.emitter()).await?;
                if !s.last_error.is_empty() {
                    s.last_error.clear();
                    s.last_error_changed(self.emitter()).await?;
                }
            }
            Err(msg) => {
                s.last_error = msg;
                s.last_error_changed(self.emitter()).await?;
            }
        }
        Ok(())
    }
}

/// Client-side proxy (CLI, GUI).
#[zbus::proxy(interface = "io.github.onedrive_rs.OneDrive1", default_service = "io.github.onedrive_rs.OneDrive1", default_path = "/io/github/onedrive_rs/OneDrive1")]
pub trait OneDrive {
    fn sync_now(&self) -> zbus::Result<()>;
    fn pause(&self) -> zbus::Result<()>;
    fn resume(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn last_sync(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn account(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn sync_dir(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn progress(&self) -> zbus::Result<Progress>;
}

/// Connect to a running daemon, or `Ok(None)` if none owns [`BUS_NAME`].
pub async fn connect() -> Result<Option<OneDriveProxy<'static>>> {
    let conn = zbus::Connection::session().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    if !dbus.name_has_owner(BUS_NAME.try_into()?).await? {
        return Ok(None);
    }
    Ok(Some(OneDriveProxy::new(&conn).await?))
}

