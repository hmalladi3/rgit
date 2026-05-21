//! `.gitignore`-style pattern matching.
//!
//! Loads patterns from the repository's top-level `.gitignore` and the
//! local-only `.git/info/exclude`. Cascading per-directory `.gitignore`
//! files are deferred — v1 reads only the root files.

#[cfg(test)]
mod tests;

use std::fs;
use std::path::Path;

/// A set of gitignore rules from one or more `.gitignore` files. Each
/// per-file rule set is tagged with the directory it lives in;
/// matching against a path applies only the rule sets whose directory
/// is an ancestor of (or identical to) the path's parent.
#[derive(Debug, Default, Clone)]
pub struct GitignoreSet {
    /// Each entry: `(dir_prefix relative to repo root using '/' separators, rules)`.
    /// The root `.gitignore` and `.git/info/exclude` use the empty prefix.
    sets: Vec<(Vec<u8>, Vec<Rule>)>,
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

    /// Load rules from every `.gitignore` file under `repo_root` (plus
    /// `<repo_root>/.git/info/exclude`). Per-directory `.gitignore`
    /// files cascade: rules in `subdir/.gitignore` apply only to paths
    /// under `subdir/`.
    pub fn load(repo_root: &Path) -> std::io::Result<Self> {
        let mut set = Self::new();
        // Root .gitignore + .git/info/exclude (both have empty prefix).
        for path in [
            repo_root.join(".gitignore"),
            repo_root.join(".git").join("info").join("exclude"),
        ] {
            match fs::read_to_string(&path) {
                Ok(contents) => set.add_rules_with_prefix(b"", &contents),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            }
        }
        // Walk subdirectories looking for nested .gitignore files.
        // (Skip .git/ to avoid recursing into our own metadata.)
        load_nested_gitignores(repo_root, repo_root, &mut set)?;
        Ok(set)
    }

    /// Parse and append rules from gitignore-formatted text with an
    /// empty base prefix (legacy single-directory mode).
    pub fn add_rules(&mut self, content: &str) {
        self.add_rules_with_prefix(b"", content);
    }

    fn add_rules_with_prefix(&mut self, prefix: &[u8], content: &str) {
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
            self.upsert_rule(
                prefix,
                Rule {
                    pattern,
                    negate,
                    dir_only,
                    anchored,
                },
            );
        }
    }

    fn upsert_rule(&mut self, prefix: &[u8], rule: Rule) {
        // Append to the existing rule set for this prefix, or create one.
        for (p, rules) in &mut self.sets {
            if p == prefix {
                rules.push(rule);
                return;
            }
        }
        self.sets.push((prefix.to_vec(), vec![rule]));
    }

    /// Test whether `path` (a byte-slice relative path with `/` separators)
    /// is ignored by this rule set. `is_dir` should be true if the path
    /// refers to a directory.
    ///
    /// A file inside an ignored directory is also ignored: if any
    /// ancestor directory of `path` matches a rule, the file is too.
    pub fn is_ignored(&self, path: &[u8], is_dir: bool) -> bool {
        let mut ignored = false;
        for (prefix, rules) in &self.sets {
            // A rule set with prefix `p` only applies to paths under `p/`.
            // Compute the path's location relative to `p`.
            let relative = match relative_to_prefix(path, prefix) {
                Some(r) => r,
                None => continue,
            };
            for rule in rules {
                if rule.matches_self_or_ancestor(relative, is_dir) {
                    ignored = !rule.negate;
                }
            }
        }
        ignored
    }
}

/// Compute `path` relative to `prefix`. Returns `None` when `prefix` is
/// not an ancestor directory of `path`. Empty prefix matches anything.
fn relative_to_prefix<'a>(path: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if prefix.is_empty() {
        return Some(path);
    }
    if !path.starts_with(prefix) {
        return None;
    }
    if path.len() == prefix.len() {
        // path is the prefix dir itself; nested rules don't apply to it.
        return None;
    }
    if path[prefix.len()] != b'/' {
        return None;
    }
    Some(&path[prefix.len() + 1..])
}

fn load_nested_gitignores(
    repo_root: &Path,
    current: &Path,
    set: &mut GitignoreSet,
) -> std::io::Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let dir = entry.path();
        let gitignore = dir.join(".gitignore");
        if let Ok(contents) = fs::read_to_string(&gitignore) {
            let rel = match dir.strip_prefix(repo_root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let prefix = rel
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/")
                .into_bytes();
            set.add_rules_with_prefix(&prefix, &contents);
        }
        load_nested_gitignores(repo_root, &dir, set)?;
    }
    Ok(())
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
