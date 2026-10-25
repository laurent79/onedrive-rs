//! Client-side filtering, as in the D client: names OneDrive refuses
//! (`isValidName`), `skip_file`, `skip_dir`, `skip_dotfiles` and selective
//! sync (`sync_list`).

/// Default `skip_file` of the D client.
pub const DEFAULT_SKIP_FILE: &str = "~*|.~*|*.tmp|*.swp|*.partial";

const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com0", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt0",
    "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Names Microsoft OneDrive refuses (or that the D client never uploads).
pub fn is_valid_onedrive_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    let stem = lower.split('.').next().unwrap_or("");
    if lower == ".lock" || lower == "desktop.ini" || RESERVED.contains(&stem) || lower.starts_with("~$") || lower.contains("_vti_") {
        return false;
    }
    if name.starts_with(char::is_whitespace) || name.ends_with(char::is_whitespace) || name.ends_with('.') {
        return false;
    }
    !name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\'))
}

/// `skip_file`-style patterns separated by `|`, with `*` and `?` wildcards,
/// matched case-insensitively against the file name.
#[derive(Debug, Clone)]
pub struct SkipList {
    spec: String,
    patterns: Vec<Vec<char>>,
}

impl SkipList {
    pub fn new(spec: &str) -> Self {
        let patterns = spec
            .split('|')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| p.to_lowercase().chars().collect())
            .collect();
        Self { spec: spec.to_owned(), patterns }
    }

    pub fn matches(&self, name: &str) -> bool {
        let name: Vec<char> = name.to_lowercase().chars().collect();
        self.patterns.iter().any(|p| glob(p, &name))
    }
}

impl Default for SkipList {
    fn default() -> Self {
        Self::new(DEFAULT_SKIP_FILE)
    }
}

/// D's `skip_dir`: an entry without `/` skips every directory of that name,
/// an entry with `/` skips that exact path (relative to the sync dir).
/// Case-insensitive, `*`/`?` wildcards, `|` separated.
#[derive(Debug, Clone, Default)]
pub struct SkipDirs {
    spec: String,
    names: Vec<Vec<char>>,
    paths: Vec<Vec<char>>,
}

impl SkipDirs {
    pub fn new(spec: &str) -> Self {
        let mut out = Self { spec: spec.to_owned(), ..Self::default() };
        for p in spec.split('|').map(str::trim).filter(|p| !p.is_empty()) {
            let p = p.trim_matches('/').to_lowercase();
            if p.contains('/') {
                out.paths.push(p.chars().collect());
            } else {
                out.names.push(p.chars().collect());
            }
        }
        out
    }

    /// `rel` is the directory path relative to the sync dir, `/`-separated.
    pub fn matches(&self, name: &str, rel: &str) -> bool {
        let name: Vec<char> = name.to_lowercase().chars().collect();
        let rel: Vec<char> = rel.trim_matches('/').to_lowercase().chars().collect();
        self.names.iter().any(|p| glob(p, &name)) || self.paths.iter().any(|p| glob(p, &rel))
    }
}

/// Client-side filtering rules applied in both directions (as the D client).
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub skip_file: SkipList,
    pub skip_dir: SkipDirs,
    pub skip_dotfiles: bool,
    /// Selective sync: `None` syncs everything.
    pub sync_list: Option<SyncList>,
    /// With a `sync_list`, also sync the files directly in the sync folder (D's `sync_root_files`).
    pub sync_root_files: bool,
}

impl Filters {
    /// `rel` is the path relative to the sync folder, `/`-separated, ending with `name`.
    pub fn excludes(&self, name: &str, rel: &str, is_dir: bool) -> bool {
        if self.skip_dotfiles && name.starts_with('.') {
            return true;
        }
        let skipped = if is_dir { self.skip_dir.matches(name, rel) } else { self.skip_file.matches(name) };
        if skipped {
            return true;
        }
        match &self.sync_list {
            Some(list) if !(self.sync_root_files && !is_dir && !rel.contains('/')) => list.excludes(rel, is_dir),
            _ => false,
        }
    }

    /// Changes whenever the set of synced items may change: what was synced
    /// under other rules must be looked at again (D requires `--resync`).
    pub fn fingerprint(&self) -> String {
        let list = self.sync_list.as_ref().map(SyncList::canonical).unwrap_or_default();
        let text = format!(
            "skip_file={}\nskip_dir={}\nskip_dotfiles={}\nsync_root_files={}\nsync_list=\n{list}",
            self.skip_file.spec, self.skip_dir.spec, self.skip_dotfiles, self.sync_root_files
        );
        format!("{:016x}", crate::local::fnv1a(&text))
    }
}

/// D's `sync_list`: everything is excluded except what a rule includes, and
/// exclusion rules (`!` or `-`) win over inclusions. One rule per line, `#` or
/// `;` comments.
///
/// - A leading `/` anchors the rule at the top of the sync folder; without
///   it, the rule matches at any depth (`Documents/temp*`).
/// - A rule matches the path and everything below it. A trailing `/` (or
///   `/*`) restricts it to a folder.
/// - `*` and `?` match within a name, `**` any number of folders.
///
/// Folders leading to an included path are synced too, with only the content
/// the rules include. A rule without a leading `/` may match anywhere, so it
/// makes every folder such a path (D warns it is costly).
#[derive(Debug, Clone)]
pub struct SyncList {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    exclude: bool,
    anchored: bool,
    dir_only: bool,
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Name(Vec<char>),
    AnyDepth,
}

