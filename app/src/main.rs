use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use odrs_engine::{Stats, SyncOptions};
use odrs_graph::{AuthConfig, Graph, LoopbackLogin, ManualLogin, TokenStore};
use odrs_store::Store;

mod config;
mod daemon;
mod import;
mod verifier;
mod verify;

#[derive(Parser)]
#[command(name = "onedrive-rs", version, about = "Microsoft OneDrive client for Linux")]
struct Cli {
    /// Configuration directory (refresh token, database).
    #[arg(long, global = true, env = "ONEDRIVE_RS_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    /// Entra application (client) ID [config: application_id; default: the D client's public app].
    #[arg(long, global = true)]
    client_id: Option<String>,
    /// More logging (-v debug, -vv trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Authenticate with Microsoft and store a refresh token.
    Login {
        /// Paste the redirect URL instead of using a local loopback listener.
        #[arg(long)]
        manual: bool,
        /// Request read-only scopes.
        #[arg(long)]
        read_only: bool,
    },
    /// Run one synchronisation pass.
    Sync {
        #[command(flatten)]
        args: SyncArgs,
        /// Same as `--mode download-only`.
        #[arg(long)]
        download_only: bool,
        /// Report what would be uploaded/deleted online without changing anything
        /// (online changes are not fetched).
        #[arg(long)]
        dry_run: bool,
    },
    /// Show account and database status.
    Status,
    /// Hash the files recorded in the database and compare them with the local
    /// folders (read-only; the daemon may keep running).
    Verify {
        /// Only these drives: `main` or a [drive "name"] from the config. Repeatable.
        #[arg(long = "drive", value_name = "NAME", conflicts_with = "db")]
        drives: Vec<String>,
        /// Another database, e.g. a copy of a D client's (it is locked while D runs).
        #[arg(long, requires = "dir")]
        db: Option<PathBuf>,
        /// Local folder that --db describes.
        #[arg(long, requires = "db")]
        dir: Option<PathBuf>,
        /// Check at most this many files per drive.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Rebuild the database of drives from their online state (D's --resync).
    /// Local files are kept: identical ones are only recorded, differing ones
    /// are kept as safeBackup before the online version is downloaded.
    Resync {
        /// `main` or a [drive "name"] from the config. Repeatable.
        #[arg(long = "drive", value_name = "NAME", required = true)]
        drives: Vec<String>,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Run as a background service: periodic sync passes, controlled over D-Bus.
    Daemon {
        #[command(flatten)]
        args: SyncArgs,
        /// Seconds between two sync passes [config: monitor_interval; default 300].
        #[arg(long)]
        interval: Option<u64>,
    },
    /// Take over a D client installation (database, sync cursor and sync folder)
    /// without downloading anything again. Stop the D client first. A D
    /// configuration with a drive_id (SharePoint library) is added as a [drive].
    ImportD {
        /// D client configuration directory.
        #[arg(long, default_value = "~/.config/onedrive")]
        from: String,
        /// Main account: replace an existing onedrive-rs database (kept as a .bak file).
        /// Library: import it again over the existing rows.
        #[arg(long)]
        force: bool,
        /// Name of the [drive] section for a SharePoint library (default: folder name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Ask the running daemon to sync now.
    SyncNow,
    /// Pause the running daemon (cancels the current pass).
    Pause,
    /// Resume the running daemon.
    Resume,
    /// Open the status window and tray icon (talks to the daemon over D-Bus).
    Gui {
        /// Start with only the tray icon.
        #[arg(long)]
        hidden: bool,
    },
}

/// Options shared by `sync` and `daemon`; unset ones come from the config file.
#[derive(clap::Args)]
struct SyncArgs {
    /// Sync direction [config: download_only; default: bidirectional].
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    /// Local folder to synchronise [config: sync_dir; default: ~/OneDrive-rs].
    #[arg(long, env = "ONEDRIVE_RS_SYNC_DIR")]
    sync_dir: Option<PathBuf>,
    /// Maximum concurrent transfers [config: threads; default 8].
    #[arg(long)]
    transfers: Option<usize>,
    /// Only these drives: `main` (the account's OneDrive) or a [drive "name"] from the config. Repeatable.
    #[arg(long = "drive", value_name = "NAME")]
    drives: Vec<String>,
}

/// One drive to synchronise and its folder.
#[derive(Clone)]
pub struct Target {
    pub name: String,
    pub opts: SyncOptions,
}

impl SyncArgs {
    /// The account's own OneDrive (`main`), then the configured libraries.
    fn resolve(self, cfg: &config::Config) -> Result<(Vec<Target>, odrs_engine::SyncMode)> {
        let main_dir = match self.sync_dir.or_else(|| cfg.sync_dir.clone()) {
            Some(d) => d,
            None => default_sync_dir()?,
        };
        let mut targets = vec![Target { name: "main".into(), opts: cfg.sync_options(main_dir) }];
        for d in &cfg.drives {
            let mut opts = cfg.sync_options(d.sync_dir.clone());
            opts.drive_id = Some(d.drive_id.clone());
            targets.push(Target { name: d.name.clone(), opts });
        }
        // Folders must not overlap: one tree would be synced (and deleted) twice.
        for (i, a) in targets.iter().enumerate() {
            for b in &targets[i + 1..] {
                if a.opts.sync_dir.starts_with(&b.opts.sync_dir) || b.opts.sync_dir.starts_with(&a.opts.sync_dir) {
                    bail!("sync folders of \"{}\" and \"{}\" overlap", a.name, b.name);
                }
            }
        }
        if !self.drives.is_empty() {
            if let Some(unknown) = self.drives.iter().find(|n| !targets.iter().any(|t| t.name.eq_ignore_ascii_case(n))) {
                bail!("unknown drive \"{unknown}\" (known: {})", targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "));
            }
            targets.retain(|t| self.drives.iter().any(|n| t.name.eq_ignore_ascii_case(n)));
        }
        for t in &mut targets {
            if let Some(n) = self.transfers {
                t.opts.max_transfers = n;
            }
        }
        let mode = self.mode.unwrap_or(if cfg.download_only == Some(true) { Mode::DownloadOnly } else { Mode::Bidirectional });
        Ok((targets, mode.into()))
    }
}

/// Sync direction.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Mode {
    /// Only apply online changes locally.
    DownloadOnly,
    /// Also upload local changes (new, modified, deleted files and folders).
    Bidirectional,
}

impl From<Mode> for odrs_engine::SyncMode {
    fn from(m: Mode) -> Self {
        match m {
            Mode::DownloadOnly => odrs_engine::SyncMode::DownloadOnly,
            Mode::Bidirectional => odrs_engine::SyncMode::Bidirectional,
        }
    }
}

struct Paths {
    config_dir: PathBuf,
}

impl Paths {
    fn new(config_dir: Option<PathBuf>) -> Result<Self> {
        let config_dir = match config_dir {
            Some(d) => d,
            None => dirs::config_dir().context("no XDG config directory")?.join("onedrive-rs"),
        };
        Ok(Self { config_dir })
    }
    fn token_store(&self) -> TokenStore {
        TokenStore::new(self.config_dir.join("refresh_token"))
    }
    fn db(&self) -> PathBuf {
        self.config_dir.join("items.sqlite3")
    }
    fn config(&self) -> PathBuf {
        self.config_dir.join("config")
    }
    fn lock(&self) -> PathBuf {
        self.config_dir.join("lock")
    }
    /// Interrupted upload sessions, resumed by the next pass.
    fn upload_sessions(&self) -> PathBuf {
        self.config_dir.join("upload_sessions")
    }
}

fn default_sync_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home directory")?.join("OneDrive-rs"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| format!("warn,onedrive_rs={level},odrs_engine={level},odrs_graph={level}").into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let paths = Paths::new(cli.config_dir)?;
    let cfg = config::Config::load(&paths.config())?;
    let client_id = cli
        .client_id
        .or_else(|| cfg.application_id.clone())
        .unwrap_or_else(|| odrs_graph::auth::DEFAULT_CLIENT_ID.to_owned());
    let auth = AuthConfig { client_id, read_only: false };
    let rt = tokio::runtime::Runtime::new()?;
    if let Command::Gui { hidden } = cli.command {
        // The UI event loop owns this thread; async D-Bus work runs on the runtime's workers.
        let login = || std::process::Command::new(std::env::current_exe()?).arg("login").stdin(std::process::Stdio::null()).spawn();
        let opts = odrs_gui::Options { hidden, start_daemon: Box::new(spawn_daemon), login: Box::new(login) };
        return Ok(odrs_gui::run(rt.handle().clone(), opts)?);
    }
    rt.block_on(run(cli.command, paths, cfg, auth))
}

/// Start the daemon for the GUI: through systemd when the user unit is installed
/// (contrib/install.sh), otherwise detached, logging to the XDG state dir.
fn spawn_daemon() -> std::io::Result<()> {
    use std::os::unix::process::CommandExt as _;
    let unit = dirs::config_dir().map(|d| d.join("systemd/user/onedrive-rs.service"));
    if unit.is_some_and(|u| u.exists()) {
        let status = std::process::Command::new("systemctl").args(["--user", "start", "onedrive-rs.service"]).status()?;
        return if status.success() { Ok(()) } else { Err(std::io::Error::other(format!("systemctl failed: {status}"))) };
    }
    let log_dir = dirs::state_dir().unwrap_or_else(std::env::temp_dir).join("onedrive-rs");
    std::fs::create_dir_all(&log_dir)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_dir.join("daemon.log"))?;
    std::process::Command::new(std::env::current_exe()?)
        .arg("daemon")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()?;
    Ok(())
}

async fn run(command: Command, paths: Paths, cfg: config::Config, auth: AuthConfig) -> Result<()> {
    match command {
        Command::Login { manual, read_only } => login(&paths, AuthConfig { read_only, ..auth }, manual).await,
        Command::Sync { mut args, download_only, dry_run } => {
            if download_only {
                args.mode = Some(Mode::DownloadOnly);
            }
            let (mut targets, mode) = args.resolve(&cfg)?;
            for t in &mut targets {
                t.opts.dry_run = dry_run;
                t.opts.upload_sessions = Some(paths.upload_sessions());
            }
            sync(&paths, auth, targets, mode).await
        }
        Command::Status => status(&paths, auth).await,
        Command::Verify { drives, db, dir, limit } => {
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(4));
            if let (Some(db), Some(dir)) = (db, dir) {
                let tree = verify::Tree { name: db.display().to_string(), drive_id: None, dir };
                return verify::run(&Store::open(&db)?, &[tree], limit, threads);
            }
            let (targets, _) = SyncArgs { mode: None, sync_dir: None, transfers: None, drives }.resolve(&cfg)?;
            let store = Store::open(&paths.db())?;
            let libraries: Vec<String> = cfg.drives.iter().map(|d| d.drive_id.clone()).collect();
            verify::run(&store, &verify::trees(&store, &targets, &libraries)?, limit, threads)
        }
        Command::Resync { drives, yes } => resync(&paths, &cfg, drives, yes).await,
        Command::Daemon { args, interval } => {
            let (mut targets, mode) = args.resolve(&cfg)?;
            for t in &mut targets {
                t.opts.upload_sessions = Some(paths.upload_sessions());
            }
            let interval = Duration::from_secs(interval.or(cfg.monitor_interval).unwrap_or(300));
            daemon::run(&paths, auth, cfg.clone(), targets, mode, interval).await
        }
        Command::ImportD { from, force, name } => import::run(&paths, auth, config::expand_home(&from), name, force).await,
        Command::SyncNow | Command::Pause | Command::Resume => {
            let client = odrs_ipc::Client::new().await?;
            if !client.is_running().await? {
                bail!("the daemon is not running");
            }
            match command {
                Command::SyncNow => client.daemon().sync_now().await?,
                Command::Pause => client.daemon().pause().await?,
                _ => client.daemon().resume().await?,
            }
            Ok(())
        }
        Command::Gui { .. } => unreachable!("handled in main"),
    }
}

