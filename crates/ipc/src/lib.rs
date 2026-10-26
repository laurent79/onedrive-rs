//! D-Bus interface `io.github.onedrive_rs.OneDrive1` between the daemon (server) and its
//! clients (GUI, CLI). The daemon owns all sync I/O; clients only observe and
//! send commands.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::{OwnedValue, Type, Value};

pub const BUS_NAME: &str = "io.github.onedrive_rs.OneDrive1";
pub const OBJECT_PATH: &str = "/io/github/onedrive_rs/OneDrive1";
/// `LastError` starts with this when the stored login is no longer valid.
pub const AUTH_ERROR_PREFIX: &str = "[login required]";
/// Error messages start with this when the network is down (retried shortly):
/// clients show them but do not raise a notification.
pub const OFFLINE_ERROR_PREFIX: &str = "[offline]";
/// Events kept in the `Events` property.
pub const MAX_EVENTS: usize = 100;

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
    pub uploaded: u64,
    pub deleted_online: u64,
    /// Size of the whole drive online, 0 if unknown.
    pub drive_bytes: u64,
    /// Bytes synced locally so far (at most `drive_bytes`).
    pub synced_bytes: u64,
    /// Uploads started in this pass: files and bytes (`uploaded` counts the finished files).
    pub upload_files: u64,
    pub upload_bytes: u64,
    /// Bytes of those uploads OneDrive has accepted so far.
    pub upload_sent: u64,
}

/// State of one synchronised drive, in the `Drives` property.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct DriveStatus {
    /// `main` for the account's own OneDrive, else the `[drive]` section name.
    pub name: String,
    /// Empty for `main`.
    pub drive_id: String,
    pub sync_dir: String,
    /// "idle" | "syncing" | "error" | "pending" (not synced yet since the daemon started)
    pub state: String,
    /// RFC 3339 time of its last successful pass, empty if none.
    pub last_sync: String,
    pub last_error: String,
    /// Counters of its current or last pass.
    pub progress: Progress,
    /// Items its last pass could not sync (at most 50).
    pub failures: Vec<ItemFailure>,
    /// safeBackup copies still present: files changed on both sides.
    pub conflicts: Vec<Conflict>,
}

/// An item a pass could not sync.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct ItemFailure {
    /// Absolute local path, or the online name when it has no local place yet.
    pub path: String,
    pub message: String,
}

/// A file changed on both sides: the online version is at `path`, the local
/// one was kept aside as `backup` (absolute paths).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct Conflict {
    pub path: String,
    pub backup: String,
}

/// Integrity check of one drive, in the `Verifications` property.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct VerifyStatus {
    /// Drive name, as in [`DriveStatus::name`].
    pub name: String,
    /// "queued" | "running" | "done" | "cancelled" | "failed"
    pub state: String,
    /// Files to check, checked so far, and bytes read.
    pub total: u64,
    pub done: u64,
    pub bytes: u64,
    pub identical: u64,
    /// Other content but the recorded mtime: in sync (e.g. Office files enriched by SharePoint).
    pub same_mtime: u64,
    pub modified: u64,
    pub missing: u64,
    /// Unreadable files.
    pub errors: u64,
    /// RFC 3339 time it ended, empty while queued or running.
    pub finished: String,
    /// Why it failed.
    pub message: String,
}

impl VerifyStatus {
    /// Files that need attention.
    pub fn problems(&self) -> u64 {
        self.modified + self.missing + self.errors
    }
}

/// A file that needs attention after a check, from `VerifyReport`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct VerifyFinding {
    /// "modified" | "missing" | "error"
    pub kind: String,
    /// Absolute local path.
    pub path: String,
    /// Hashes for a modified file, the error for an unreadable one.
    pub detail: String,
}

/// Something that happened in the daemon, in the `Events` property, for
/// notifications (and later an activity list).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct Event {
    /// Increasing; starts from the daemon's start time in milliseconds, so ids
    /// keep increasing across restarts and clients can tell new events apart.
    pub id: u64,
    /// RFC 3339.
    pub time: String,
    /// "conflict" (path: the file, detail: its safeBackup copy) |
    /// "initial-sync" (a drive enumerated as a whole was synced: first sync, resync)
    pub kind: String,
    /// Drive name, as in [`DriveStatus::name`].
    pub drive: String,
    pub path: String,
    pub detail: String,
}

