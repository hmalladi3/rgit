//! Tests for the blame module.

use super::*;
use crate::object::{Blob, Commit, EntryMode, Object, Signature, Tree, TreeEntry};
use tempfile::TempDir;

fn make_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path(), false).unwrap();
    (dir, repo)
}

fn write_commit(
    repo: &Repository,
    file_name: &[u8],
    content: &[u8],
    parents: Vec<ObjectId>,
) -> ObjectId {
    let blob_id = repo
        .write_object(&Object::Blob(Blob::new(content.to_vec())))
        .unwrap();
    let tree = Tree {
        entries: vec![TreeEntry {
            mode: EntryMode::Regular,
            name: file_name.to_vec(),
            id: blob_id,
        }],
    };
    let tree_id = repo.write_object(&Object::Tree(tree)).unwrap();
    let sig = Signature {
        raw: b"t <t@e.com> 1700000000 +0000".to_vec(),
        name: Some(b"t".to_vec()),
        email: Some(b"t@e.com".to_vec()),
        timestamp: Some(1_700_000_000),
        timezone: Some(b"+0000".to_vec()),
    };
    repo.write_object(&Object::Commit(Commit {
        tree: tree_id,
        parents,
        author: sig.clone(),
        committer: sig,
        extra_headers: vec![],
        message: b"msg\n".to_vec(),
    }))
    .unwrap()
}

#[test]
fn blame_attributes_introducing_commit_for_each_line() {
    let (_d, repo) = make_repo();
    // c1: "a\n"
    let c1 = write_commit(&repo, b"f.txt", b"a\n", vec![]);
    // c2: "a\nb\n"
    let c2 = write_commit(&repo, b"f.txt", b"a\nb\n", vec![c1]);
    // c3: "a\nb\nc\n"
    let c3 = write_commit(&repo, b"f.txt", b"a\nb\nc\n", vec![c2]);

    let result = repo.blame("f.txt", &c3).unwrap();
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].commit, c1);
    assert_eq!(result[0].line, b"a\n");
    assert_eq!(result[1].commit, c2);
    assert_eq!(result[1].line, b"b\n");
    assert_eq!(result[2].commit, c3);
    assert_eq!(result[2].line, b"c\n");
}

#[test]
fn blame_handles_modified_middle_line() {
    let (_d, repo) = make_repo();
    let c1 = write_commit(&repo, b"f.txt", b"alpha\nbeta\ngamma\n", vec![]);
    let c2 = write_commit(&repo, b"f.txt", b"alpha\nBETA\ngamma\n", vec![c1]);

    let result = repo.blame("f.txt", &c2).unwrap();
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].commit, c1); // alpha — original
    assert_eq!(result[1].commit, c2); // BETA — modified
    assert_eq!(result[2].commit, c1); // gamma — original
}

#[test]
fn blame_attributes_root_when_file_unchanged_through_history() {
    let (_d, repo) = make_repo();
    let c1 = write_commit(&repo, b"f.txt", b"line\n", vec![]);
    // c2: same content, just a different commit.
    let c2 = write_commit(&repo, b"f.txt", b"line\n", vec![c1]);
    let result = repo.blame("f.txt", &c2).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].commit, c1);
}

#[test]
fn blame_file_missing_returns_error() {
    let (_d, repo) = make_repo();
    let c1 = write_commit(&repo, b"other.txt", b"x\n", vec![]);
    let result = repo.blame("nope.txt", &c1);
    assert!(matches!(result, Err(BlameError::FileNotFound(_))));
}