async fn resync(paths: &Paths, cfg: &config::Config, drives: Vec<String>, yes: bool) -> Result<()> {
    let (targets, _) = SyncArgs { mode: None, sync_dir: None, transfers: None, drives }.resolve(cfg)?;
    let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
    if !yes {
        eprintln!(
            "Resync forgets what onedrive-rs knows about {} and rebuilds it from OneDrive:\n\
             - local files identical to the online ones are kept as they are;\n\
             - differing local files are kept as <name>-<host>-safeBackup-NNNN, then the online version is downloaded;\n\
             - files deleted on one side only since the last sync come back (they cannot be told apart from new ones);\n\
             - local files that are not online are uploaded.\n\
             Type 'yes' to continue:",
            names.join(", ")
        );
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != "yes" {
            bail!("resync not confirmed");
        }
    }
    // Free lock: no daemon uses this config dir, do it here. Otherwise ask the
    // daemon (the session bus may also show a daemon of another config dir).
    let _lock = match lock(paths) {
        Ok(l) => l,
        Err(busy) => {
            let client = odrs_ipc::Client::new().await.map_err(|_| busy)?;
            if !client.is_running().await? {
                bail!("{} is locked, but no daemon answers on the session bus", paths.config_dir.display());
            }
            for name in &names {
                client.daemon().resync(name).await?;
            }
            eprintln!("Resync requested; the daemon rebuilds {} now.", names.join(", "));
            return Ok(());
        }
    };
    let store = Store::open(&paths.db())?;
    let all = SyncArgs { mode: None, sync_dir: None, transfers: None, drives: Vec::new() }.resolve(cfg)?.0;
    for name in &names {
        daemon::reset_drive(&store, &all, name);
    }
    eprintln!("Done; the next `onedrive-rs sync` (or the daemon) rebuilds {}.", names.join(", "));
    Ok(())
}