impl SyncList {
    /// Rules D refuses are reported and left out. No valid rule = no
    /// selective sync (`None`), as in D.
    pub fn parse(text: &str) -> (Option<Self>, Vec<String>) {
        let mut rules = Vec::new();
        let mut invalid = Vec::new();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            match Rule::parse(line) {
                Some(rule) => rules.push(rule),
                None => invalid.push(line.to_owned()),
            }
        }
        ((!rules.is_empty()).then_some(Self { rules }), invalid)
    }

    /// Read a `sync_list` file; a missing file means no selective sync.
    pub fn load(path: &std::path::Path) -> std::io::Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let (list, invalid) = Self::parse(&text);
        for rule in invalid {
            tracing::warn!(file = %path.display(), rule, "invalid sync_list rule ignored");
        }
        Ok(list)
    }

    /// `rel`: path relative to the sync folder, `/`-separated.
    pub fn excludes(&self, rel: &str, is_dir: bool) -> bool {
        let path: Vec<Vec<char>> = rel.split('/').filter(|s| !s.is_empty()).map(|s| s.to_lowercase().chars().collect()).collect();
        if path.is_empty() {
            return false;
        }
        if self.rules.iter().any(|r| r.exclude && r.covers(&path, is_dir)) {
            return true;
        }
        let includes = self.rules.iter().filter(|r| !r.exclude);
        !includes.clone().any(|r| r.covers(&path, is_dir)) && !(is_dir && includes.clone().any(|r| r.leads_through(&path)))
    }

    fn canonical(&self) -> String {
        self.rules.iter().map(|r| format!("{r:?}\n")).collect()
    }
}

impl Rule {
    fn parse(line: &str) -> Option<Self> {
        let (exclude, rest) = match line.strip_prefix(['!', '-']) {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line),
        };
        // D refuses these: the root itself is not a rule (see `sync_root_files`).
        if rest.starts_with("./") || matches!(rest, "/" | "/*") {
            return None;
        }
        let anchored = rest.starts_with('/');
        let mut body = rest.trim_start_matches('/');
        let mut dir_only = false;
        if let Some(b) = body.strip_suffix("/*") {
            (body, dir_only) = (b, true);
        }
        if body.ends_with('/') {
            dir_only = true;
        }
        let segments: Vec<Segment> = body
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .map(|s| if s == "**" { Segment::AnyDepth } else { Segment::Name(s.to_lowercase().chars().collect()) })
            .collect();
        if segments.is_empty() || segments.iter().any(|s| matches!(s, Segment::Name(n) if n == &['.', '.'])) {
            return None;
        }
        Some(Self { exclude, anchored, dir_only, segments })
    }

    /// The rule matches `path` itself or one of its parent folders.
    fn covers(&self, path: &[Vec<char>], is_dir: bool) -> bool {
        let starts = if self.anchored { 0..1 } else { 0..path.len() };
        starts.into_iter().any(|start| {
            (start + 1..=path.len()).any(|end| {
                let a_folder = end < path.len() || is_dir;
                (!self.dir_only || a_folder) && segments_match(&self.segments, &path[start..end])
            })
        })
    }

    /// `path` (a folder) may contain something this rule matches.
    fn leads_through(&self, path: &[Vec<char>]) -> bool {
        !self.anchored || prefix_match(&self.segments, path)
    }
}

/// The whole of `path` is matched by `rule`.
fn segments_match(rule: &[Segment], path: &[Vec<char>]) -> bool {
    match rule.split_first() {
        None => path.is_empty(),
        Some((Segment::AnyDepth, rest)) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
        Some((Segment::Name(p), rest)) => path.split_first().is_some_and(|(name, tail)| glob(p, name) && segments_match(rest, tail)),
    }
}

/// `path` is matched by the beginning of `rule`, with something left to match below it.
fn prefix_match(rule: &[Segment], path: &[Vec<char>]) -> bool {
    match (rule.split_first(), path.split_first()) {
        (None, _) => false,
        (Some((Segment::AnyDepth, _)), _) => true,
        (Some(_), None) => true,
        (Some((Segment::Name(p), rest)), Some((name, tail))) => glob(p, name) && prefix_match(rest, tail),
    }
}

