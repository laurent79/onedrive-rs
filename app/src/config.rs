//! `~/.config/onedrive-rs/config`, in the D client's format: one
//! `key = "value"` per line, `#` comments; `skip_file`/`skip_dir` may repeat.
//! Command-line options override the file.
//!
//! Top-level keys describe the account's own OneDrive. Other drives
//! (SharePoint document libraries) are added as sections, all synced by the
//! same daemon with the same login. A section may set its own filters, which
//! replace the top-level ones for that drive:
//!
//! ```text
//! [drive "Engineering"]
//! drive_id = "b!8mPT…"
//! sync_dir = "~/SharePoint/Engineering"
//! skip_dir = "build"
//! ```
//!
//! Selective sync rules live next to the config file, as in the D client:
//! `sync_list` for the account's OneDrive, `sync_list.<name>` for a drive.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Config {
    pub sync_dir: Option<PathBuf>,
    pub application_id: Option<String>,
    pub threads: Option<usize>,
    pub monitor_interval: Option<u64>,
    pub download_only: Option<bool>,
    pub filters: FilterConfig,
    pub classify_as_big_delete: Option<u64>,
    /// No near-real-time notifications of online changes (D's option name).
    pub disable_websocket_support: Option<bool>,
    pub sync_dir_permissions: Option<u32>,
    pub sync_file_permissions: Option<u32>,
    /// Only in a D client configuration dedicated to one SharePoint library.
    pub drive_id: Option<String>,
    /// Additional drives (`[drive "name"]` sections).
    pub drives: Vec<DriveConfig>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DriveConfig {
    pub name: String,
    pub drive_id: String,
    pub sync_dir: PathBuf,
    /// Set keys replace the top-level ones for this drive.
    pub filters: FilterConfig,
}

/// Client-side filtering options (both directions).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FilterConfig {
    /// Added to the default `skip_file` of the D client.
    pub skip_file: Option<String>,
    pub skip_dir: Option<String>,
    pub skip_dotfiles: Option<bool>,
    /// With a `sync_list`, also sync the files at the top of the sync folder.
    pub sync_root_files: Option<bool>,
}