async fn login(paths: &Paths, auth: AuthConfig, manual: bool) -> Result<()> {
    let http = odrs_graph::http_client()?;
    let client_id = auth.client_id.clone();
    let tokens = if manual {
        let flow = ManualLogin::new(auth);
        eprintln!("Open this URL in a browser, sign in, then paste the URL of the blank page you land on:\n\n{}\n", flow.url());
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        flow.finish(&http, &line).await?
    } else {
        let flow = LoopbackLogin::start(auth).await?;
        eprintln!("Opening the browser for Microsoft sign-in. If it does not open, visit:\n\n{}\n", flow.url());
        if let Err(e) = std::process::Command::new("xdg-open").arg(flow.url()).spawn() {
            tracing::debug!(error = %e, "xdg-open failed");
        }
        tokio::select! {
            r = flow.finish(&http) => r?,
            _ = tokio::time::sleep(Duration::from_secs(600)) => bail!("timed out waiting for the browser sign-in"),
        }
    };
    Graph::save_login(&paths.token_store(), &tokens)?;
    let graph = Graph::new(http, AuthConfig { client_id, read_only: false }, paths.token_store())?;
    match graph.me_drive().await {
        Ok(drive) => {
            let who = drive.owner.and_then(|o| o.user).and_then(|u| u.email.or(u.display_name)).unwrap_or_default();
            eprintln!("Logged in: {who} ({} drive {})", drive.drive_type.unwrap_or_default(), drive.id);
        }
        Err(e) => eprintln!("Logged in, but reading the drive failed: {e}"),
    }
    Ok(())
}

