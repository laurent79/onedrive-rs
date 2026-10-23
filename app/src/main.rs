use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use odrs_engine::{Stats, SyncOptions};
use odrs_graph::{AuthConfig, Graph, LoopbackLogin, ManualLogin, TokenStore};
use odrs_store::Store;

mod daemon;

#[derive(Parser)]
#[command(name = "onedrive-rs", version, about = "Microsoft OneDrive client for Linux")]
struct Cli {
    /// Configuration directory (refresh token, database).
    #[arg(long, global = true, env = "ONEDRIVE_RS_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    /// Entra application (client) ID.
    #[arg(long, global = true, default_value = odrs_graph::auth::DEFAULT_CLIENT_ID)]
    client_id: String,
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
        /// Only apply online changes locally (the only mode implemented so far).
        #[arg(long)]
        download_only: bool,
        /// Local folder to synchronise.
        #[arg(long, env = "ONEDRIVE_RS_SYNC_DIR")]
        sync_dir: Option<PathBuf>,
        /// Maximum concurrent downloads.
        #[arg(long, default_value_t = 8)]
        transfers: usize,
    },
    /// Show account and database status.
    Status,
    /// Hash every file recorded in a database and compare with a local tree.
    #[command(hide = true)]
    Verify {
        /// Database to read (default: our own). Copy a locked D client DB first.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Local root the database describes (default: our sync dir).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Stop after this many files.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Run as a background service: periodic sync passes, controlled over D-Bus.
    Daemon {
        /// Local folder to synchronise.
        #[arg(long, env = "ONEDRIVE_RS_SYNC_DIR")]
        sync_dir: Option<PathBuf>,
        /// Maximum concurrent downloads.
        #[arg(long, default_value_t = 8)]
        transfers: usize,
        /// Seconds between two sync passes.
        #[arg(long, default_value_t = 300)]
        interval: u64,
    },
    /// Ask the running daemon to sync now.
    SyncNow,
    /// Pause the running daemon (cancels the current pass).
    Pause,
    /// Resume the running daemon.
    Resume,
    /// Open the graphical interface (not implemented yet).
    Gui,
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
    fn lock(&self) -> PathBuf {
        self.config_dir.join("lock")
    }
}

fn default_sync_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home directory")?.join("OneDrive-rs"))
}

#[tokio::main]
async fn main() -> Result<()> {
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
    let auth = AuthConfig { client_id: cli.client_id, read_only: false };
    match cli.command {
        Command::Login { manual, read_only } => login(&paths, AuthConfig { read_only, ..auth }, manual).await,
        Command::Sync { download_only, sync_dir, transfers } => {
            if !download_only {
                bail!("only --download-only is implemented for now");
            }
            let sync_dir = match sync_dir {
                Some(d) => d,
                None => default_sync_dir()?,
            };
            sync(&paths, auth, sync_dir, transfers).await
        }
        Command::Status => status(&paths, auth).await,
        Command::Verify { db, dir, limit } => {
            let dir = match dir {
                Some(d) => d,
                None => default_sync_dir()?,
            };
            verify(&db.unwrap_or_else(|| paths.db()), &dir, limit)
        }
        Command::Daemon { sync_dir, transfers, interval } => {
            let sync_dir = match sync_dir {
                Some(d) => d,
                None => default_sync_dir()?,
            };
            daemon::run(&paths, auth, sync_dir, transfers, Duration::from_secs(interval)).await
        }
        Command::SyncNow | Command::Pause | Command::Resume => {
            let proxy = odrs_ipc::connect().await?.context("the daemon is not running")?;
            match cli.command {
                Command::SyncNow => proxy.sync_now().await?,
                Command::Pause => proxy.pause().await?,
                _ => proxy.resume().await?,
            }
            Ok(())
        }
        Command::Gui => bail!("not implemented yet"),
    }
}

async fn login(paths: &Paths, auth: AuthConfig, manual: bool) -> Result<()> {
    let http = odrs_graph::http_client()?;
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
    let graph = Graph::new(http, AuthConfig::default(), paths.token_store())?;
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

async fn sync(paths: &Paths, auth: AuthConfig, sync_dir: PathBuf, transfers: usize) -> Result<()> {
    let _lock = lock(paths)?;
    let graph = Arc::new(Graph::new(odrs_graph::http_client()?, auth, paths.token_store())?);
    let store = Store::open(&paths.db())?;
    let stats = Arc::new(Stats::default());
    let opts = SyncOptions { sync_dir, max_transfers: transfers, device_name: odrs_engine::local::device_name() };

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
        r = odrs_engine::sync_download_only(graph, &store, opts, stats.clone()) => r.map_err(anyhow::Error::from),
        _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("interrupted")),
    };
    progress.abort();
    print_stats(&stats, started, "done");
    result
}

fn print_stats(s: &Stats, started: Instant, label: &str) {
    let g = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    tracing::info!(
        "{label}: {:.0?} elapsed, {} pages, {} items, {} downloaded ({:.1} MiB), {} unchanged, {} folders created, {} deleted, {} skipped, {} errors",
        started.elapsed(),
        g(&s.pages),
        g(&s.items),
        g(&s.downloaded),
        g(&s.bytes) as f64 / (1024.0 * 1024.0),
        g(&s.unchanged),
        g(&s.dirs_created),
        g(&s.deleted),
        g(&s.skipped),
        g(&s.errors),
    );
}

async fn status(paths: &Paths, auth: AuthConfig) -> Result<()> {
    println!("Config dir: {}", paths.config_dir.display());
    // While the daemon runs it owns the tokens: ask it instead of refreshing them here.
    let daemon = match odrs_ipc::connect().await {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(error = %e, "no session bus");
            None
        }
    };
    if let Some(d) = daemon {
        let p = d.progress().await?;
        println!("Daemon:     {} (sync dir {})", d.state().await?, d.sync_dir().await?);
        println!("Account:    {}", d.account().await?);
        println!("Last sync:  {}", Some(d.last_sync().await?).filter(|s| !s.is_empty()).unwrap_or_else(|| "never".into()));
        let err = d.last_error().await?;
        if !err.is_empty() {
            println!("Last error: {err}");
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

fn verify(db: &Path, dir: &Path, limit: Option<usize>) -> Result<()> {
    let store = Store::open(db)?;
    let (mut ok, mut missing, mut mismatched, mut no_hash) = (0usize, 0usize, 0usize, 0usize);
    let mut files = Vec::new();
    store.for_each_file(|item| files.push(item))?;
    for item in files.into_iter().take(limit.unwrap_or(usize::MAX)) {
        let Some(rel) = store.path_of(&item.drive_id, &item.id)? else { continue };
        let Some(expected) = &item.quick_xor_hash else {
            no_hash += 1;
            continue;
        };
        let path = dir.join(&rel);
        match odrs_quickxor::hash_file(&path) {
            Ok(h) if &h == expected => ok += 1,
            Ok(h) => {
                mismatched += 1;
                println!("MISMATCH {rel}: local {h}, db {expected}");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing += 1;
                println!("MISSING  {rel}");
            }
            Err(e) => return Err(e).with_context(|| format!("hashing {}", path.display())),
        }
    }
    println!("{ok} identical, {mismatched} different, {missing} missing, {no_hash} without QuickXorHash");
    Ok(())
}
