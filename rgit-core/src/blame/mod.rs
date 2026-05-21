//! Per-line authorship walk.
//!
//! For each line in the file at HEAD (or a given commit), find the
//! commit that introduced that line. Walks the first-parent chain
//! backwards; each iteration uses the diff module's LCS edit script to
//! map current-side lines back to the parent's lines. A line that's
//! `Added` (present in current, absent in parent) is attributed to the
//! current commit and removed from the active set.

#[cfg(test)]
mod tests;

use crate::diff::{diff_lines, split_lines, LineChange};
use crate::object::{EntryMode, Object, ObjectId};
use crate::odb::{OdbError, Repository};
use crate::refs::RefError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BlameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("odb: {0}")]
    Odb(#[from] OdbError),
    #[error("refs: {0}")]
    Refs(#[from] RefError),
    #[error("object: {0}")]
    Object(#[from] crate::object::ParseError),
    #[error("file not found at the requested commit: {0:?}")]
    FileNotFound(String),
}

#[derive(Debug, Clone)]
pub struct BlameLine {
    pub commit: ObjectId,
    pub line: Vec<u8>,
}

impl Repository {
    /// Annotate each line of `path` at `head_commit` with the commit
    /// that introduced it.
    pub fn blame(&self, path: &str, head_commit: &ObjectId) -> Result<Vec<BlameLine>, BlameError> {
        let head_lines = read_file_at(self, head_commit, path)?
            .ok_or_else(|| BlameError::FileNotFound(path.to_string()))?;

        let n = head_lines.len();
        let mut annotations: Vec<ObjectId> = vec![*head_commit; n];
        let mut decided: Vec<bool> = vec![false; n];
        let mut current_to_head: Vec<usize> = (0..n).collect();
        let mut current_lines: Vec<Vec<u8>> = head_lines.clone();
        let mut current_commit = *head_commit;

        loop {
            if decided.iter().all(|d| *d) {
                break;
            }
            let obj = self.read_object(&current_commit)?;
            let Object::Commit(commit) = obj else { break };

            if commit.parents.is_empty() {
                for &h in &current_to_head {
                    if !decided[h] {
                        annotations[h] = current_commit;
                        decided[h] = true;
                    }
                }
                break;
            }

            let parent = commit.parents[0];
            let parent_lines = match read_file_at(self, &parent, path)? {
                Some(lines) => lines,
                None => {
                    // File didn't exist in parent — every remaining
                    // current line was introduced by `current_commit`.
                    for &h in &current_to_head {
                        if !decided[h] {
                            annotations[h] = current_commit;
                            decided[h] = true;
                        }
                    }
                    break;
                }
            };

            let parent_refs: Vec<&[u8]> = parent_lines.iter().map(|v| v.as_slice()).collect();
            let current_refs: Vec<&[u8]> = current_lines.iter().map(|v| v.as_slice()).collect();
            let script = diff_lines(&parent_refs, &current_refs);

            let mut new_lines: Vec<Vec<u8>> = Vec::new();
            let mut new_to_head: Vec<usize> = Vec::new();
            let mut current_idx: usize = 0;
            for op in &script {
                match op {
                    LineChange::Same(line) => {
                        let h = current_to_head[current_idx];
                        new_lines.push(line.to_vec());
                        new_to_head.push(h);
                        current_idx += 1;
                    }
                    LineChange::Added(_) => {
                        // Introduced going from parent to current_commit.
                        let h = current_to_head[current_idx];
                        if !decided[h] {
                            annotations[h] = current_commit;
                            decided[h] = true;
                        }
                        current_idx += 1;
                    }
                    LineChange::Removed(_) => {
                        // Parent-only line, no current correspondence.
                    }
                }
            }

            current_lines = new_lines;
            current_to_head = new_to_head;
            current_commit = parent;
        }

        Ok(annotations
            .into_iter()
            .zip(head_lines)
            .map(|(commit, line)| BlameLine { commit, line })
            .collect())
    }
}

fn read_file_at(
    repo: &Repository,
    commit_id: &ObjectId,
    path: &str,
) -> Result<Option<Vec<Vec<u8>>>, BlameError> {
    let obj = repo.read_object(commit_id)?;
    let Object::Commit(commit) = obj else {
        return Ok(None);
    };
    let Some(blob_id) = find_path_in_tree(repo, &commit.tree, path)? else {
        return Ok(None);
    };
    let blob_obj = repo.read_object(&blob_id)?;
    let Object::Blob(blob) = blob_obj else {
        return Ok(None);
    };
    let lines: Vec<Vec<u8>> = split_lines(&blob.data)
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect();
    Ok(Some(lines))
}

fn find_path_in_tree(
    repo: &Repository,
    tree_id: &ObjectId,
    path: &str,
) -> Result<Option<ObjectId>, BlameError> {
    let mut components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(last) = components.pop() else {
        return Ok(None);
    };
    let mut cur_tree = *tree_id;
    for comp in components {
        let obj = repo.read_object(&cur_tree)?;
        let Object::Tree(tree) = obj else {
            return Ok(None);
        };
        let Some(entry) = tree.entries.iter().find(|e| e.name == comp.as_bytes()) else {
            return Ok(None);
        };
        if entry.mode != EntryMode::Tree {
            return Ok(None);
        }
        cur_tree = entry.id;
    }
    let obj = repo.read_object(&cur_tree)?;
    let Object::Tree(tree) = obj else {
        return Ok(None);
    };
    let Some(entry) = tree.entries.iter().find(|e| e.name == last.as_bytes()) else {
        return Ok(None);
    };
    match entry.mode {
        EntryMode::Regular | EntryMode::Executable | EntryMode::Symlink => Ok(Some(entry.id)),
        _ => Ok(None),
    }
}