/// Exclusive lock on the config dir, so two instances never share a database.
fn lock(paths: &Paths) -> Result<std::fs::File> {
    std::fs::create_dir_all(&paths.config_dir)?;
    let file = std::fs::File::create(paths.lock())?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => bail!("another onedrive-rs instance is using {}", paths.config_dir.display()),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

async fn sync(paths: &Paths, auth: AuthConfig, targets: Vec<Target>, mode: odrs_engine::SyncMode) -> Result<()> {
    let _lock = lock(paths)?;
    let graph = Arc::new(Graph::new(odrs_graph::http_client()?, auth, paths.token_store())?);
    let store = Store::open(&paths.db())?;
    let mut failed = Vec::new();
    for t in targets {
        tracing::info!(drive = %t.name, sync_dir = %t.opts.sync_dir.display(), "syncing");
        match sync_one(&graph, &store, t.opts, mode).await {
            Ok(()) => {}
            Err(e) if e.is::<Interrupted>() => return Err(e),
            Err(e) => {
                tracing::error!(drive = %t.name, error = %e, "sync failed");
                failed.push(t.name);
            }
        }
    }
    if !failed.is_empty() {
        bail!("sync failed for: {}", failed.join(", "));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("interrupted")]
struct Interrupted;

async fn sync_one(graph: &Arc<Graph>, store: &Store, opts: SyncOptions, mode: odrs_engine::SyncMode) -> Result<()> {
    let graph = graph.clone();
    let stats = Arc::new(Stats::default());
    let started = Instant::now();
    let progress = tokio::spawn({
        let stats = stats.clone();
        async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            tick.tick().await;
            loop {
                tick.tick().await;
                print_stats(&stats, started, "progress");
            }
        }
    });
    let result = tokio::select! {
        r = odrs_engine::sync(graph, store, opts, stats.clone(), mode) => r.map_err(anyhow::Error::from),
        _ = tokio::signal::ctrl_c() => Err(Interrupted.into()),
    };
    progress.abort();
    print_stats(&stats, started, "done");
    result
}

fn print_stats(s: &Stats, started: Instant, label: &str) {
    let g = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    tracing::info!(
        "{label}: {:.0?} elapsed, {} pages, {} items, {} downloaded ({:.1} MiB), {} unchanged, {} folders created, {} deleted, \
         {} uploaded ({:.1} MiB), {} folders created online, {} deleted online, {} moved online, {} conflicts, {} skipped, {} errors",
        started.elapsed(),
        g(&s.pages),
        g(&s.items),
        g(&s.downloaded),
        g(&s.bytes) as f64 / (1024.0 * 1024.0),
        g(&s.unchanged),
        g(&s.dirs_created),
        g(&s.deleted),
        g(&s.uploaded),
        g(&s.uploaded_bytes) as f64 / (1024.0 * 1024.0),
        g(&s.folders_created_online),
        g(&s.deleted_online),
        g(&s.moved_online),
        g(&s.conflicts),
        g(&s.skipped),
        g(&s.errors),
    );
}

async fn status(paths: &Paths, auth: AuthConfig) -> Result<()> {
    println!("Config dir: {}", paths.config_dir.display());
    // While the daemon runs it owns the tokens: ask it instead of refreshing them here.
    let snapshot = match odrs_ipc::Client::new().await {
        Ok(c) => c.snapshot().await?,
        Err(e) => {
            tracing::debug!(error = %e, "no session bus");
            None
        }
    };
    if let Some(d) = snapshot {
        let p = d.progress;
        println!("Daemon:     {} (sync dir {})", d.state, d.sync_dir);
        println!("Account:    {}", d.account);
        println!("Last sync:  {}", if d.last_sync.is_empty() { "never" } else { &d.last_sync });
        if !d.last_error.is_empty() {
            println!("Last error: {}", d.last_error);
        }
        println!(
            "Last pass:  {} items, {} downloaded ({:.1} MiB), {} unchanged, {} deleted, {} skipped, {} errors",
            p.items,
            p.downloaded,
            p.bytes as f64 / (1024.0 * 1024.0),
            p.unchanged,
            p.deleted,
            p.skipped,
            p.errors
        );
    } else {
        println!("Daemon:     not running");
        account_status(auth, paths).await?;
    }
    if paths.db().exists() {
        let store = Store::open(&paths.db())?;
        for (t, n) in store.count_by_type()? {
            println!("DB items:   {t:<6} {n}");
        }
    } else {
        println!("Database:   none yet");
    }
    Ok(())
}

async fn account_status(auth: AuthConfig, paths: &Paths) -> Result<()> {
    match Graph::new(odrs_graph::http_client()?, auth, paths.token_store()) {
        Ok(graph) => {
            let drive = graph.me_drive().await?;
            let who = drive.owner.as_ref().and_then(|o| o.user.as_ref()).and_then(|u| u.email.clone().or(u.display_name.clone()));
            println!("Account:    {} ({} drive {})", who.unwrap_or_default(), drive.drive_type.as_deref().unwrap_or("?"), drive.id);
            if let Some(q) = drive.quota {
                let gib = |v: Option<i64>| v.unwrap_or(0) as f64 / (1u64 << 30) as f64;
                println!("Quota:      {:.1} GiB used / {:.1} GiB", gib(q.used), gib(q.total));
            }
        }
        Err(odrs_graph::Error::NotLoggedIn) => println!("Account:    not logged in"),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
