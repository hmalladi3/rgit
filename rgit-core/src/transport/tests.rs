//! Spec-anchored tests for the transport module.
//!
//! HTTP-level tests against a real remote belong to integration tests
//! and live outside the unit suite. These tests cover the pure pieces:
//! pkt-line encode/decode, reachability walks, ref-advertisement parsing.

use super::*;
use crate::object::{Blob, Commit, Object, Signature, Tree, TreeEntry};
use tempfile::TempDir;

fn make_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path(), false).unwrap();
    (dir, repo)
}

// ---------------------------------------------------------------------
// pkt-line
// ---------------------------------------------------------------------

// @spec TX-PKTLINE-001
#[test]
fn pkt_line_encode_includes_length_prefix() {
    let encoded = pkt_line_encode(b"hello");
    // "0009" = 9 bytes total (4 prefix + 5 data)
    assert_eq!(encoded, b"0009hello");
}

// @spec TX-PKTLINE-001
#[test]
fn pkt_line_encode_zero_byte_data() {
    let encoded = pkt_line_encode(b"");
    // "0004" = 4 bytes total (just the prefix)
    assert_eq!(encoded, b"0004");
}

// @spec TX-PKTLINE-002
#[test]
fn pkt_line_flush_is_literal_0000() {
    assert_eq!(pkt_line_flush(), b"0000");
}

// @spec TX-PKTLINE-003, TX-PKTLINE-004
#[test]
fn pkt_line_decode_round_trip() {
    let mut stream = Vec::new();
    stream.extend_from_slice(&pkt_line_encode(b"first"));
    stream.extend_from_slice(&pkt_line_encode(b"second packet"));
    stream.extend_from_slice(pkt_line_flush());
    stream.extend_from_slice(&pkt_line_encode(b"after-flush"));
    let pkts = pkt_line_decode_all(&stream).unwrap();
    assert_eq!(pkts.len(), 3);
    assert_eq!(pkts[0], b"first");
    assert_eq!(pkts[1], b"second packet");
    assert_eq!(pkts[2], b"after-flush");
}

// @spec TX-PKTLINE-003
#[test]
fn pkt_line_decode_rejects_invalid_hex() {
    let bytes = b"ZZZZpayload";
    assert!(matches!(
        pkt_line_decode_all(bytes),
        Err(TransportError::PktLine)
    ));
}

// ---------------------------------------------------------------------
// Ref advertisement parsing
// ---------------------------------------------------------------------

// @spec TX-LSREF-002, TX-LSREF-003
#[test]
fn ref_advertisement_parses_service_and_refs() {
    // Synthesize a server response:
    //   001f# service=git-receive-pack\n
    //   0000
    //   <pkt with "version 2\n">      (some servers include this)
    //   <pkt with "<sha> <ref>\0caps">
    //   <pkt with "<sha> <ref>">
    //   0000
    let mut body = Vec::new();
    body.extend_from_slice(&pkt_line_encode(b"# service=git-receive-pack\n"));
    body.extend_from_slice(pkt_line_flush());
    let ref1 = format!(
        "{} {}\0report-status delete-refs\n",
        "0".repeat(40),
        "refs/heads/main",
    );
    body.extend_from_slice(&pkt_line_encode(ref1.as_bytes()));
    let ref2 = format!("{} {}\n", "a".repeat(40), "refs/tags/v1");
    body.extend_from_slice(&pkt_line_encode(ref2.as_bytes()));
    body.extend_from_slice(pkt_line_flush());

    let parsed = parse_ref_advertisement(&body).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].name, "refs/heads/main");
    assert_eq!(parsed[1].name, "refs/tags/v1");
}

// ---------------------------------------------------------------------
// Object reachability walk
// ---------------------------------------------------------------------

fn write_commit(repo: &Repository, tree: ObjectId, parents: Vec<ObjectId>, msg: &[u8]) -> ObjectId {
    let sig = Signature {
        raw: b"t <t@e.com> 1700000000 +0000".to_vec(),
        name: Some(b"t".to_vec()),
        email: Some(b"t@e.com".to_vec()),
        timestamp: Some(1_700_000_000),
        timezone: Some(b"+0000".to_vec()),
    };
    let commit = Commit {
        tree,
        parents,
        author: sig.clone(),
        committer: sig,
        extra_headers: vec![],
        message: msg.to_vec(),
    };
    repo.write_object(&Object::Commit(commit)).unwrap()
}

// @spec TX-OBJWALK-001
#[test]
fn collect_reachable_enumerates_commit_tree_and_blobs() {
    let (_d, repo) = make_repo();
    let blob1 = repo
        .write_object(&Object::Blob(Blob::new(b"a".to_vec())))
        .unwrap();
    let blob2 = repo
        .write_object(&Object::Blob(Blob::new(b"b".to_vec())))
        .unwrap();
    let tree = Tree {
        entries: vec![
            TreeEntry {
                mode: EntryMode::Regular,
                name: b"a.txt".to_vec(),
                id: blob1,
            },
            TreeEntry {
                mode: EntryMode::Regular,
                name: b"b.txt".to_vec(),
                id: blob2,
            },
        ],
    };
    let tree_id = repo.write_object(&Object::Tree(tree)).unwrap();
    let commit_id = write_commit(&repo, tree_id, vec![], b"root\n");

    let mut out = HashSet::new();
    collect_reachable_objects(&repo, &commit_id, &mut out).unwrap();
    assert!(out.contains(&commit_id));
    assert!(out.contains(&tree_id));
    assert!(out.contains(&blob1));
    assert!(out.contains(&blob2));
}

// @spec TX-OBJWALK-002
#[test]
fn collect_reachable_walks_parent_chain() {
    let (_d, repo) = make_repo();
    let blob = repo
        .write_object(&Object::Blob(Blob::new(b"x".to_vec())))
        .unwrap();
    let tree = Tree {
        entries: vec![TreeEntry {
            mode: EntryMode::Regular,
            name: b"x".to_vec(),
            id: blob,
        }],
    };
    let tree_id = repo.write_object(&Object::Tree(tree)).unwrap();
    let c1 = write_commit(&repo, tree_id, vec![], b"first\n");
    let c2 = write_commit(&repo, tree_id, vec![c1], b"second\n");
    let c3 = write_commit(&repo, tree_id, vec![c2], b"third\n");

    let mut out = HashSet::new();
    collect_reachable_objects(&repo, &c3, &mut out).unwrap();
    assert!(out.contains(&c1));
    assert!(out.contains(&c2));
    assert!(out.contains(&c3));
    assert!(out.contains(&tree_id));
    assert!(out.contains(&blob));
}

// @spec TX-OBJWALK-003
#[test]
fn collect_reachable_skips_gitlinks() {
    let (_d, repo) = make_repo();
    let mut sub_sha = [0u8; 20];
    sub_sha.fill(0xab);
    let gitlink_id = ObjectId::from_bytes(sub_sha);
    let tree = Tree {
        entries: vec![TreeEntry {
            mode: EntryMode::Gitlink,
            name: b"submod".to_vec(),
            id: gitlink_id,
        }],
    };
    let tree_id = repo.write_object(&Object::Tree(tree)).unwrap();
    let commit_id = write_commit(&repo, tree_id, vec![], b"with-gitlink\n");

    let mut out = HashSet::new();
    collect_reachable_objects(&repo, &commit_id, &mut out).unwrap();
    assert!(out.contains(&commit_id));
    assert!(out.contains(&tree_id));
    // Gitlink id is NOT in the set — we don't have that submodule's
    // objects and shouldn't claim to.
    assert!(!out.contains(&gitlink_id));
}