/// A SharePoint site, from `SearchSites`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Site {
    pub id: String,
    pub name: String,
    pub web_url: String,
}

/// A document library (drive) of a site, from `ListLibraries`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Library {
    pub drive_id: String,
    pub name: String,
    pub web_url: String,
}

/// A subfolder in a drive, from `ListFolders`, with its selective sync state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Folder {
    pub name: String,
    /// Relative to the drive's sync folder, `/`-separated.
    pub path: String,
    /// Synced (its files and the subfolders no rule targets).
    pub included: bool,
    /// A rule targets something below it: its subfolders differ.
    pub mixed: bool,
    pub has_children: bool,
}

/// A folder as left by the user in the folder chooser, for `SetSelection`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct FolderChoice {
    pub path: String,
    pub included: bool,
    /// Untouched while [`Folder::mixed`]: the rules below it are kept.
    pub mixed: bool,
    /// Its subfolders were listed, and are among the choices.
    pub expanded: bool,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Drive management offered by the daemon. Answers do not wait for the sync
/// loop, so they stay responsive during a long pass. Errors are user-facing messages.
pub trait DriveManager: Send + Sync + 'static {
    /// Sites matching `query` (empty: all sites the account can see).
    fn search_sites(&self, query: String) -> BoxFuture<'_, std::result::Result<Vec<Site>, String>>;
    fn list_libraries(&self, site_id: String) -> BoxFuture<'_, std::result::Result<Vec<Library>, String>>;
    /// Add a `[drive]` to the configuration; it is synced from the next pass.
    fn add_drive(&self, name: String, drive_id: String, sync_dir: String) -> std::result::Result<(), String>;
    /// Stop syncing a `[drive]`; its local files are kept.
    fn remove_drive(&self, name: String) -> std::result::Result<(), String>;
    /// Forget what the database knows about a drive, so that the next pass
    /// rebuilds it from the online state (D's `--resync`).
    fn resync(&self, name: String) -> std::result::Result<(), String>;
    /// Queue an integrity check of a drive (empty name: every drive).
    fn verify(&self, name: String) -> std::result::Result<(), String>;
    /// Stop the queued or running check of a drive (empty name: all).
    fn cancel_verify(&self, name: String) -> std::result::Result<(), String>;
    /// Files that need attention after the last check of a drive.
    fn verify_report(&self, name: String) -> std::result::Result<Vec<VerifyFinding>, String>;
    /// Online subfolders of `path` in a drive (empty: its top), with their state.
    fn list_folders(&self, name: String, path: String) -> BoxFuture<'_, std::result::Result<Vec<Folder>, String>>;
    /// Selective sync rules of a drive the folder chooser cannot show (kept as written).
    fn custom_rules(&self, name: String) -> std::result::Result<Vec<String>, String>;
    /// Sync only the chosen folders of a drive (every folder: no selective
    /// sync). `delete_local`: also delete the local copies of what is no longer
    /// synced (unchanged files only); otherwise they are kept.
    fn set_selection(&self, name: String, choices: Vec<FolderChoice>, delete_local: bool) -> std::result::Result<(), String>;
    /// Settle a conflict of a drive: keep the local version (`backup` replaces
    /// the file) or the online one (`backup` goes). The version set aside goes
    /// to the desktop trash.
    fn resolve_conflict(&self, name: String, backup: String, keep_local: bool) -> std::result::Result<(), String>;
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
    drives: Vec<DriveStatus>,
    verifications: Vec<VerifyStatus>,
    events: Vec<Event>,
    commands: mpsc::UnboundedSender<Command>,
    manager: Arc<dyn DriveManager>,
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

    async fn search_sites(&self, query: String) -> zbus::fdo::Result<Vec<Site>> {
        self.manager.search_sites(query).await.map_err(zbus::fdo::Error::Failed)
    }

    async fn list_libraries(&self, site_id: String) -> zbus::fdo::Result<Vec<Library>> {
        self.manager.list_libraries(site_id).await.map_err(zbus::fdo::Error::Failed)
    }

    async fn add_drive(&self, name: String, drive_id: String, sync_dir: String) -> zbus::fdo::Result<()> {
        self.manager.add_drive(name, drive_id, sync_dir).map_err(zbus::fdo::Error::Failed)?;
        self.send(Command::SyncNow)
    }

    async fn remove_drive(&self, name: String) -> zbus::fdo::Result<()> {
        self.manager.remove_drive(name).map_err(zbus::fdo::Error::Failed)?;
        self.send(Command::SyncNow)
    }

    /// Rebuild a drive's database from the online state, keeping local files:
    /// identical ones are only recorded, differing ones are kept as safeBackup
    /// before the online version is downloaded. Runs before the next pass, which starts now.
    async fn resync(&self, name: String) -> zbus::fdo::Result<()> {
        self.manager.resync(name).map_err(zbus::fdo::Error::Failed)?;
        self.send(Command::SyncNow)
    }

    /// Check the local files of a drive against the database (empty name: every
    /// drive), next to the sync passes. Read-only.
    async fn verify(&self, name: String) -> zbus::fdo::Result<()> {
        self.manager.verify(name).map_err(zbus::fdo::Error::Failed)
    }

    async fn cancel_verify(&self, name: String) -> zbus::fdo::Result<()> {
        self.manager.cancel_verify(name).map_err(zbus::fdo::Error::Failed)
    }

    async fn verify_report(&self, name: String) -> zbus::fdo::Result<Vec<VerifyFinding>> {
        self.manager.verify_report(name).map_err(zbus::fdo::Error::Failed)
    }

    /// Online subfolders of `path` in a drive, with their selective sync state.
    async fn list_folders(&self, name: String, path: String) -> zbus::fdo::Result<Vec<Folder>> {
        self.manager.list_folders(name, path).await.map_err(zbus::fdo::Error::Failed)
    }

    async fn custom_rules(&self, name: String) -> zbus::fdo::Result<Vec<String>> {
        self.manager.custom_rules(name).map_err(zbus::fdo::Error::Failed)
    }

    /// Settle a conflict (see [`DriveManager::resolve_conflict`]); a pass starts now.
    async fn resolve_conflict(&self, name: String, backup: String, keep_local: bool) -> zbus::fdo::Result<()> {
        self.manager.resolve_conflict(name, backup, keep_local).map_err(zbus::fdo::Error::Failed)?;
        self.send(Command::SyncNow)
    }

    /// Change which folders of a drive are synced; applied by a pass that starts now.
    async fn set_selection(&self, name: String, choices: Vec<FolderChoice>, delete_local: bool) -> zbus::fdo::Result<()> {
        self.manager.set_selection(name, choices, delete_local).map_err(zbus::fdo::Error::Failed)?;
        self.send(Command::SyncNow)
    }

    /// Recent events, oldest first (at most [`MAX_EVENTS`]).
    #[zbus(property)]
    async fn events(&self) -> Vec<Event> {
        self.events.clone()
    }

    /// Last or current integrity check of each drive that had one.
    #[zbus(property)]
    async fn verifications(&self) -> Vec<VerifyStatus> {
        self.verifications.clone()
    }

    /// One entry per synchronised drive, `main` first.
    #[zbus(property)]
    async fn drives(&self) -> Vec<DriveStatus> {
        self.drives.clone()
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
    pub async fn start(info: Info, commands: mpsc::UnboundedSender<Command>, manager: Arc<dyn DriveManager>) -> Result<Self> {
        let service = Service {
            info,
            state: State::Idle,
            last_sync: String::new(),
            last_error: String::new(),
            progress: Progress::default(),
            drives: Vec::new(),
            verifications: Vec::new(),
            events: Vec::new(),
            commands,
            manager,
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

    pub async fn set_drives(&self, drives: Vec<DriveStatus>) -> Result<()> {
        let mut s = self.iface.get_mut().await;
        if s.drives != drives {
            s.drives = drives;
            s.drives_changed(self.emitter()).await?;
        }
        Ok(())
    }

    /// Publish events; their ids must already be set (see [`Event::id`]).
    pub async fn push_events(&self, events: Vec<Event>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut s = self.iface.get_mut().await;
        s.events.extend(events);
        let excess = s.events.len().saturating_sub(MAX_EVENTS);
        s.events.drain(..excess);
        s.events_changed(self.emitter()).await
    }

    pub async fn set_verifications(&self, verifications: Vec<VerifyStatus>) -> Result<()> {
        let mut s = self.iface.get_mut().await;
        if s.verifications != verifications {
            s.verifications = verifications;
            s.verifications_changed(self.emitter()).await?;
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
    fn search_sites(&self, query: &str) -> zbus::Result<Vec<Site>>;
    fn list_libraries(&self, site_id: &str) -> zbus::Result<Vec<Library>>;
    fn add_drive(&self, name: &str, drive_id: &str, sync_dir: &str) -> zbus::Result<()>;
    fn remove_drive(&self, name: &str) -> zbus::Result<()>;
    fn resync(&self, name: &str) -> zbus::Result<()>;
    fn verify(&self, name: &str) -> zbus::Result<()>;
    fn cancel_verify(&self, name: &str) -> zbus::Result<()>;
    fn verify_report(&self, name: &str) -> zbus::Result<Vec<VerifyFinding>>;
    fn list_folders(&self, name: &str, path: &str) -> zbus::Result<Vec<Folder>>;
    fn custom_rules(&self, name: &str) -> zbus::Result<Vec<String>>;
    fn set_selection(&self, name: &str, choices: &[FolderChoice], delete_local: bool) -> zbus::Result<()>;
    fn resolve_conflict(&self, name: &str, backup: &str, keep_local: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn verifications(&self) -> zbus::Result<Vec<VerifyStatus>>;
    #[zbus(property)]
    fn events(&self) -> zbus::Result<Vec<Event>>;
    #[zbus(property)]
    fn drives(&self) -> zbus::Result<Vec<DriveStatus>>;
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

/// Everything a client displays, read in one `GetAll` call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub state: String,
    pub last_sync: String,
    pub last_error: String,
    pub account: String,
    pub sync_dir: String,
    pub progress: Progress,
    pub drives: Vec<DriveStatus>,
    pub verifications: Vec<VerifyStatus>,
    pub events: Vec<Event>,
}

/// Session-bus client usable whether or not the daemon is running (it may
/// start or stop at any time).
#[derive(Clone)]
pub struct Client {
    dbus: zbus::fdo::DBusProxy<'static>,
    props: zbus::fdo::PropertiesProxy<'static>,
    proxy: OneDriveProxy<'static>,
}

impl Client {
    pub async fn new() -> Result<Self> {
        let conn = zbus::Connection::session().await?;
        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
        let props = zbus::fdo::PropertiesProxy::builder(&conn)
            .destination(BUS_NAME)?
            .path(OBJECT_PATH)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        let proxy = OneDriveProxy::builder(&conn).cache_properties(zbus::proxy::CacheProperties::No).build().await?;
        Ok(Self { dbus, props, proxy })
    }

    pub async fn is_running(&self) -> Result<bool> {
        Ok(self.dbus.name_has_owner(BUS_NAME.try_into()?).await?)
    }

    /// Current daemon state, or `None` if the daemon is not running.
    pub async fn snapshot(&self) -> Result<Option<Snapshot>> {
        if !self.is_running().await? {
            return Ok(None);
        }
        let iface = zbus::names::InterfaceName::from_static_str_unchecked(BUS_NAME);
        let mut all = self.props.get_all(iface).await?;
        let mut text = |k: &str| all.remove(k).and_then(|v| String::try_from(v).ok()).unwrap_or_default();
        let (state, last_sync, last_error, account, sync_dir) =
            (text("State"), text("LastSync"), text("LastError"), text("Account"), text("SyncDir"));
        let progress = all.remove("Progress").and_then(|v| Progress::try_from(v).ok()).unwrap_or_default();
        let drives = all.remove("Drives").and_then(|v| Vec::<DriveStatus>::try_from(v).ok()).unwrap_or_default();
        let verifications =
            all.remove("Verifications").and_then(|v| Vec::<VerifyStatus>::try_from(v).ok()).unwrap_or_default();
        let events = all.remove("Events").and_then(|v| Vec::<Event>::try_from(v).ok()).unwrap_or_default();
        Ok(Some(Snapshot { state, last_sync, last_error, account, sync_dir, progress, drives, verifications, events }))
    }

    /// Method calls (`sync_now`, `pause`, `resume`, drive management).
    pub fn daemon(&self) -> &OneDriveProxy<'static> {
        &self.proxy
    }
}

