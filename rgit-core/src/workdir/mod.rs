//! Working tree — operations bridging in-memory objects and on-disk files.
//!
//! v1: checkout (used by `clone`), status (used by the `status` CLI),
//! build_tree_from_index (used by `commit`). No textual diff and no
//! gitignore parsing in v1; both are deferred.

#[cfg(test)]
mod tests;

use crate::index::{Index, IndexEntry, Time};
use crate::object::{EntryMode, Object, ObjectId, ObjectKind, ParseError, Tree, TreeEntry};
use crate::odb::{OdbError, Repository};
use crate::refs::RefError;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkdirChange {
    Deleted(Vec<u8>),
    Modified(Vec<u8>),
    Untracked(Vec<u8>),
    Staged(Vec<u8>),
}

#[derive(Debug, Default)]
pub struct WorkdirStatus {
    pub changes: Vec<WorkdirChange>,
}

#[derive(Debug, Error)]
pub enum WorkdirError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("object error: {0}")]
    Object(#[from] ParseError),

    #[error("odb error: {0}")]
    Odb(#[from] OdbError),

    #[error("refs error: {0}")]
    Refs(#[from] RefError),

    #[error("index error: {0}")]
    Index(#[from] crate::index::IndexError),

    #[error("repository has no working tree")]
    NoWorkTree,

    #[error("unexpected object kind: {0:?}")]
    UnexpectedKind(ObjectKind),
}

impl Repository {
    /// Materialize a tree into the working tree. Returns a populated
    /// `Index` reflecting every file written.
    // @spec WD-CHECKOUT-001, WD-CHECKOUT-002, WD-CHECKOUT-003,
    //       WD-CHECKOUT-004, WD-CHECKOUT-005, WD-CHECKOUT-006
    pub fn checkout(&self, tree_id: &ObjectId) -> Result<Index, WorkdirError> {
        let work_dir = self
            .work_dir()
            .ok_or(WorkdirError::NoWorkTree)?
            .to_path_buf();
        let mut index = Index::new();
        checkout_tree_recursive(self, tree_id, &work_dir, b"", &mut index)?;
        Ok(index)
    }

    /// Report differences between HEAD-tree, index, and working tree.
    // @spec WD-STATUS-001, WD-STATUS-002, WD-STATUS-003, WD-STATUS-004,
    //       WD-STATUS-005
    pub fn status(&self) -> Result<WorkdirStatus, WorkdirError> {
        let work_dir = self
            .work_dir()
            .ok_or(WorkdirError::NoWorkTree)?
            .to_path_buf();
        let index = self.read_index()?;

        let head_tree_paths = match resolve_head_tree(self)? {
            Some(tree_id) => load_tree_paths(self, &tree_id)?,
            None => HashMap::new(),
        };

        let mut changes: Vec<WorkdirChange> = Vec::new();
        let mut index_paths: HashSet<Vec<u8>> = HashSet::new();

        for entry in index.entries() {
            if entry.stage != 0 {
                continue;
            }
            index_paths.insert(entry.path.clone());

            let path_str = std::str::from_utf8(&entry.path).unwrap_or("");
            let abs_path = work_dir.join(path_str);
            match fs::symlink_metadata(&abs_path) {
                Ok(_) => {
                    let bytes = if abs_path.is_symlink() {
                        fs::read_link(&abs_path)?
                            .to_string_lossy()
                            .into_owned()
                            .into_bytes()
                    } else {
                        fs::read(&abs_path)?
                    };
                    let computed = ObjectId::compute(ObjectKind::Blob, &bytes);
                    if computed != entry.id {
                        changes.push(WorkdirChange::Modified(entry.path.clone()));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    changes.push(WorkdirChange::Deleted(entry.path.clone()));
                }
                Err(e) => return Err(WorkdirError::Io(e)),
            }

            match head_tree_paths.get(&entry.path) {
                Some(head_id) if *head_id == entry.id => {}
                _ => changes.push(WorkdirChange::Staged(entry.path.clone())),
            }
        }

        walk_workdir(&work_dir, b"", &index_paths, &mut changes)?;
        Ok(WorkdirStatus { changes })
    }

    /// Build a tree object hierarchy from the current index and return
    /// the root tree's id.
    // @spec WD-TREE-001, WD-TREE-002, WD-TREE-003,
    //       WD-MODE-001, WD-MODE-002, WD-MODE-003
    pub fn build_tree_from_index(&self, index: &Index) -> Result<ObjectId, WorkdirError> {
        let mut root = TreeNode::default();
        for entry in index.entries() {
            if entry.stage != 0 {
                continue;
            }
            root.insert(&entry.path, entry);
        }
        write_tree_node(&root, self)
    }
}

#[derive(Default)]
struct TreeNode {
    files: Vec<(Vec<u8>, IndexEntry)>,
    dirs: HashMap<Vec<u8>, TreeNode>,
}

impl TreeNode {
    fn insert(&mut self, path: &[u8], entry: &IndexEntry) {
        let mut parts: Vec<&[u8]> = path.split(|&b| b == b'/').collect();
        let Some(leaf) = parts.pop() else { return };
        let mut node = self;
        for part in parts {
            node = node.dirs.entry(part.to_vec()).or_default();
        }
        node.files.push((leaf.to_vec(), entry.clone()));
    }
}

fn write_tree_node(node: &TreeNode, repo: &Repository) -> Result<ObjectId, WorkdirError> {
    let mut entries: Vec<TreeEntry> = Vec::new();
    for (name, entry) in &node.files {
        let mode = mode_for_index_entry(entry.mode);
        entries.push(TreeEntry {
            mode,
            name: name.clone(),
            id: entry.id,
        });
    }
    let mut sorted_dirs: Vec<(&Vec<u8>, &TreeNode)> = node.dirs.iter().collect();
    sorted_dirs.sort_by(|a, b| a.0.cmp(b.0));
    for (name, child) in sorted_dirs {
        let child_id = write_tree_node(child, repo)?;
        entries.push(TreeEntry {
            mode: EntryMode::Tree,
            name: name.clone(),
            id: child_id,
        });
    }
    let tree = Tree { entries };
    let id = repo.write_object(&Object::Tree(tree))?;
    Ok(id)
}

fn mode_for_index_entry(index_mode: u32) -> EntryMode {
    match index_mode & 0o170000 {
        0o100000 => {
            if index_mode & 0o111 != 0 {
                EntryMode::Executable
            } else {
                EntryMode::Regular
            }
        }
        0o120000 => EntryMode::Symlink,
        0o160000 => EntryMode::Gitlink,
        _ => EntryMode::Regular,
    }
}

fn checkout_tree_recursive(
    repo: &Repository,
    tree_id: &ObjectId,
    work_dir: &Path,
    rel_prefix: &[u8],
    index: &mut Index,
) -> Result<(), WorkdirError> {
    let obj = repo.read_object(tree_id)?;
    let Object::Tree(tree) = obj else {
        return Err(WorkdirError::UnexpectedKind(obj.kind()));
    };

    let prefix_dir = if rel_prefix.is_empty() {
        work_dir.to_path_buf()
    } else {
        let s = std::str::from_utf8(rel_prefix).unwrap_or("");
        work_dir.join(s)
    };
    fs::create_dir_all(&prefix_dir)?;

    for entry in &tree.entries {
        let name_str = std::str::from_utf8(&entry.name).unwrap_or("");
        let file_path = prefix_dir.join(name_str);
        let mut rel_path = rel_prefix.to_vec();
        if !rel_path.is_empty() {
            rel_path.push(b'/');
        }
        rel_path.extend_from_slice(&entry.name);

        match entry.mode {
            EntryMode::Regular | EntryMode::Executable => {
                let blob_obj = repo.read_object(&entry.id)?;
                let Object::Blob(blob) = blob_obj else {
                    return Err(WorkdirError::UnexpectedKind(blob_obj.kind()));
                };
                fs::write(&file_path, &blob.data)?;
                let perms_mode = if entry.mode == EntryMode::Executable {
                    0o755
                } else {
                    0o644
                };
                fs::set_permissions(&file_path, fs::Permissions::from_mode(perms_mode))?;
                let meta = fs::metadata(&file_path)?;
                let stat_mode = if entry.mode == EntryMode::Executable {
                    0o100755
                } else {
                    0o100644
                };
                index.upsert(IndexEntry {
                    ctime: time_from_meta(&meta, true),
                    mtime: time_from_meta(&meta, false),
                    dev: 0,
                    ino: 0,
                    mode: stat_mode,
                    uid: 0,
                    gid: 0,
                    size: meta.len() as u32,
                    id: entry.id,
                    assume_valid: false,
                    stage: 0,
                    path: rel_path.clone(),
                });
            }
            EntryMode::Symlink => {
                let blob_obj = repo.read_object(&entry.id)?;
                let Object::Blob(blob) = blob_obj else {
                    return Err(WorkdirError::UnexpectedKind(blob_obj.kind()));
                };
                let target = std::str::from_utf8(&blob.data).unwrap_or("");
                let _ = fs::remove_file(&file_path);
                std::os::unix::fs::symlink(target, &file_path)?;
                let meta = fs::symlink_metadata(&file_path)?;
                index.upsert(IndexEntry {
                    ctime: time_from_meta(&meta, true),
                    mtime: time_from_meta(&meta, false),
                    dev: 0,
                    ino: 0,
                    mode: 0o120000,
                    uid: 0,
                    gid: 0,
                    size: blob.data.len() as u32,
                    id: entry.id,
                    assume_valid: false,
                    stage: 0,
                    path: rel_path.clone(),
                });
            }
            EntryMode::Tree => {
                checkout_tree_recursive(repo, &entry.id, work_dir, &rel_path, index)?;
            }
            EntryMode::Gitlink => {
                fs::create_dir_all(&file_path)?;
            }
        }
    }
    Ok(())
}

fn time_from_meta(meta: &fs::Metadata, ctime: bool) -> Time {
    use std::os::unix::fs::MetadataExt;
    let (secs, nanos) = if ctime {
        (meta.ctime(), meta.ctime_nsec())
    } else {
        (meta.mtime(), meta.mtime_nsec())
    };
    Time {
        secs: secs as u32,
        nanos: nanos as u32,
    }
}

fn resolve_head_tree(repo: &Repository) -> Result<Option<ObjectId>, WorkdirError> {
    let commit_id = match repo.read_ref("HEAD") {
        Ok(id) => id,
        Err(RefError::RefNotFound(_)) => return Ok(None),
        Err(e) => return Err(WorkdirError::Refs(e)),
    };
    let obj = repo.read_object(&commit_id)?;
    match obj {
        Object::Commit(c) => Ok(Some(c.tree)),
        _ => Ok(None),
    }
}

fn load_tree_paths(
    repo: &Repository,
    tree_id: &ObjectId,
) -> Result<HashMap<Vec<u8>, ObjectId>, WorkdirError> {
    let mut out = HashMap::new();
    walk_tree_paths(repo, tree_id, b"", &mut out)?;
    Ok(out)
}

fn walk_tree_paths(
    repo: &Repository,
    tree_id: &ObjectId,
    prefix: &[u8],
    out: &mut HashMap<Vec<u8>, ObjectId>,
) -> Result<(), WorkdirError> {
    let obj = repo.read_object(tree_id)?;
    let Object::Tree(tree) = obj else {
        return Err(WorkdirError::UnexpectedKind(obj.kind()));
    };
    for entry in &tree.entries {
        let mut path = prefix.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&entry.name);
        match entry.mode {
            EntryMode::Tree => walk_tree_paths(repo, &entry.id, &path, out)?,
            _ => {
                out.insert(path, entry.id);
            }
        }
    }
    Ok(())
}

fn walk_workdir(
    dir: &Path,
    rel: &[u8],
    index_paths: &HashSet<Vec<u8>>,
    changes: &mut Vec<WorkdirChange>,
) -> Result<(), WorkdirError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_bytes = name.to_string_lossy().into_owned().into_bytes();
        if name_bytes == b".git" {
            continue;
        }
        let path = entry.path();
        let mut rel_path = rel.to_vec();
        if !rel_path.is_empty() {
            rel_path.push(b'/');
        }
        rel_path.extend_from_slice(&name_bytes);

        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_workdir(&path, &rel_path, index_paths, changes)?;
        } else if !index_paths.contains(&rel_path) {
            changes.push(WorkdirChange::Untracked(rel_path));
        }
    }
    Ok(())
}

/// Silence the unused-PathBuf-import warning when symlink paths happen to
/// not exercise the import on all targets.
#[allow(dead_code)]
fn _path_buf_marker(_: &PathBuf) {}
