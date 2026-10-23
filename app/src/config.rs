//! `~/.config/onedrive-rs/config`, in the D client's format: one
//! `key = "value"` per line, `#` comments; `skip_file`/`skip_dir` may repeat.
//! Command-line options override the file.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Config {
    pub sync_dir: Option<PathBuf>,
    pub application_id: Option<String>,
    pub threads: Option<usize>,
    pub monitor_interval: Option<u64>,
    pub download_only: Option<bool>,
    pub skip_file: Option<String>,
    pub skip_dir: Option<String>,
    pub skip_dotfiles: Option<bool>,
    pub classify_as_big_delete: Option<u64>,
    pub sync_dir_permissions: Option<u32>,
    pub sync_file_permissions: Option<u32>,
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
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
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
                "skip_file" => append(&mut c.skip_file, value),
                "skip_dir" => append(&mut c.skip_dir, value),
                "skip_dotfiles" => c.skip_dotfiles = Some(boolean(value, line_no)?),
                "classify_as_big_delete" => c.classify_as_big_delete = Some(number(value, line_no)?),
                // Octal digits written as a decimal number, as in the D client ("700").
                "sync_dir_permissions" => c.sync_dir_permissions = Some(octal(value, line_no)?),
                "sync_file_permissions" => c.sync_file_permissions = Some(octal(value, line_no)?),
                other => tracing::warn!(key = other, line = line_no, "unsupported config option ignored"),
            }
        }
        Ok(c)
    }

    /// Engine options: defaults, then this file.
    pub fn sync_options(&self, sync_dir: PathBuf) -> odrs_engine::SyncOptions {
        let mut o = odrs_engine::SyncOptions::new(sync_dir);
        if let Some(t) = self.threads {
            o.max_transfers = t;
        }
        // Like D, user skip_file entries come on top of the defaults.
        if let Some(extra) = &self.skip_file {
            o.filters.skip_file = odrs_engine::filter::SkipList::new(&format!("{}|{extra}", odrs_engine::filter::DEFAULT_SKIP_FILE));
        }
        if let Some(d) = &self.skip_dir {
            o.filters.skip_dir = odrs_engine::filter::SkipDirs::new(d);
        }
        o.filters.skip_dotfiles = self.skip_dotfiles.unwrap_or(false);
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
        assert_eq!(c.skip_dir.as_deref(), Some("node_modules|/Documents/Old"));
        assert_eq!(c.sync_file_permissions, Some(0o640));
        let o = Config::parse("skip_file = \"*.bak\"").unwrap().sync_options("/x".into());
        assert!(o.filters.skip_file.matches("a.bak") && o.filters.skip_file.matches("a.tmp"));
        assert!(Config::parse("threads = \"many\"").is_err());
        assert!(Config::parse("garbage").is_err());
    }
}
