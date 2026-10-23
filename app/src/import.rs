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

pub async fn run(paths: &Paths, auth: AuthConfig, from: PathBuf, force: bool) -> Result<()> {
    let _lock = crate::lock(paths)?;
    let d_db = from.join("items.sqlite3");
    if !d_db.exists() {
        bail!("no D client database at {}", d_db.display());
    }
    let d_cfg = Config::load(&from.join("config"))?;
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

/// Replace `key = "..."` in a D-style config file, or append it.
fn set_config_value(path: &Path, key: &str, value: &str) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let line = format!("{key} = \"{value}\"");
    let mut found = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            let is_key = l.trim_start().split('=').next().is_some_and(|k| k.trim() == key);
            if is_key && !found {
                found = true;
                line.clone()
            } else {
                l.to_owned()
            }
        })
        .collect();
    if !found {
        out.push(line);
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
        std::fs::remove_file(&p).unwrap();
    }
}
