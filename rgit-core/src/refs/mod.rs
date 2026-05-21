//! References — branches, tags, remote-tracking refs, HEAD.
//!
//! Owns the namespace of named pointers into the object database. Refs
//! live as either loose files under `git_dir/refs/...` or as entries in
//! `git_dir/packed-refs`. v1 reads both; writes only loose files.

#[cfg(test)]
mod tests;

use crate::object::ObjectId;
use crate::odb::Repository;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

const MAX_SYMREF_DEPTH: usize = 5;

/// What HEAD points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadState {
    /// HEAD points at a branch (e.g., `refs/heads/main`).
    Symbolic(String),
    /// HEAD is detached at a specific commit.
    Detached(ObjectId),
}

/// Errors returned by the refs module.
#[derive(Debug, Error)]
pub enum RefError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid ref name: {0:?}")]
    InvalidRefName(String),

    #[error("invalid ref content for {0:?}")]
    InvalidRefContent(String),

    #[error("ref not found: {0:?}")]
    RefNotFound(String),

    #[error("symbolic ref chain exceeds limit ({0})")]
    SymRefLoop(usize),

    #[error("cannot delete ref present only in packed-refs: {0:?}")]
    CannotDeletePackedOnly(String),

    #[error("cannot delete HEAD")]
    CannotDeleteHead,
}

impl Repository {
    /// Read a ref by full name (e.g., `refs/heads/main`, `HEAD`,
    /// `refs/tags/v1.0`). Resolves symbolic refs recursively.
    // @spec REFS-READ-001, REFS-READ-002, REFS-READ-003, REFS-READ-004,
    //       REFS-READ-005, REFS-NAME-008
    pub fn read_ref(&self, name: &str) -> Result<ObjectId, RefError> {
        validate_ref_name(name)?;
        self.read_ref_at_depth(name, 0)
    }

    fn read_ref_at_depth(&self, name: &str, depth: usize) -> Result<ObjectId, RefError> {
        if depth > MAX_SYMREF_DEPTH {
            return Err(RefError::SymRefLoop(MAX_SYMREF_DEPTH));
        }

        let path = self.git_dir().join(name);
        match fs::read_to_string(&path) {
            Ok(content) => {
                let trimmed = content.strip_suffix('\n').unwrap_or(&content);
                if trimmed.is_empty() {
                    return Err(RefError::InvalidRefContent(name.to_string()));
                }
                if let Some(target) = trimmed.strip_prefix("ref: ") {
                    validate_ref_name(target)?;
                    return self.read_ref_at_depth(target, depth + 1);
                }
                ObjectId::from_hex(trimmed)
                    .map_err(|_| RefError::InvalidRefContent(name.to_string()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => self
                .packed_refs
                .get(name)
                .copied()
                .ok_or_else(|| RefError::RefNotFound(name.to_string())),
            Err(e) => Err(RefError::Io(e)),
        }
    }

    /// Read HEAD as either a symbolic ref or a detached id.
    // @spec REFS-HEAD-001, REFS-HEAD-002, REFS-HEAD-003, REFS-HEAD-004
    pub fn read_head(&self) -> Result<HeadState, RefError> {
        let path = self.git_dir().join("HEAD");
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(RefError::RefNotFound("HEAD".to_string()))
            }
            Err(e) => return Err(RefError::Io(e)),
        };
        let trimmed = content.strip_suffix('\n').unwrap_or(&content);
        if trimmed.is_empty() {
            return Err(RefError::InvalidRefContent("HEAD".to_string()));
        }
        if let Some(target) = trimmed.strip_prefix("ref: ") {
            return Ok(HeadState::Symbolic(target.to_string()));
        }
        let id = ObjectId::from_hex(trimmed)
            .map_err(|_| RefError::InvalidRefContent("HEAD".to_string()))?;
        Ok(HeadState::Detached(id))
    }

    /// Atomically write a loose ref.
    // @spec REFS-WRITE-001, REFS-WRITE-002, REFS-WRITE-003, REFS-WRITE-004,
    //       REFS-LOOSE-001, REFS-LOOSE-002, REFS-LOOSE-004, REFS-NAME-008
    pub fn write_ref(&self, name: &str, id: &ObjectId) -> Result<(), RefError> {
        validate_ref_name(name)?;
        let path = self.git_dir().join(name);
        let parent = path
            .parent()
            .ok_or_else(|| RefError::InvalidRefName(name.to_string()))?;
        fs::create_dir_all(parent)?;
        let contents = format!("{}\n", id.to_hex());
        atomic_write(&path, contents.as_bytes())?;
        Ok(())
    }

    /// Point HEAD at a branch ref.
    // @spec REFS-HEAD-005, REFS-HEAD-007
    pub fn set_head_symbolic(&self, target: &str) -> Result<(), RefError> {
        if !target.starts_with("refs/") {
            return Err(RefError::InvalidRefName(target.to_string()));
        }
        validate_ref_name(target)?;
        let path = self.git_dir().join("HEAD");
        let contents = format!("ref: {target}\n");
        atomic_write(&path, contents.as_bytes())?;
        Ok(())
    }

    /// Detach HEAD at a specific commit.
    // @spec REFS-HEAD-006
    pub fn set_head_detached(&self, id: &ObjectId) -> Result<(), RefError> {
        let path = self.git_dir().join("HEAD");
        let contents = format!("{}\n", id.to_hex());
        atomic_write(&path, contents.as_bytes())?;
        Ok(())
    }

    /// Remove a loose ref file.
    // @spec REFS-DELETE-001, REFS-DELETE-002, REFS-DELETE-003, REFS-DELETE-004
    pub fn delete_ref(&self, name: &str) -> Result<(), RefError> {
        if name == "HEAD" {
            return Err(RefError::CannotDeleteHead);
        }
        validate_ref_name(name)?;
        let path = self.git_dir().join(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if self.packed_refs.contains_key(name) {
                    Err(RefError::CannotDeletePackedOnly(name.to_string()))
                } else {
                    Err(RefError::RefNotFound(name.to_string()))
                }
            }
            Err(e) => Err(RefError::Io(e)),
        }
    }

