//! Tests for fast-forward merge.

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
    tree: ObjectId,
    parents: Vec<ObjectId>,
    msg: &[u8],
) -> ObjectId {
    let sig = Signature {
        raw: b"t <t@e.com> 1700000000 +0000".to_vec(),
        name: Some(b"t".to_vec()),
        email: Some(b"t@e.com".to_vec()),
        timestamp: Some(1_700_000_000),
        timezone: Some(b"+0000".to_vec()),
    };
    repo.write_object(&Object::Commit(Commit {
        tree,
        parents,
        author: sig.clone(),
        committer: sig,
        extra_headers: vec![],
        message: msg.to_vec(),
    }))
    .unwrap()
}

fn write_tree_with_blob(repo: &Repository, name: &[u8], content: &[u8]) -> ObjectId {
    let blob_id = repo
        .write_object(&Object::Blob(Blob::new(content.to_vec())))
        .unwrap();
    let tree = Tree {
        entries: vec![TreeEntry {
            mode: EntryMode::Regular,
            name: name.to_vec(),
            id: blob_id,
        }],
    };
    repo.write_object(&Object::Tree(tree)).unwrap()
}

#[test]
fn fast_forward_advances_head_to_descendant() {
    let (_d, repo) = make_repo();
    let tree_a = write_tree_with_blob(&repo, b"f.txt", b"a");
    let c1 = write_commit(&repo, tree_a, vec![], b"a\n");
    let tree_b = write_tree_with_blob(&repo, b"f.txt", b"b");
    let c2 = write_commit(&repo, tree_b, vec![c1], b"b\n");
    repo.write_ref("refs/heads/main", &c1).unwrap();
    // HEAD points at main → c1; target is c2 (descendant).
    let result = repo.merge_fast_forward(&c2).unwrap();
    match result {
        MergeResult::FastForwarded { from, to } => {
            assert_eq!(from, c1);
            assert_eq!(to, c2);
        }
        other => panic!("expected FastForwarded, got {other:?}"),
    }
    // Branch ref now points at c2.
    assert_eq!(repo.read_ref("refs/heads/main").unwrap(), c2);
}

#[test]
fn up_to_date_when_target_equals_head() {
    let (_d, repo) = make_repo();
    let tree = write_tree_with_blob(&repo, b"f.txt", b"x");
    let c1 = write_commit(&repo, tree, vec![], b"only\n");
    repo.write_ref("refs/heads/main", &c1).unwrap();
    assert_eq!(repo.merge_fast_forward(&c1).unwrap(), MergeResult::UpToDate);
}

#[test]
fn non_fast_forward_rejected_for_divergent_history() {
    let (_d, repo) = make_repo();
    let tree_root = write_tree_with_blob(&repo, b"f.txt", b"root");
    let c1 = write_commit(&repo, tree_root, vec![], b"root\n");
    // Diverge: c2a and c2b both have c1 as parent (siblings, not ancestor/descendant).
    let tree_a = write_tree_with_blob(&repo, b"f.txt", b"a");
    let c2a = write_commit(&repo, tree_a, vec![c1], b"branch a\n");
    let tree_b = write_tree_with_blob(&repo, b"f.txt", b"b");
    let c2b = write_commit(&repo, tree_b, vec![c1], b"branch b\n");
    repo.write_ref("refs/heads/main", &c2a).unwrap();
    let err = repo.merge_fast_forward(&c2b).unwrap_err();
    assert!(matches!(err, MergeError::NonFastForward));
    // Branch ref is unchanged.
    assert_eq!(repo.read_ref("refs/heads/main").unwrap(), c2a);
}

#[test]
fn non_fast_forward_rejected_when_target_is_ancestor() {
    // HEAD is at c2; target is c1 (HEAD's parent). This is "rewinding,"
    // not fast-forwarding. Currently we reject as non-FF (matches what
    // git does without `--force`).
    let (_d, repo) = make_repo();
    let tree_root = write_tree_with_blob(&repo, b"f.txt", b"root");
    let c1 = write_commit(&repo, tree_root, vec![], b"root\n");
    let tree_2 = write_tree_with_blob(&repo, b"f.txt", b"second");
    let c2 = write_commit(&repo, tree_2, vec![c1], b"second\n");
    repo.write_ref("refs/heads/main", &c2).unwrap();
    let err = repo.merge_fast_forward(&c1).unwrap_err();
    assert!(matches!(err, MergeError::NonFastForward));
}
