//! Merge — fast-forward only in v1.
//!
//! A merge is fast-forward when the local HEAD is an ancestor of the
//! target commit. The merge then collapses to "move HEAD's branch to
//! the target." No three-way text merge, no rename detection, no
//! conflict markers — those land with the deferred 3-way merge work.

#[cfg(test)]
mod tests;

use crate::object::{Object, ObjectId};
use crate::odb::{OdbError, Repository};
use crate::refs::{HeadState, RefError};
use std::collections::HashSet;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    /// Target is the same as HEAD; no work to do.
    UpToDate,
    /// HEAD's branch was advanced to the target commit.
    FastForwarded { from: ObjectId, to: ObjectId },
}

#[derive(Debug, Error)]
pub enum MergeError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("odb error: {0}")]
    Odb(#[from] OdbError),

    #[error("refs error: {0}")]
    Refs(#[from] RefError),

    #[error("workdir error: {0}")]
    Workdir(#[from] crate::workdir::WorkdirError),

    #[error("index error: {0}")]
    Index(#[from] crate::index::IndexError),

    #[error("object error: {0}")]
    Object(#[from] crate::object::ParseError),

    #[error("target is not a commit")]
    NotACommit,

    #[error(
        "non-fast-forward merge required (target diverges from HEAD); \
        v1 supports fast-forward only — three-way merge is deferred"
    )]
    NonFastForward,
}

impl Repository {
    /// Fast-forward HEAD's current branch to `target`.
    ///
    /// Three cases:
    /// - `target == HEAD`: returns `MergeResult::UpToDate` with no work.
    /// - HEAD is an ancestor of `target`: updates HEAD's branch ref to
    ///   `target`, checks out `target`'s tree, returns `FastForwarded`.
    /// - HEAD diverges from `target`: returns `Err(NonFastForward)`.
    pub fn merge_fast_forward(&self, target: &ObjectId) -> Result<MergeResult, MergeError> {
        let head_id = self.read_ref("HEAD")?;
        if head_id == *target {
            return Ok(MergeResult::UpToDate);
        }
        if !is_ancestor(self, &head_id, target)? {
            return Err(MergeError::NonFastForward);
        }

        // Resolve target's tree.
        let target_obj = self.read_object(target)?;
        let target_commit = match target_obj {
            Object::Commit(c) => c,
            _ => return Err(MergeError::NotACommit),
        };

        // Remember which paths the old index tracked so we can remove
        // files that don't exist in the new tree.
        let old_paths: HashSet<Vec<u8>> = self
            .read_index()?
            .entries()
            .iter()
            .filter(|e| e.stage == 0)
            .map(|e| e.path.clone())
            .collect();

        let new_index = self.checkout(&target_commit.tree)?;
        let new_paths: HashSet<Vec<u8>> = new_index
            .entries()
            .iter()
            .filter(|e| e.stage == 0)
            .map(|e| e.path.clone())
            .collect();
        if let Some(work_dir) = self.work_dir() {
            for stale in old_paths.difference(&new_paths) {
                let path_str = std::str::from_utf8(stale).unwrap_or("");
                let _ = std::fs::remove_file(work_dir.join(path_str));
            }
        }
        self.write_index(&new_index)?;

        // Advance HEAD's branch ref (or HEAD itself if detached).
        match self.read_head()? {
            HeadState::Symbolic(branch) => self.write_ref(&branch, target)?,
            HeadState::Detached(_) => self.set_head_detached(target)?,
        }

        Ok(MergeResult::FastForwarded {
            from: head_id,
            to: *target,
        })
    }
}

/// True if `ancestor` appears in the commit graph reachable from
/// `descendant` (i.e., `descendant` is at or below `ancestor` in the
/// DAG, so HEAD@ancestor can fast-forward to descendant).
fn is_ancestor(
    repo: &Repository,
    ancestor: &ObjectId,
    descendant: &ObjectId,
) -> Result<bool, MergeError> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mut to_visit: Vec<ObjectId> = vec![*descendant];
    let mut visited: HashSet<ObjectId> = HashSet::new();
    while let Some(id) = to_visit.pop() {
        if !visited.insert(id) {
            continue;
        }
        let obj = repo.read_object(&id)?;
        if let Object::Commit(c) = obj {
            for parent in c.parents {
                if parent == *ancestor {
                    return Ok(true);
                }
                if !visited.contains(&parent) {
                    to_visit.push(parent);
                }
            }
        }
    }
    Ok(false)
}
