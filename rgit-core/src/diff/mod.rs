//! Line-level diff (LCS algorithm) and unified-diff output formatting.
//!
//! The line algorithm is a straightforward LCS dynamic-programming
//! implementation — O(N×M) time and space. For typical source files
//! (sub-10K lines) this is more than fast enough; the Myers-O(N+D)
//! variant is a future optimization.
//!
//! `unified_diff` formats per-file diffs in the format `git diff`
//! emits. `Repository::diff_*` methods stitch per-file diffs into a
//! repo-level multi-file diff covering working-tree, index, and tree
//! snapshots.

#[cfg(test)]
mod tests;

use crate::index::Index;
use crate::object::{EntryMode, Object, ObjectId, ObjectKind};
use crate::odb::{OdbError, Repository};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DiffError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("odb error: {0}")]
    Odb(#[from] OdbError),
    #[error("refs error: {0}")]
    Refs(#[from] crate::refs::RefError),
    #[error("workdir error: {0}")]
    Workdir(#[from] crate::workdir::WorkdirError),
    #[error("index error: {0}")]
    Index(#[from] crate::index::IndexError),
    #[error("object error: {0}")]
    Object(#[from] crate::object::ParseError),
    #[error("repository has no working tree")]
    NoWorkTree,
}

/// One step in an LCS-derived edit script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineChange<'a> {
    Same(&'a [u8]),
    Removed(&'a [u8]),
    Added(&'a [u8]),
}

/// Split bytes into lines. Each line includes its trailing `\n` if
/// present. The final line lacks `\n` only if the input does.
pub fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    out
}

/// Compute the diff between two line sequences via LCS.
pub fn diff_lines<'a>(a: &[&'a [u8]], b: &[&'a [u8]]) -> Vec<LineChange<'a>> {
    let n = a.len();
    let m = b.len();
    // dp[i][j] = LCS length of a[..i], b[..j].
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            if a[i - 1] == b[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Backtrack.
    let mut out = Vec::with_capacity(n + m);
    let mut i = n;
    let mut j = m;
    while i > 0 && j > 0 {
        if a[i - 1] == b[j - 1] {
            out.push(LineChange::Same(a[i - 1]));
            i -= 1;
            j -= 1;
        } else if dp[i][j - 1] >= dp[i - 1][j] {
            // Prefer going left (= Added) on ties so the reversed
            // output emits Removed-before-Added for adjacent changes,
            // matching `git diff` convention.
            out.push(LineChange::Added(b[j - 1]));
            j -= 1;
        } else {
            out.push(LineChange::Removed(a[i - 1]));
            i -= 1;
        }
    }
    while i > 0 {
        out.push(LineChange::Removed(a[i - 1]));
        i -= 1;
    }
    while j > 0 {
        out.push(LineChange::Added(b[j - 1]));
        j -= 1;
    }
    out.reverse();
    out
}

/// Format a unified diff between two byte sequences with the given
/// labels and `context` lines around each hunk.
pub fn unified_diff(a: &[u8], a_label: &str, b: &[u8], b_label: &str, context: usize) -> String {
    let a_lines = split_lines(a);
    let b_lines = split_lines(b);
    let script = diff_lines(&a_lines, &b_lines);

    let mut out = String::new();
    out.push_str(&format!("--- {a_label}\n+++ {b_label}\n"));

    // Walk the edit script in hunks. A hunk starts at the first change
    // (preceded by up to `context` Same lines) and ends when `context`
    // consecutive Same lines after the last change have elapsed.
    let mut a_line: usize = 0; // 1-based line numbers when emitted
    let mut b_line: usize = 0;
    let mut i = 0;
    while i < script.len() {
        // Find next change.
        let mut start = i;
        while start < script.len() && matches!(script[start], LineChange::Same(_)) {
            if let LineChange::Same(_) = script[start] {
                a_line += 1;
                b_line += 1;
            }
            start += 1;
        }
        if start >= script.len() {
            break;
        }
        // Back up to include `context` lines before the change.
        let hunk_start = start.saturating_sub(context);
        for _ in 0..(start - hunk_start) {
            a_line -= 1;
            b_line -= 1;
        }
        let hunk_a_start = a_line + 1;
        let hunk_b_start = b_line + 1;

        // Extend the hunk forward, growing past changes until we've
        // seen `context` consecutive Same lines.
        let mut end = start;
        let mut seen_trailing_same = 0;
        while end < script.len() {
            match &script[end] {
                LineChange::Same(_) => {
                    seen_trailing_same += 1;
                    if seen_trailing_same > context {
                        // Roll back the extra trailing-context lines.
                        end -= seen_trailing_same - context;
                        break;
                    }
                }
                _ => {
                    seen_trailing_same = 0;
                }
            }
            end += 1;
        }
        if end > script.len() {
            end = script.len();
        }

        // Count lines from a and b in this hunk.
        let mut a_count = 0usize;
        let mut b_count = 0usize;
        for op in &script[hunk_start..end] {
            match op {
                LineChange::Same(_) => {
                    a_count += 1;
                    b_count += 1;
                }
                LineChange::Removed(_) => a_count += 1,
                LineChange::Added(_) => b_count += 1,
            }
        }
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk_a_start, a_count, hunk_b_start, b_count,
        ));
        for op in &script[hunk_start..end] {
            let (prefix, bytes) = match op {
                LineChange::Same(b) => (' ', *b),
                LineChange::Removed(b) => ('-', *b),
                LineChange::Added(b) => ('+', *b),
            };
            out.push(prefix);
            out.push_str(&String::from_utf8_lossy(bytes));
            if !bytes.ends_with(b"\n") {
                out.push('\n');
                out.push_str("\\ No newline at end of file\n");
            }
        }

        // Advance running line counters past this hunk.
        for op in &script[hunk_start..end] {
            match op {
                LineChange::Same(_) => {
                    a_line += 1;
                    b_line += 1;
                }
                LineChange::Removed(_) => a_line += 1,
                LineChange::Added(_) => b_line += 1,
            }
        }
        i = end;
    }

    out
}

// ---------------------------------------------------------------------
// Repository-level diff helpers
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PathEntry {
    id: ObjectId,
}

fn collect_tree_paths(
    repo: &Repository,
    tree_id: &ObjectId,
    prefix: &[u8],
    out: &mut BTreeMap<Vec<u8>, PathEntry>,
) -> Result<(), DiffError> {
    let obj = repo.read_object(tree_id)?;
    let Object::Tree(tree) = obj else {
        return Ok(());
    };
    for entry in &tree.entries {
        let mut path = prefix.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&entry.name);
        match entry.mode {
            EntryMode::Tree => collect_tree_paths(repo, &entry.id, &path, out)?,
            _ => {
                out.insert(path, PathEntry { id: entry.id });
            }
        }
    }
    Ok(())
}

fn index_paths(index: &Index) -> BTreeMap<Vec<u8>, PathEntry> {
    let mut out = BTreeMap::new();
    for entry in index.entries() {
        if entry.stage != 0 {
            continue;
        }
        out.insert(entry.path.clone(), PathEntry { id: entry.id });
    }
    out
}

fn blob_bytes(repo: &Repository, id: &ObjectId) -> Result<Vec<u8>, DiffError> {
    let (kind, payload) = repo.read_object_raw(id)?;
    if kind != ObjectKind::Blob {
        // Tree/commit/tag content — not a regular file diff target.
        return Ok(Vec::new());
    }
    Ok(payload)
}

impl Repository {
    /// Unified diff between two trees, walking all paths.
    pub fn diff_trees(&self, a: &ObjectId, b: &ObjectId) -> Result<String, DiffError> {
        let mut a_paths = BTreeMap::new();
        collect_tree_paths(self, a, b"", &mut a_paths)?;
        let mut b_paths = BTreeMap::new();
        collect_tree_paths(self, b, b"", &mut b_paths)?;
        diff_path_maps(
            self,
            &a_paths,
            &b_paths,
            |a_id| self.blob_bytes_owned(a_id),
            |b_id| self.blob_bytes_owned(b_id),
        )
    }

    /// Unified diff between the index and the HEAD tree.
    pub fn diff_index_vs_head(&self) -> Result<String, DiffError> {
        let head_id = self.read_ref("HEAD")?;
        let Object::Commit(commit) = self.read_object(&head_id)? else {
            return Ok(String::new());
        };
        let mut head_paths = BTreeMap::new();
        collect_tree_paths(self, &commit.tree, b"", &mut head_paths)?;
        let index = self.read_index()?;
        let idx_paths = index_paths(&index);
        diff_path_maps(
            self,
            &head_paths,
            &idx_paths,
            |id| self.blob_bytes_owned(id),
            |id| self.blob_bytes_owned(id),
        )
    }

    /// Unified diff between the working tree and the index.
    pub fn diff_working_vs_index(&self) -> Result<String, DiffError> {
        let work_dir = self.work_dir().ok_or(DiffError::NoWorkTree)?.to_path_buf();
        let index = self.read_index()?;
        let idx_paths = index_paths(&index);
        // Build a "current working tree" map keyed by path → bytes.
        let mut wt_contents: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        for path in idx_paths.keys() {
            let path_str = std::str::from_utf8(path).unwrap_or("");
            let abs = work_dir.join(path_str);
            match fs::read(&abs) {
                Ok(bytes) => {
                    wt_contents.insert(path.clone(), bytes);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // file deleted; no entry → diff renders as full removal
                }
                Err(e) => return Err(DiffError::Io(e)),
            }
        }

        let mut out = String::new();
        for (path, idx_entry) in &idx_paths {
            let idx_bytes = self.blob_bytes_owned(&idx_entry.id)?;
            let wt_bytes_owned;
            let wt_bytes: &[u8] = match wt_contents.get(path) {
                Some(b) => b,
                None => {
                    wt_bytes_owned = Vec::new();
                    &wt_bytes_owned
                }
            };
            if wt_bytes == idx_bytes.as_slice() {
                continue;
            }
            let path_str = String::from_utf8_lossy(path);
            out.push_str(&format!("diff --git a/{path_str} b/{path_str}\n"));
            out.push_str(&unified_diff(
                &idx_bytes,
                &format!("a/{path_str}"),
                wt_bytes,
                &format!("b/{path_str}"),
                3,
            ));
        }
        Ok(out)
    }

    fn blob_bytes_owned(&self, id: &ObjectId) -> Result<Vec<u8>, DiffError> {
        blob_bytes(self, id)
    }
}

fn diff_path_maps<FA, FB>(
    _repo: &Repository,
    a: &BTreeMap<Vec<u8>, PathEntry>,
    b: &BTreeMap<Vec<u8>, PathEntry>,
    fetch_a: FA,
    fetch_b: FB,
) -> Result<String, DiffError>
where
    FA: Fn(&ObjectId) -> Result<Vec<u8>, DiffError>,
    FB: Fn(&ObjectId) -> Result<Vec<u8>, DiffError>,
{
    let mut out = String::new();
    let mut all_paths: BTreeMap<&[u8], ()> = BTreeMap::new();
    for k in a.keys() {
        all_paths.insert(k.as_slice(), ());
    }
    for k in b.keys() {
        all_paths.insert(k.as_slice(), ());
    }
    for path in all_paths.keys() {
        let path_owned: Vec<u8> = path.to_vec();
        let a_entry = a.get(&path_owned);
        let b_entry = b.get(&path_owned);
        let same = match (a_entry, b_entry) {
            (Some(ae), Some(be)) => ae.id == be.id,
            _ => false,
        };
        if same {
            continue;
        }
        let a_bytes = match a_entry {
            Some(e) => fetch_a(&e.id)?,
            None => Vec::new(),
        };
        let b_bytes = match b_entry {
            Some(e) => fetch_b(&e.id)?,
            None => Vec::new(),
        };
        let path_str = String::from_utf8_lossy(path);
        out.push_str(&format!("diff --git a/{path_str} b/{path_str}\n"));
        out.push_str(&unified_diff(
            &a_bytes,
            &format!("a/{path_str}"),
            &b_bytes,
            &format!("b/{path_str}"),
            3,
        ));
    }
    Ok(out)
}