impl FilterConfig {
    /// Handle a filter key; `false` if `key` is not one.
    fn set(&mut self, key: &str, value: &str, line: usize) -> Result<bool> {
        match key {
            "skip_file" => append(&mut self.skip_file, value),
            "skip_dir" => append(&mut self.skip_dir, value),
            "skip_dotfiles" => self.skip_dotfiles = Some(boolean(value, line)?),
            "sync_root_files" => self.sync_root_files = Some(boolean(value, line)?),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// This drive's keys, the top-level ones for the others.
    fn over(&self, top: &Self) -> Self {
        Self {
            skip_file: self.skip_file.clone().or_else(|| top.skip_file.clone()),
            skip_dir: self.skip_dir.clone().or_else(|| top.skip_dir.clone()),
            skip_dotfiles: self.skip_dotfiles.or(top.skip_dotfiles),
            sync_root_files: self.sync_root_files.or(top.sync_root_files),
        }
    }

    /// The set keys and their values.
    pub fn entries(&self) -> Vec<(&'static str, String)> {
        [("skip_file", self.skip_file.clone()), ("skip_dir", self.skip_dir.clone())]
            .into_iter()
            .chain([("skip_dotfiles", self.skip_dotfiles), ("sync_root_files", self.sync_root_files)].map(|(k, v)| (k, v.map(|b| b.to_string()))))
            .filter_map(|(key, value)| Some((key, value?)))
            .collect()
    }
}

/// Selective sync rules of a drive (`main`: the account's own OneDrive).
pub fn sync_list_path(config_dir: &Path, drive: &str) -> PathBuf {
    if drive == "main" { config_dir.join("sync_list") } else { config_dir.join(format!("sync_list.{drive}")) }
}

impl Config {
    /// Missing file = all defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut c = Self::default();
        // Current [drive "name"] section (drive_id, sync_dir may still be empty) and its header line.
        let mut section: Option<(DriveConfig, usize)> = None;
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                if let Some(s) = section.take() {
                    c.drives.push(finish_section(s)?);
                }
                let name = header
                    .trim()
                    .strip_prefix("drive")
                    .map(str::trim)
                    .and_then(|n| n.strip_prefix('"'))
                    .and_then(|n| n.strip_suffix('"'))
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("line {}: expected [drive \"name\"]", n + 1))?;
                section = Some((DriveConfig { name: name.to_owned(), ..Default::default() }, n + 1));
                continue;
            }
            if let Some((d, _)) = section.as_mut() {
                let Some((key, value)) = line.split_once('=') else {
                    bail!("line {}: expected `key = \"value\"`", n + 1);
                };
                let value = value.trim();
                let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
                match key.trim() {
                    "drive_id" => d.drive_id = value.to_owned(),
                    "sync_dir" => d.sync_dir = expand_home(value),
                    other if d.filters.set(other, value, n + 1)? => {}
                    other => bail!("line {}: `{other}` is not supported in a [drive] section", n + 1),
                }
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                bail!("line {}: expected `key = \"value\"`", n + 1);
            };
            let key = key.trim();
            let value = value.trim();
            let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
            let line_no = n + 1;
            match key {
                "sync_dir" => c.sync_dir = Some(expand_home(value)),
                "application_id" => c.application_id = Some(value.to_owned()).filter(|v| !v.is_empty()),
                "threads" => c.threads = Some(number(value, line_no)?),
                "monitor_interval" => c.monitor_interval = Some(number(value, line_no)?),
                "download_only" => c.download_only = Some(boolean(value, line_no)?),
                other if c.filters.set(other, value, line_no)? => {}
                "classify_as_big_delete" => c.classify_as_big_delete = Some(number(value, line_no)?),
                "disable_websocket_support" => c.disable_websocket_support = Some(boolean(value, line_no)?),
                // Octal digits written as a decimal number, as in the D client ("700").
                "sync_dir_permissions" => c.sync_dir_permissions = Some(octal(value, line_no)?),
                "sync_file_permissions" => c.sync_file_permissions = Some(octal(value, line_no)?),
                // Used by the D client for a SharePoint-only configuration (see `import-d`).
                "drive_id" => c.drive_id = Some(value.to_owned()),
                other => tracing::warn!(key = other, line = line_no, "unsupported config option ignored"),
            }
        }
        if let Some(s) = section.take() {
            c.drives.push(finish_section(s)?);
        }
        let mut names = std::collections::HashSet::new();
        if let Some(d) = c.drives.iter().find(|d| !names.insert(d.name.to_lowercase()) || d.name.eq_ignore_ascii_case("main")) {
            bail!("drive name \"{}\" is used twice (\"main\" is the account's own OneDrive)", d.name);
        }
        Ok(c)
    }

    /// Engine options: defaults, then this file (and the section of `drive`).
    pub fn sync_options(&self, sync_dir: PathBuf, drive: Option<&DriveConfig>) -> odrs_engine::SyncOptions {
        let mut o = odrs_engine::SyncOptions::new(sync_dir);
        if let Some(t) = self.threads {
            o.max_transfers = t;
        }
        let f = drive.map_or_else(|| self.filters.clone(), |d| d.filters.over(&self.filters));
        // Like D, user skip_file entries come on top of the defaults.
        if let Some(extra) = &f.skip_file {
            o.filters.skip_file = odrs_engine::filter::SkipList::new(&format!("{}|{extra}", odrs_engine::filter::DEFAULT_SKIP_FILE));
        }
        if let Some(d) = &f.skip_dir {
            o.filters.skip_dir = odrs_engine::filter::SkipDirs::new(d);
        }
        o.filters.skip_dotfiles = f.skip_dotfiles.unwrap_or(false);
        o.filters.sync_root_files = f.sync_root_files.unwrap_or(false);
        if let Some(n) = self.classify_as_big_delete {
            o.big_delete_threshold = n;
        }
        if let Some(m) = self.sync_dir_permissions {
            o.dir_mode = m;
        }
        if let Some(m) = self.sync_file_permissions {
            o.file_mode = m;
        }
        o
    }
}

