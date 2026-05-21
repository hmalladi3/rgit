//! Spec-anchored tests for the workdir module.

use super::*;
use crate::object::{Blob, Commit, Signature};
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn make_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path(), false).unwrap();
    (dir, repo)
}

fn write_blob_and_tree(repo: &Repository, entries: &[(EntryMode, &[u8], &[u8])]) -> ObjectId {
    let mut tree_entries = Vec::new();
    for (mode, name, data) in entries {
        let blob_id = repo
            .write_object(&Object::Blob(Blob::new(data.to_vec())))
            .unwrap();
        tree_entries.push(TreeEntry {
            mode: *mode,
            name: name.to_vec(),
            id: blob_id,
        });
    }
    let tree = Tree {
        entries: tree_entries,
    };
    repo.write_object(&Object::Tree(tree)).unwrap()
}

// ---------------------------------------------------------------------
// CHECKOUT
// ---------------------------------------------------------------------

// @spec WD-CHECKOUT-001
#[test]
fn checkout_writes_blob_to_working_tree() {
    let (dir, repo) = make_repo();
    let tree_id = write_blob_and_tree(&repo, &[(EntryMode::Regular, b"hello.txt", b"hello world")]);
    let _index = repo.checkout(&tree_id).unwrap();
    let written = std::fs::read(dir.path().join("hello.txt")).unwrap();
    assert_eq!(written, b"hello world");
}

// @spec WD-CHECKOUT-002
#[test]
fn checkout_errors_for_bare_repo() {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path(), true).unwrap();
    let tree_id = write_blob_and_tree(&repo, &[(EntryMode::Regular, b"x", b"x")]);
    assert!(matches!(
        repo.checkout(&tree_id),
        Err(WorkdirError::NoWorkTree)
    ));
}

// @spec WD-CHECKOUT-003
#[test]
fn checkout_sets_executable_permission() {
    let (dir, repo) = make_repo();
    let tree_id = write_blob_and_tree(
        &repo,
        &[(EntryMode::Executable, b"run.sh", b"#!/bin/sh\necho hi\n")],
    );
    repo.checkout(&tree_id).unwrap();
    let meta = std::fs::metadata(dir.path().join("run.sh")).unwrap();
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
}

