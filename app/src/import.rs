//! `onedrive-rs import-d`: take over a D client installation without
//! downloading anything again. Both clients use the same database schema and
//! the same sync cursor (deltaLink on the root row), so the D database can be
//! copied as is and the D sync folder reused.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use odrs_graph::{AuthConfig, Graph};
use odrs_store::Store;

use crate::Paths;
use crate::config::{self, Config};

pub async fn run(paths: &Paths, auth: AuthConfig, from: PathBuf, name: Option<String>, force: bool) -> Result<()> {
    let _lock = crate::lock(paths)?;
    let d_db = from.join("items.sqlite3");
    if !d_db.exists() {
        bail!("no D client database at {}", d_db.display());
    }
    let d_cfg = Config::load(&from.join("config"))?;
    if let Some(drive_id) = d_cfg.drive_id.clone() {
        let sync_dir = d_cfg.sync_dir.clone().context("the D configuration has a drive_id but no sync_dir")?;
        let library = Library { from: &from, drive_id, sync_dir, filters: d_cfg.filters.clone() };
        return import_library(paths, auth, &d_db, library, name, force).await;
    }
    let sync_dir = d_cfg.sync_dir.clone().unwrap_or_else(|| config::expand_home("~/OneDrive"));

    // Our login must be for the same drive as the imported database.
    let graph = Graph::new(odrs_graph::http_client()?, auth, paths.token_store()).context("log in with `onedrive-rs login` first")?;
    let drive = graph.me_drive().await?;

    let ours = paths.db();
    let staging = paths.config_dir.join("items.sqlite3.import");
    let _ = std::fs::remove_file(&staging);
    let imported = Store::import(&d_db, &staging).map_err(|e| match e {
        odrs_store::Error::Sqlite(ref inner) if inner.to_string().contains("locked") => {
            anyhow::anyhow!("the D client database is locked: stop the D client first ({e})")
        }
        other => other.into(),
    })?;
    let root = imported.root(&drive.id)?;
    let has_cursor = match &root {
        Some(r) => imported.delta_link(&r.drive_id, &r.id)?.is_some(),
        None => false,
    };
    let counts = imported.count_by_type()?;
    drop(imported);
    if root.is_none() {
        let _ = std::fs::remove_file(&staging);
        bail!("the D database does not describe drive {} (the account logged in with onedrive-rs)", drive.id);
    }

    if ours.exists() {
        if !force {
            let _ = std::fs::remove_file(&staging);
            bail!("{} already exists; use --force to replace it (it is kept as a .bak file)", ours.display());
        }
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let backup = ours.with_extension(format!("sqlite3.bak-{stamp}"));
        std::fs::rename(&ours, &backup)?;
        for ext in ["sqlite3-wal", "sqlite3-shm"] {
            let _ = std::fs::remove_file(ours.with_extension(ext));
        }
        eprintln!("Previous database kept as {}", backup.display());
    }
    std::fs::rename(&staging, &ours)?;
    config::set_value(&paths.config(), None, "sync_dir", sync_dir.display().to_string())?;
    // The imported database reflects D's filters: keep them.
    for (key, value) in d_cfg.filters.entries() {
        config::set_value(&paths.config(), None, key, value.clone())?;
        eprintln!("{key} = \"{value}\" taken from the D configuration");
    }
    import_sync_list(&from, &config::sync_list_path(&paths.config_dir, "main"))?;

    eprintln!("Imported {} (sync cursor: {}):", d_db.display(), if has_cursor { "yes" } else { "no, a full enumeration will run" });
    for (t, n) in counts {
        eprintln!("  {t:<6} {n}");
    }
    eprintln!("sync_dir set to {} in {}", sync_dir.display(), paths.config().display());
    eprintln!("Keep the D client stopped from now on: two clients must never share a sync folder.");
    Ok(())
}

/// The D configuration of a library instance.
struct Library<'a> {
    from: &'a Path,
    drive_id: String,
    sync_dir: PathBuf,
    filters: config::FilterConfig,
}

/// A D client instance dedicated to one drive (a SharePoint library): merge its
/// rows into our database and add a `[drive]` section to our config.
async fn import_library(paths: &Paths, auth: AuthConfig, d_db: &Path, library: Library<'_>, name: Option<String>, force: bool) -> Result<()> {
    let Library { from, drive_id, sync_dir, filters } = library;
    let name = name
        .or_else(|| sync_dir.file_name().map(|n| n.to_string_lossy().into_owned()))
        .context("cannot derive a drive name; use --name")?;
    let ours = Config::load(&paths.config())?;
    let already = ours.drives.iter().any(|d| d.drive_id == drive_id);
    if already && !force {
        bail!("drive {drive_id} is already configured; use --force to import it again");
    }
    if !already {
        let main_dir = ours.sync_dir.clone().unwrap_or_else(|| config::expand_home("~/OneDrive-rs"));
        config::validate_new_drive(&ours, &main_dir, &name, &drive_id, &sync_dir)?;
    }

    // Our login must be able to reach the library.
    let graph = Graph::new(odrs_graph::http_client()?, auth, paths.token_store()).context("log in with `onedrive-rs login` first")?;
    let drive = graph.drive(&drive_id).await.with_context(|| format!("cannot access drive {drive_id} with the onedrive-rs login"))?;

    std::fs::create_dir_all(&paths.config_dir)?;
    let store = Store::open(&paths.db())?;
    let rows = store.merge_drive(d_db, &drive_id).map_err(|e| match e {
        odrs_store::Error::Sqlite(ref inner) if inner.to_string().contains("locked") => {
            anyhow::anyhow!("the D client database is locked: stop that D client first ({e})")
        }
        other => other.into(),
    })?;
    let root = store.root(&drive_id)?.context("the D database has no root for this drive")?;
    let has_cursor = store.delta_link(&root.drive_id, &root.id)?.is_some();

    if !already {
        let section = config::DriveConfig { name: name.clone(), drive_id: drive_id.clone(), sync_dir: sync_dir.clone(), filters };
        config::append_drive_section(&paths.config(), &section)?;
        import_sync_list(from, &config::sync_list_path(&paths.config_dir, &name))?;
    }
    let kind = drive.drive_type.as_deref().unwrap_or("?");
    eprintln!("Imported {rows} items of {kind} drive \"{name}\" ({drive_id}), sync cursor: {}", if has_cursor { "yes" } else { "no" });
    eprintln!("Folder: {} (added as [drive \"{name}\"] in {})", sync_dir.display(), paths.config().display());
    eprintln!("Keep that D client stopped from now on, then restart onedrive-rs.");
    Ok(())
}

/// Copy D's selective sync rules (the imported database reflects them), unless
/// we already have some.
fn import_sync_list(from: &Path, to: &Path) -> Result<()> {
    let source = from.join("sync_list");
    if !source.exists() {
        return Ok(());
    }
    if to.exists() {
        eprintln!("{} not copied: {} already exists", source.display(), to.display());
        return Ok(());
    }
    std::fs::copy(&source, to).with_context(|| format!("copying {}", source.display()))?;
    eprintln!("Selective sync rules copied to {}", to.display());
    Ok(())
}