/// Checks before adding a drive: unique name and drive, folder not overlapping
/// any configured one (including the main `sync_dir`, `main_dir`).
pub fn validate_new_drive(cfg: &Config, main_dir: &Path, name: &str, drive_id: &str, sync_dir: &Path) -> Result<()> {
    if name.trim().is_empty() || name.contains(['"', '[', ']', '\n', '/']) {
        bail!("invalid drive name \"{name}\"");
    }
    if name.eq_ignore_ascii_case("main") || cfg.drives.iter().any(|d| d.name.eq_ignore_ascii_case(name)) {
        bail!("a drive named \"{name}\" already exists");
    }
    if let Some(d) = cfg.drives.iter().find(|d| d.drive_id == drive_id) {
        bail!("this library is already synced as \"{}\"", d.name);
    }
    if !sync_dir.is_absolute() {
        bail!("the folder must be an absolute path");
    }
    let dirs = std::iter::once(main_dir).chain(cfg.drives.iter().map(|d| d.sync_dir.as_path()));
    if let Some(other) = dirs.into_iter().find(|d| d.starts_with(sync_dir) || sync_dir.starts_with(d)) {
        bail!("{} overlaps {}", sync_dir.display(), other.display());
    }
    Ok(())
}

/// Append a `[drive]` section to the config file.
pub fn append_drive_section(path: &Path, d: &DriveConfig) -> Result<()> {
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("\n[drive \"{}\"]\ndrive_id = \"{}\"\nsync_dir = \"{}\"\n", d.name, d.drive_id, d.sync_dir.display()));
    for (key, value) in d.filters.entries() {
        text.push_str(&format!("{key} = \"{value}\"\n"));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

/// Remove the `[drive "name"]` section (header and its keys) from the config file.
pub fn remove_drive_section(path: &Path, name: &str) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let header = |l: &str| {
        let l = l.trim();
        l.starts_with('[') && l.ends_with(']')
    };
    let target = format!("[drive \"{name}\"]");
    let mut skipping = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if header(line) {
            skipping = line.trim() == target;
        }
        if !skipping {
            out.push(line);
        }
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    std::fs::write(path, out.join("\n") + "\n")?;
    Ok(())
}

fn finish_section((d, line): (DriveConfig, usize)) -> Result<DriveConfig> {
    if d.drive_id.is_empty() || d.sync_dir.as_os_str().is_empty() {
        bail!("line {line}: [drive \"{}\"] needs both drive_id and sync_dir", d.name);
    }
    Ok(d)
}

fn append(slot: &mut Option<String>, value: &str) {
    *slot = Some(match slot.take() {
        Some(prev) => format!("{prev}|{value}"),
        None => value.to_owned(),
    });
}

fn number<T: std::str::FromStr>(v: &str, line: usize) -> Result<T> {
    v.parse().map_err(|_| anyhow::anyhow!("line {line}: `{v}` is not a number"))
}

fn boolean(v: &str, line: usize) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("line {line}: `{v}` is not true/false"),
    }
}

fn octal(v: &str, line: usize) -> Result<u32> {
    u32::from_str_radix(v, 8).map_err(|_| anyhow::anyhow!("line {line}: `{v}` is not an octal mode"))
}

