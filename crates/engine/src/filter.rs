//! Which local names may be uploaded (`isValidName` and `skip_file` in the D client).

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
        Self { patterns }
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
