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
        return import_library(paths, auth, &d_db, drive_id, sync_dir, name, force).await;
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
    set_config_value(&paths.config(), "sync_dir", &sync_dir.display().to_string())?;

    eprintln!("Imported {} (sync cursor: {}):", d_db.display(), if has_cursor { "yes" } else { "no, a full enumeration will run" });
    for (t, n) in counts {
        eprintln!("  {t:<6} {n}");
    }
    eprintln!("sync_dir set to {} in {}", sync_dir.display(), paths.config().display());
    eprintln!("Keep the D client stopped from now on: two clients must never share a sync folder.");
    Ok(())
}

/// A D client instance dedicated to one drive (a SharePoint library): merge its
/// rows into our database and add a `[drive]` section to our config.
async fn import_library(
    paths: &Paths,
    auth: AuthConfig,
    d_db: &Path,
    drive_id: String,
    sync_dir: PathBuf,
    name: Option<String>,
    force: bool,
) -> Result<()> {
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
        config::append_drive_section(&paths.config(), &config::DriveConfig { name: name.clone(), drive_id: drive_id.clone(), sync_dir: sync_dir.clone() })?;
    }
    let kind = drive.drive_type.as_deref().unwrap_or("?");
    eprintln!("Imported {rows} items of {kind} drive \"{name}\" ({drive_id}), sync cursor: {}", if has_cursor { "yes" } else { "no" });
    eprintln!("Folder: {} (added as [drive \"{name}\"] in {})", sync_dir.display(), paths.config().display());
    eprintln!("Keep that D client stopped from now on, then restart onedrive-rs.");
    Ok(())
}

/// Replace `key = "..."` in a D-style config file, or append it.
fn set_config_value(path: &Path, key: &str, value: &str) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let line = format!("{key} = \"{value}\"");
    let mut found = false;
    let mut in_section = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            in_section |= l.trim_start().starts_with('[');
            let is_key = !in_section && l.trim_start().split('=').next().is_some_and(|k| k.trim() == key);
            if is_key && !found {
                found = true;
                line.clone()
            } else {
                l.to_owned()
            }
        })
        .collect();
    if !found {
        // Top-level keys must stay before the first [drive] section.
        let at = out.iter().position(|l| l.trim_start().starts_with('[')).unwrap_or(out.len());
        out.insert(at, line);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, out.join("\n") + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_value_is_replaced_or_appended() {
        let p = std::env::temp_dir().join(format!("odrs-cfg-test-{}", std::process::id()));
        std::fs::write(&p, "# mine\nthreads = \"4\"\nsync_dir = \"/old\"\n").unwrap();
        set_config_value(&p, "sync_dir", "/new").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "# mine\nthreads = \"4\"\nsync_dir = \"/new\"\n");
        std::fs::write(&p, "threads = \"4\"\n").unwrap();
        set_config_value(&p, "sync_dir", "/x").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "threads = \"4\"\nsync_dir = \"/x\"\n");
        std::fs::write(&p, "threads = \"4\"\n[drive \"L\"]\nsync_dir = \"/l\"\n").unwrap();
        set_config_value(&p, "sync_dir", "/x").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "threads = \"4\"\nsync_dir = \"/x\"\n[drive \"L\"]\nsync_dir = \"/l\"\n");
        std::fs::remove_file(&p).unwrap();
    }
}