    /// List every ref whose name starts with `prefix`. Loose entries
    /// shadow packed entries with the same name.
    // @spec REFS-LIST-001, REFS-LIST-002, REFS-LIST-003, REFS-LIST-004
    pub fn list_refs(&self, prefix: &str) -> Result<Vec<(String, ObjectId)>, RefError> {
        let mut out: HashMap<String, ObjectId> = HashMap::new();

        // Packed refs first; loose will shadow.
        for (name, id) in &self.packed_refs {
            if name.starts_with(prefix) {
                out.insert(name.clone(), *id);
            }
        }

        // Loose refs.
        let refs_dir = self.git_dir().join("refs");
        if refs_dir.is_dir() {
            collect_loose_refs(&refs_dir, self.git_dir(), prefix, &mut out)?;
        }

        let mut v: Vec<_> = out.into_iter().collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(v)
    }
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Load `git_dir/packed-refs` into a name→id map. Skips malformed lines
/// silently per REFS-PACKED-003. Returns an empty map if the file is
/// absent.
// @spec REFS-PACKED-001, REFS-PACKED-002, REFS-PACKED-003,
//       REFS-PACKED-004, REFS-PACKED-005
pub(crate) fn load_packed_refs(git_dir: &Path) -> Result<HashMap<String, ObjectId>, RefError> {
    let path = git_dir.join("packed-refs");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(RefError::Io(e)),
    };

    let mut map = HashMap::new();
    for line in content.lines() {
        // Skip blanks, comments, and peeled-tag hint lines.
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let Some((hex, name)) = line.split_once(' ') else {
            continue;
        };
        let Ok(id) = ObjectId::from_hex(hex) else {
            continue;
        };
        if validate_ref_name(name).is_err() {
            continue;
        }
        map.insert(name.to_string(), id);
    }
    Ok(map)
}

/// Validate a ref name. Applied on both read and write so path-traversal
/// attempts (`"../etc/passwd"`) cannot escape the git directory.
// @spec REFS-NAME-001, REFS-NAME-002, REFS-NAME-003, REFS-NAME-004,
//       REFS-NAME-005, REFS-NAME-006, REFS-NAME-007, REFS-NAME-008
fn validate_ref_name(name: &str) -> Result<(), RefError> {
    if name == "HEAD" {
        return Ok(());
    }
    if name.is_empty() {
        return Err(RefError::InvalidRefName(name.to_string()));
    }
    let bytes = name.as_bytes();
    if bytes[0] == b'-' || bytes[0] == b'.' {
        return Err(RefError::InvalidRefName(name.to_string()));
    }
    if bytes[0] == b'/' {
        return Err(RefError::InvalidRefName(name.to_string()));
    }
    if bytes[bytes.len() - 1] == b'/' || name.ends_with(".lock") {
        return Err(RefError::InvalidRefName(name.to_string()));
    }
    if name.contains("..") || name.contains("//") {
        return Err(RefError::InvalidRefName(name.to_string()));
    }
    for &b in bytes {
        match b {
            b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\' => {
                return Err(RefError::InvalidRefName(name.to_string()));
            }
            b' ' => return Err(RefError::InvalidRefName(name.to_string())),
            b if b < 0x20 || b == 0x7f => {
                return Err(RefError::InvalidRefName(name.to_string()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Write `contents` to `path` atomically: tmp file in the parent
/// directory, fsync, rename, dir fsync.
fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), RefError> {
    let parent = path
        .parent()
        .ok_or_else(|| RefError::Io(io::Error::other("ref path has no parent")))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(tmp_name());
    {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(RefError::Io(e));
    }
    if let Ok(d) = fs::File::open(parent) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn tmp_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("tmp-ref-{}-{}", std::process::id(), n)
}

/// Recursively collect loose ref files under `dir`, adding any whose
/// name (relative to `git_dir`) starts with `prefix` to `out`. Skips
/// symbolic refs (their target lookup belongs to `read_ref`, not
/// enumeration).
fn collect_loose_refs(
    dir: &Path,
    git_dir: &Path,
    prefix: &str,
    out: &mut HashMap<String, ObjectId>,
) -> Result<(), RefError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_loose_refs(&path, git_dir, prefix, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(git_dir) else {
            continue;
        };
        let name = rel
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if !name.starts_with(prefix) {
            continue;
        }
        let content = fs::read_to_string(&path)?;
        let trimmed = content.strip_suffix('\n').unwrap_or(&content);
        if trimmed.is_empty() || trimmed.starts_with("ref: ") {
            continue;
        }
        if let Ok(id) = ObjectId::from_hex(trimmed) {
            out.insert(name, id);
        }
    }
    Ok(())
}