pub fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_sections() {
        let c = Config::parse(
            r#"
            sync_dir = "~/OneDrive"
            [drive "Engineering"]
            drive_id = "b!abc"
            sync_dir = "~/SharePoint/Engineering"
            [drive "Sales"]
            sync_dir = "/srv/sales"
            drive_id = "b!def"
            "#,
        )
        .unwrap();
        assert_eq!(c.drives.len(), 2);
        let sales = DriveConfig { name: "Sales".into(), drive_id: "b!def".into(), sync_dir: "/srv/sales".into(), ..Default::default() };
        assert_eq!(c.drives[1], sales);
        assert!(c.sync_dir.unwrap().ends_with("OneDrive"));
        assert!(Config::parse("[drive \"X\"]\ndrive_id = \"a\"").is_err(), "missing sync_dir");
        assert!(Config::parse("[drive \"X\"]\ndrive_id = \"a\"\nsync_dir = \"/a\"\nthreads = \"2\"").is_err());
        assert!(Config::parse("[drive \"main\"]\ndrive_id = \"a\"\nsync_dir = \"/a\"").is_err());
        assert!(Config::parse("[drives]").is_err());
    }

    #[test]
    fn add_and_remove_sections() {
        let p = std::env::temp_dir().join(format!("odrs-sections-test-{}", std::process::id()));
        std::fs::write(&p, "sync_dir = \"/home/u/OneDrive\"\n").unwrap();
        let d = DriveConfig {
            name: "Engineering".into(),
            drive_id: "b!a".into(),
            sync_dir: "/home/u/SharePoint/Engineering".into(),
            ..Default::default()
        };
        append_drive_section(&p, &d).unwrap();
        let filters = FilterConfig { skip_dir: Some("build|.cache".into()), skip_dotfiles: Some(true), ..Default::default() };
        let sales = DriveConfig { name: "Sales".into(), drive_id: "b!b".into(), sync_dir: "/v".into(), filters };
        append_drive_section(&p, &sales).unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.drives.len(), 2);
        assert_eq!(c.drives[1], sales);
        let main = Path::new("/home/u/OneDrive");
        assert!(validate_new_drive(&c, main, "engineering", "b!z", Path::new("/x")).is_err(), "name taken");
        assert!(validate_new_drive(&c, main, "X", "b!a", Path::new("/x")).is_err(), "drive taken");
        assert!(validate_new_drive(&c, main, "X", "b!z", Path::new("/home/u/OneDrive/sub")).is_err(), "overlap");
        assert!(validate_new_drive(&c, main, "X", "b!z", Path::new("relative")).is_err());
        assert!(validate_new_drive(&c, main, "X", "b!z", Path::new("/home/u/SharePoint/X")).is_ok());
        remove_drive_section(&p, "Engineering").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.drives.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["Sales"]);
        assert!(c.sync_dir.is_some());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn parses_d_style_config() {
        let c = Config::parse(
            r#"
            # comment
            sync_dir = "~/SharePoint/Engineering"
            threads = "2"
            skip_dir = "node_modules"
            skip_dir = "/Documents/Old"
            skip_file = "*.bak"
            skip_dotfiles = "true"
            sync_file_permissions = "640"
            some_future_option = "x"
            "#,
        )
        .unwrap();
        assert!(c.sync_dir.unwrap().ends_with("SharePoint/Engineering"));
        assert_eq!(c.threads, Some(2));
        assert_eq!(c.filters.skip_dir.as_deref(), Some("node_modules|/Documents/Old"));
        assert_eq!(c.sync_file_permissions, Some(0o640));
        let o = Config::parse("skip_file = \"*.bak\"").unwrap().sync_options("/x".into(), None);
        assert!(o.filters.skip_file.matches("a.bak") && o.filters.skip_file.matches("a.tmp"));
        assert!(Config::parse("threads = \"many\"").is_err());

        // A drive's own filter keys replace the top-level ones.
        let c = Config::parse(
            "skip_dir = \"node_modules\"\nskip_dotfiles = \"true\"\n\
             [drive \"Sales\"]\ndrive_id = \"b!s\"\nsync_dir = \"/s\"\nskip_dir = \"Archive\"\nsync_root_files = \"true\"\n",
        )
        .unwrap();
        let o = c.sync_options("/s".into(), Some(&c.drives[0]));
        assert!(o.filters.skip_dir.matches("Archive", "Archive") && !o.filters.skip_dir.matches("node_modules", "node_modules"));
        assert!(o.filters.skip_dotfiles && o.filters.sync_root_files);
        let main = c.sync_options("/m".into(), None);
        assert!(main.filters.skip_dir.matches("node_modules", "node_modules") && !main.filters.sync_root_files);
        assert_eq!(sync_list_path(Path::new("/c"), "main"), Path::new("/c/sync_list"));
        assert_eq!(sync_list_path(Path::new("/c"), "Sales"), Path::new("/c/sync_list.Sales"));
        assert!(Config::parse("garbage").is_err());
    }
}