// @spec WD-CHECKOUT-003
#[test]
fn checkout_sets_regular_permission() {
    let (dir, repo) = make_repo();
    let tree_id = write_blob_and_tree(&repo, &[(EntryMode::Regular, b"plain.txt", b"hello")]);
    repo.checkout(&tree_id).unwrap();
    let mode = std::fs::metadata(dir.path().join("plain.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o644);
}

// @spec WD-CHECKOUT-004
#[test]
fn checkout_creates_symlink_with_target() {
    let (dir, repo) = make_repo();
    let tree_id = write_blob_and_tree(&repo, &[(EntryMode::Symlink, b"link", b"target.txt")]);
    repo.checkout(&tree_id).unwrap();
    let path = dir.path().join("link");
    assert!(std::fs::symlink_metadata(&path).unwrap().is_symlink());
    let target = std::fs::read_link(&path).unwrap();
    assert_eq!(target.to_str().unwrap(), "target.txt");
}

// @spec WD-CHECKOUT-005
#[test]
fn checkout_gitlink_creates_empty_dir_without_recursion() {
    let (dir, repo) = make_repo();
    // Gitlink "id" is just an arbitrary sha; we don't write a sub-repo.
    let mut sub_sha = [0u8; 20];
    sub_sha.fill(0x99);
    let tree = Tree {
        entries: vec![TreeEntry {
            mode: EntryMode::Gitlink,
            name: b"submod".to_vec(),
            id: ObjectId::from_bytes(sub_sha),
        }],
    };
    let tree_id = repo.write_object(&Object::Tree(tree)).unwrap();
    repo.checkout(&tree_id).unwrap();
    let p = dir.path().join("submod");
    assert!(p.is_dir());
    assert_eq!(std::fs::read_dir(&p).unwrap().count(), 0);
}

// @spec WD-CHECKOUT-006
#[test]
fn checkout_returns_populated_index() {
    let (_d, repo) = make_repo();
    let tree_id = write_blob_and_tree(
        &repo,
        &[
            (EntryMode::Regular, b"a.txt", b"a"),
            (EntryMode::Regular, b"b.txt", b"bb"),
        ],
    );
    let index = repo.checkout(&tree_id).unwrap();
    assert_eq!(index.entries().len(), 2);
    assert!(index.lookup(b"a.txt").is_some());
    assert!(index.lookup(b"b.txt").is_some());
}

#[test]
fn checkout_nested_tree() {
    let (dir, repo) = make_repo();
    // Build inner tree.
    let inner_id = write_blob_and_tree(&repo, &[(EntryMode::Regular, b"deep.txt", b"inside")]);
    // Outer tree references inner.
    let outer = Tree {
        entries: vec![TreeEntry {
            mode: EntryMode::Tree,
            name: b"subdir".to_vec(),
            id: inner_id,
        }],
    };
    let outer_id = repo.write_object(&Object::Tree(outer)).unwrap();
    repo.checkout(&outer_id).unwrap();
    let content = std::fs::read(dir.path().join("subdir/deep.txt")).unwrap();
    assert_eq!(content, b"inside");
}

// ---------------------------------------------------------------------
// STATUS
// ---------------------------------------------------------------------

// @spec WD-STATUS-003
#[test]
fn status_reports_untracked_files() {
    let (dir, repo) = make_repo();
    std::fs::write(dir.path().join("new.txt"), b"untracked").unwrap();
    let status = repo.status().unwrap();
    assert!(status
        .changes
        .iter()
        .any(|c| matches!(c, WorkdirChange::Untracked(p) if p == b"new.txt")));
}

// @spec WD-STATUS-001
#[test]
fn status_reports_deleted_files() {
    let (dir, repo) = make_repo();
    let tree_id = write_blob_and_tree(&repo, &[(EntryMode::Regular, b"will-delete.txt", b"hi")]);
    let index = repo.checkout(&tree_id).unwrap();
    repo.write_index(&index).unwrap();
    std::fs::remove_file(dir.path().join("will-delete.txt")).unwrap();
    let status = repo.status().unwrap();
    assert!(status.changes.iter().any(|c| matches!(
        c,
        WorkdirChange::Deleted(p) if p == b"will-delete.txt"
    )));
}

// @spec WD-STATUS-002
#[test]
fn status_reports_modified_files() {
    let (dir, repo) = make_repo();
    let tree_id = write_blob_and_tree(&repo, &[(EntryMode::Regular, b"mod.txt", b"original")]);
    let index = repo.checkout(&tree_id).unwrap();
    repo.write_index(&index).unwrap();
    std::fs::write(dir.path().join("mod.txt"), b"changed bytes").unwrap();
    let status = repo.status().unwrap();
    assert!(status
        .changes
        .iter()
        .any(|c| matches!(c, WorkdirChange::Modified(p) if p == b"mod.txt")));
}

// @spec WD-STATUS-005
#[test]
fn status_skips_git_directory() {
    let (dir, repo) = make_repo();
    // Touch a file in .git/objects — should never show up in status.
    std::fs::write(dir.path().join(".git/objects/marker"), b"x").unwrap();
    let status = repo.status().unwrap();
    for c in &status.changes {
        if let WorkdirChange::Untracked(p) = c {
            assert!(!p.starts_with(b".git/"), "leaked .git path: {p:?}");
        }
    }
}

// ---------------------------------------------------------------------
// BUILD TREE FROM INDEX
// ---------------------------------------------------------------------

// @spec WD-TREE-001, WD-TREE-002
#[test]
fn build_tree_from_index_round_trips_via_checkout() {
    let (_d, repo) = make_repo();
    let original_tree_id = write_blob_and_tree(
        &repo,
        &[
            (EntryMode::Regular, b"a.txt", b"a"),
            (EntryMode::Regular, b"b.txt", b"bb"),
        ],
    );
    let index = repo.checkout(&original_tree_id).unwrap();
    let rebuilt_tree_id = repo.build_tree_from_index(&index).unwrap();
    assert_eq!(rebuilt_tree_id, original_tree_id);
}

// @spec WD-TREE-003
#[test]
fn build_tree_from_index_handles_nested_paths() {
    let (_d, repo) = make_repo();
    // Build an index with a nested path.
    let blob_id = repo
        .write_object(&Object::Blob(Blob::new(b"deep".to_vec())))
        .unwrap();
    let mut index = Index::new();
    index.upsert(IndexEntry {
        ctime: Time::default(),
        mtime: Time::default(),
        dev: 0,
        ino: 0,
        mode: 0o100644,
        uid: 0,
        gid: 0,
        size: 4,
        id: blob_id,
        assume_valid: false,
        stage: 0,
        path: b"dir/sub/file.txt".to_vec(),
    });
    let root_tree_id = repo.build_tree_from_index(&index).unwrap();
    // Read root, expect entry "dir" → tree.
    let Object::Tree(root) = repo.read_object(&root_tree_id).unwrap() else {
        panic!("expected tree");
    };
    assert_eq!(root.entries.len(), 1);
    assert_eq!(root.entries[0].name, b"dir");
    assert_eq!(root.entries[0].mode, EntryMode::Tree);
    // Recurse: "dir" tree has "sub" → tree.
    let Object::Tree(dir) = repo.read_object(&root.entries[0].id).unwrap() else {
        panic!("expected tree");
    };
    assert_eq!(dir.entries[0].name, b"sub");
    let Object::Tree(sub) = repo.read_object(&dir.entries[0].id).unwrap() else {
        panic!("expected tree");
    };
    assert_eq!(sub.entries[0].name, b"file.txt");
    assert_eq!(sub.entries[0].id, blob_id);
}

// @spec WD-MODE-001
#[test]
fn build_tree_encodes_regular_mode() {
    let (_d, repo) = make_repo();
    let blob_id = repo
        .write_object(&Object::Blob(Blob::new(b"x".to_vec())))
        .unwrap();
    let mut index = Index::new();
    index.upsert(IndexEntry {
        ctime: Time::default(),
        mtime: Time::default(),
        dev: 0,
        ino: 0,
        mode: 0o100644,
        uid: 0,
        gid: 0,
        size: 1,
        id: blob_id,
        assume_valid: false,
        stage: 0,
        path: b"plain.txt".to_vec(),
    });
    let tree_id = repo.build_tree_from_index(&index).unwrap();
    let Object::Tree(tree) = repo.read_object(&tree_id).unwrap() else {
        panic!("expected tree");
    };
    assert_eq!(tree.entries[0].mode, EntryMode::Regular);
}

// @spec WD-MODE-002
#[test]
fn build_tree_encodes_executable_mode() {
    let (_d, repo) = make_repo();
    let blob_id = repo
        .write_object(&Object::Blob(Blob::new(b"#!\n".to_vec())))
        .unwrap();
    let mut index = Index::new();
    index.upsert(IndexEntry {
        ctime: Time::default(),
        mtime: Time::default(),
        dev: 0,
        ino: 0,
        mode: 0o100755,
        uid: 0,
        gid: 0,
        size: 3,
        id: blob_id,
        assume_valid: false,
        stage: 0,
        path: b"run.sh".to_vec(),
    });
    let tree_id = repo.build_tree_from_index(&index).unwrap();
    let Object::Tree(tree) = repo.read_object(&tree_id).unwrap() else {
        panic!("expected tree");
    };
    assert_eq!(tree.entries[0].mode, EntryMode::Executable);
}

// @spec WD-MODE-003
#[test]
fn build_tree_encodes_symlink_mode() {
    let (_d, repo) = make_repo();
    let blob_id = repo
        .write_object(&Object::Blob(Blob::new(b"target".to_vec())))
        .unwrap();
    let mut index = Index::new();
    index.upsert(IndexEntry {
        ctime: Time::default(),
        mtime: Time::default(),
        dev: 0,
        ino: 0,
        mode: 0o120000,
        uid: 0,
        gid: 0,
        size: 6,
        id: blob_id,
        assume_valid: false,
        stage: 0,
        path: b"link".to_vec(),
    });
    let tree_id = repo.build_tree_from_index(&index).unwrap();
    let Object::Tree(tree) = repo.read_object(&tree_id).unwrap() else {
        panic!("expected tree");
    };
    assert_eq!(tree.entries[0].mode, EntryMode::Symlink);
}

// ---------------------------------------------------------------------
// Cross-cutting: end-to-end commit-like flow exercising checkout +
// build_tree_from_index + commit writing.
// ---------------------------------------------------------------------

#[test]
fn commit_then_status_clean() {
    let (dir, repo) = make_repo();
    // Write a blob + tree.
    let blob_id = repo
        .write_object(&Object::Blob(Blob::new(b"hello".to_vec())))
        .unwrap();
    let tree = Tree {
        entries: vec![TreeEntry {
            mode: EntryMode::Regular,
            name: b"f.txt".to_vec(),
            id: blob_id,
        }],
    };
    let tree_id = repo.write_object(&Object::Tree(tree)).unwrap();
    // Write a commit referencing the tree.
    let raw_sig = b"Tester <t@e.com> 1700000000 +0000".to_vec();
    let commit = Commit {
        tree: tree_id,
        parents: vec![],
        author: Signature {
            raw: raw_sig.clone(),
            name: Some(b"Tester".to_vec()),
            email: Some(b"t@e.com".to_vec()),
            timestamp: Some(1_700_000_000),
            timezone: Some(b"+0000".to_vec()),
        },
        committer: Signature {
            raw: raw_sig,
            name: Some(b"Tester".to_vec()),
            email: Some(b"t@e.com".to_vec()),
            timestamp: Some(1_700_000_000),
            timezone: Some(b"+0000".to_vec()),
        },
        extra_headers: vec![],
        message: b"initial\n".to_vec(),
    };
    let commit_id = repo.write_object(&Object::Commit(commit)).unwrap();
    repo.write_ref("refs/heads/main", &commit_id).unwrap();
    let index = repo.checkout(&tree_id).unwrap();
    repo.write_index(&index).unwrap();
    let status = repo.status().unwrap();
    assert!(
        status.changes.is_empty(),
        "expected clean status, got {:?}",
        status.changes
    );
    let _ = dir;
}
