//! `.gitignore`-style pattern matching.
//!
//! Loads patterns from the repository's top-level `.gitignore` and the
//! local-only `.git/info/exclude`. Cascading per-directory `.gitignore`
//! files are deferred — v1 reads only the root files.

#[cfg(test)]
mod tests;

use std::fs;
use std::path::Path;

/// A set of gitignore rules. Later rules can override earlier rules
/// (via `!` negation).
#[derive(Debug, Default, Clone)]
pub struct GitignoreSet {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    /// Pattern with leading `!` and trailing `/` stripped.
    pattern: Vec<u8>,
    negate: bool,
    /// If true, only matches directories.
    dir_only: bool,
    /// If true, pattern anchored to the gitignore file's directory.
    anchored: bool,
}

impl GitignoreSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load rules from `<repo_root>/.gitignore` and
    /// `<repo_root>/.git/info/exclude` (both optional).
    pub fn load(repo_root: &Path) -> std::io::Result<Self> {
        let mut set = Self::new();
        for path in [
            repo_root.join(".gitignore"),
            repo_root.join(".git").join("info").join("exclude"),
        ] {
            match fs::read_to_string(&path) {
                Ok(contents) => set.add_rules(&contents),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(set)
    }

    /// Parse and append rules from gitignore-formatted text.
    pub fn add_rules(&mut self, content: &str) {
        for raw_line in content.lines() {
            let line = raw_line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Trailing-space handling: gitignore preserves a trailing
            // backslash-escaped space; we don't bother with that nuance.
            let mut pattern = line.trim_end_matches(' ').to_string();
            if pattern.is_empty() {
                continue;
            }

            let negate = if let Some(rest) = pattern.strip_prefix('!') {
                pattern = rest.to_string();
                true
            } else {
                false
            };

            let dir_only = if pattern.ends_with('/') {
                pattern.pop();
                true
            } else {
                false
            };

            // Anchored if leading `/`, or if the (now-stripped) pattern
            // contains a `/` anywhere other than at the end.
            let anchored = pattern.starts_with('/') || pattern.contains('/');
            let pattern = pattern.trim_start_matches('/').as_bytes().to_vec();
            if pattern.is_empty() {
                continue;
            }
            self.rules.push(Rule {
                pattern,
                negate,
                dir_only,
                anchored,
            });
        }
    }

    /// Test whether `path` (a byte-slice relative path with `/` separators)
    /// is ignored by this rule set. `is_dir` should be true if the path
    /// refers to a directory.
    ///
    /// A file inside an ignored directory is also ignored: if any
    /// ancestor directory of `path` matches a rule, the file is too.
    pub fn is_ignored(&self, path: &[u8], is_dir: bool) -> bool {
        let mut ignored = false;
        for rule in &self.rules {
            if rule.matches_self_or_ancestor(path, is_dir) {
                ignored = !rule.negate;
            }
        }
        ignored
    }
}

impl Rule {
    /// True if this rule matches the path itself OR any ancestor
    /// directory of the path. Used so `target/` ignores files inside
    /// `target/` too.
    fn matches_self_or_ancestor(&self, path: &[u8], is_dir: bool) -> bool {
        // The path itself, if rule applies to non-dirs or path is a dir.
        if (!self.dir_only || is_dir) && self.matches(path) {
            return true;
        }
        // Any ancestor directory of `path`.
        for (i, &b) in path.iter().enumerate() {
            if b == b'/' && self.matches(&path[..i]) {
                return true;
            }
        }
        false
    }

    fn matches(&self, path: &[u8]) -> bool {
        if self.anchored {
            glob_match(&self.pattern, path)
        } else {
            // Unanchored: try matching against the full path AND every
            // suffix that starts at a `/` boundary.
            if glob_match(&self.pattern, path) {
                return true;
            }
            for (i, &b) in path.iter().enumerate() {
                if b == b'/' && glob_match(&self.pattern, &path[i + 1..]) {
                    return true;
                }
            }
            false
        }
    }
}

/// Glob match. Supports `*` (non-`/`), `**` (anything), `?` (single
/// non-`/` char), and literal bytes elsewhere.
fn glob_match(pattern: &[u8], path: &[u8]) -> bool {
    match (pattern.first(), path.first()) {
        (None, None) => true,
        (None, _) => false,
        (Some(b'*'), _) if pattern.len() >= 2 && pattern[1] == b'*' => {
            // `**` — matches anything, including `/`.
            let mut rest = &pattern[2..];
            if let Some(b'/') = rest.first() {
                rest = &rest[1..];
            }
            // Try matching the rest at every position of `path`.
            for skip in 0..=path.len() {
                if glob_match(rest, &path[skip..]) {
                    return true;
                }
            }
            false
        }
        (Some(b'*'), _) => {
            let rest = &pattern[1..];
            // `*` matches zero or more non-`/` characters.
            if glob_match(rest, path) {
                return true;
            }
            if let Some(&pc) = path.first() {
                if pc != b'/' {
                    return glob_match(pattern, &path[1..]);
                }
            }
            false
        }
        (Some(b'?'), Some(&pc)) if pc != b'/' => glob_match(&pattern[1..], &path[1..]),
        (Some(&pc), Some(&tc)) if pc == tc => glob_match(&pattern[1..], &path[1..]),
        _ => false,
    }
}