/// Iterative wildcard match (`*` any run, `?` one char).
fn glob(pattern: &[char], text: &[char]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onedrive_names() {
        assert!(is_valid_onedrive_name("Rapport final.docx"));
        assert!(is_valid_onedrive_name(".bashrc"));
        assert!(!is_valid_onedrive_name("CON"));
        assert!(!is_valid_onedrive_name("con.txt"));
        assert!(is_valid_onedrive_name("console.txt"));
        assert!(!is_valid_onedrive_name("~$rapport.docx"));
        assert!(!is_valid_onedrive_name("a:b"));
        assert!(!is_valid_onedrive_name("trailing."));
        assert!(!is_valid_onedrive_name(" leading"));
        assert!(!is_valid_onedrive_name("Desktop.ini"));
    }

    #[test]
    fn skip_dirs() {
        let d = SkipDirs::new("node_modules|/Documents/Visual Studio*|.cache");
        assert!(d.matches("node_modules", "a/b/node_modules"));
        assert!(d.matches("Visual Studio 2022", "Documents/Visual Studio 2022"));
        assert!(!d.matches("Visual Studio 2022", "Other/Visual Studio 2022"));
        assert!(d.matches(".Cache", ".Cache"));
        assert!(!d.matches("src", "src"));
        let f = Filters { skip_dotfiles: true, ..Default::default() };
        assert!(f.excludes(".git", ".git", true));
        assert!(f.excludes("x.partial", "x.partial", false));
        assert!(!f.excludes("notes.txt", "notes.txt", false));
    }

    fn list(text: &str) -> SyncList {
        let (list, invalid) = SyncList::parse(text);
        assert!(invalid.is_empty(), "{invalid:?}");
        list.unwrap()
    }

    #[test]
    fn sync_list_anchored_rules() {
        let l = list("# comment\n/Documents\n/Teams/Engineering/\n!/Documents/Old\n-/Teams/Engineering/build/*\n");
        // Included paths and everything below them.
        assert!(!l.excludes("Documents", true));
        assert!(!l.excludes("documents/report.docx", false), "case-insensitive");
        assert!(!l.excludes("Documents/a/b/c.txt", false));
        // Folders leading to an included path are kept, not their other content.
        assert!(!l.excludes("Teams", true));
        assert!(l.excludes("Teams/notes.txt", false));
        assert!(l.excludes("Teams/Sales", true));
        assert!(!l.excludes("Teams/Engineering/plan.pdf", false));
        // A trailing / only matches a folder.
        assert!(l.excludes("Teams/Engineering", false));
        // Exclusions win, below them too.
        assert!(l.excludes("Documents/Old", true));
        assert!(l.excludes("Documents/Old/x.txt", false));
        assert!(!l.excludes("Documents/Older", true));
        assert!(l.excludes("Teams/Engineering/build", true));
        assert!(l.excludes("Teams/Engineering/build/out.o", false));
        // Not listed.
        assert!(l.excludes("Pictures", true));
        assert!(l.excludes("top.txt", false));
        let root_files = Filters { sync_list: Some(l), sync_root_files: true, ..Default::default() };
        assert!(!root_files.excludes("top.txt", "top.txt", false));
        assert!(root_files.excludes("Pictures", "Pictures", true));
    }

    #[test]
    fn sync_list_wildcards_and_anywhere_rules() {
        let l = list("!node_modules/\n!*.tmp\n/Work/Project*\n/Code/**/docs/\nnotes.txt\n");
        assert!(!l.excludes("Work/Project A", true));
        assert!(!l.excludes("Work/Project A/spec.odt", false));
        assert!(l.excludes("Work/Other/a.txt", false));
        assert!(l.excludes("Work/Project A/node_modules", true));
        assert!(l.excludes("Work/Project A/x.tmp", false));
        assert!(!l.excludes("Code/docs/index.md", false), "** matches no folder too");
        assert!(!l.excludes("Code/a/b/docs/index.md", false));
        assert!(!l.excludes("Code/a/b", true), "may lead to a docs folder");
        assert!(l.excludes("Code/a/b/main.rs", false));
        // A rule without a leading / matches at any depth, so every folder may lead to it.
        assert!(!l.excludes("Random/deep/notes.txt", false));
        assert!(!l.excludes("Random/deep", true));
        assert!(l.excludes("Random/deep/other.txt", false));
        assert!(l.excludes("Random/node_modules", true));
    }

    #[test]
    fn sync_list_invalid_rules() {
        let (none, invalid) = SyncList::parse("/\n/*\n!/*\n./Documents\n/a/../b\n\n");
        assert!(none.is_none(), "no valid rule = no selective sync");
        assert_eq!(invalid.len(), 5);
        let a = Filters { sync_list: Some(list("/A")), ..Default::default() };
        let b = Filters { sync_list: Some(list("/B")), ..Default::default() };
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint(), Filters { sync_list: Some(list("# same\n/A")), ..Default::default() }.fingerprint());
        assert_ne!(Filters::default().fingerprint(), Filters { skip_dotfiles: true, ..Default::default() }.fingerprint());
    }

    #[test]
    fn skip_list() {
        let s = SkipList::default();
        assert!(s.matches("~lock"));
        assert!(s.matches(".~lock.rapport.odt#"));
        assert!(s.matches("x.TMP"));
        assert!(s.matches("photo.jpg.partial"));
        assert!(!s.matches("notes.txt"));
        assert!(!s.matches("a~b"));
        let s = SkipList::new("*.bak | build?.log");
        assert!(s.matches("x.bak") && s.matches("build1.log"));
        assert!(!s.matches("build12.log"));
    }
}
