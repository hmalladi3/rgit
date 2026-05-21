//! Spec-anchored unit tests for the object module.
//!
//! Each test cites the EARS spec ID it exercises via a `@spec` comment.

use super::*;

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------
// ObjectId
// ---------------------------------------------------------------------

// @spec OBJ-ID-001
#[test]
fn object_id_is_opaque_20_byte_newtype() {
    assert_eq!(std::mem::size_of::<ObjectId>(), 20);
    let zero = ObjectId::ZERO;
    assert_eq!(zero.as_bytes().len(), 20);
}

// @spec OBJ-ID-002
#[test]
fn object_id_from_hex_accepts_lowercase() {
    let id = ObjectId::from_hex("abcdef0123456789abcdef0123456789abcdef01").unwrap();
    assert_eq!(id.to_hex(), "abcdef0123456789abcdef0123456789abcdef01");
}

// @spec OBJ-ID-002
#[test]
fn object_id_from_hex_accepts_uppercase() {
    let id = ObjectId::from_hex("ABCDEF0123456789ABCDEF0123456789ABCDEF01").unwrap();
    assert_eq!(id.to_hex(), "abcdef0123456789abcdef0123456789abcdef01");
}

// @spec OBJ-ID-002
#[test]
fn object_id_from_hex_accepts_mixed_case() {
    let id = ObjectId::from_hex("AbCdEf0123456789aBcDeF0123456789AbCdEf01").unwrap();
    assert_eq!(id.to_hex(), "abcdef0123456789abcdef0123456789abcdef01");
}

// @spec OBJ-ID-003
#[test]
fn object_id_from_hex_rejects_wrong_length() {
    assert_eq!(ObjectId::from_hex("abc"), Err(ParseError::InvalidHex));
    assert_eq!(
        ObjectId::from_hex("abcdef0123456789abcdef0123456789abcdef0"),
        Err(ParseError::InvalidHex),
    );
    assert_eq!(
        ObjectId::from_hex("abcdef0123456789abcdef0123456789abcdef012"),
        Err(ParseError::InvalidHex),
    );
}

// @spec OBJ-ID-004
#[test]
fn object_id_from_hex_rejects_non_hex_chars() {
    assert_eq!(
        ObjectId::from_hex("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"),
        Err(ParseError::InvalidHex),
    );
    assert_eq!(
        ObjectId::from_hex("abcdef0123456789abcdef0123456789abcdef0g"),
        Err(ParseError::InvalidHex),
    );
}

// @spec OBJ-ID-005
#[test]
fn object_id_to_hex_emits_lowercase() {
    let id = ObjectId::from_hex("ABCDEF0123456789ABCDEF0123456789ABCDEF01").unwrap();
    let hex = id.to_hex();
    assert!(hex.chars().all(|c| !c.is_ascii_uppercase()));
    assert_eq!(hex.len(), 40);
}

// @spec OBJ-ID-006
#[test]
fn object_id_is_zero_for_all_zero() {
    assert!(ObjectId::ZERO.is_zero());
    let from_hex = ObjectId::from_hex("0000000000000000000000000000000000000000").unwrap();
    assert!(from_hex.is_zero());
}

// @spec OBJ-ID-006
#[test]
fn object_id_is_not_zero_for_nonzero() {
    let id = ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap();
    assert!(!id.is_zero());
}

// @spec OBJ-ID-007
#[test]
fn object_id_compute_matches_git_hash_object_empty_blob() {
    // `printf "" | git hash-object --stdin`
    let id = ObjectId::compute(ObjectKind::Blob, b"");
    assert_eq!(id.to_hex(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
}

// @spec OBJ-ID-007
#[test]
fn object_id_compute_matches_git_hash_object_hello_blob() {
    // `printf "hello\n" | git hash-object --stdin`
    let id = ObjectId::compute(ObjectKind::Blob, b"hello\n");
    assert_eq!(id.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
}

// @spec OBJ-ID-008
#[test]
fn object_id_equality_is_byte_equality() {
    let a = ObjectId::from_hex("abcdef0123456789abcdef0123456789abcdef01").unwrap();
    let b = ObjectId::from_hex("abcdef0123456789abcdef0123456789abcdef01").unwrap();
    let c = ObjectId::from_hex("abcdef0123456789abcdef0123456789abcdef02").unwrap();
    assert_eq!(a, b);
    assert_ne!(a, c);
}

// ---------------------------------------------------------------------
// Loose object framing
// ---------------------------------------------------------------------

// @spec OBJ-FRAME-001
#[test]
fn loose_frame_round_trip_empty_blob() {
    let bytes = b"blob 0\0";
    let obj = Object::parse_loose(bytes).unwrap();
    assert_eq!(obj.kind(), ObjectKind::Blob);
    assert_eq!(obj.serialize(), bytes);
}

// @spec OBJ-FRAME-001
#[test]
fn loose_frame_round_trip_short_blob() {
    let bytes = b"blob 5\0hello";
    let obj = Object::parse_loose(bytes).unwrap();
    assert_eq!(obj.kind(), ObjectKind::Blob);
    assert_eq!(obj.serialize(), bytes);
}

// @spec OBJ-FRAME-002
#[test]
fn loose_frame_rejects_uppercase_kind() {
    let err = Object::parse_loose(b"BLOB 0\0").unwrap_err();
    assert!(matches!(err, ParseError::UnknownKind(_)));
}

// @spec OBJ-FRAME-002
#[test]
fn loose_frame_rejects_unknown_kind() {
    let err = Object::parse_loose(b"snap 2\0hi").unwrap_err();
    assert!(matches!(err, ParseError::UnknownKind(_)));
}

// @spec OBJ-FRAME-003
#[test]
fn loose_frame_serialize_size_has_no_leading_zeros() {
    let blob = Object::Blob(Blob::new(b"".to_vec()));
    let bytes = blob.serialize();
    assert!(bytes.starts_with(b"blob 0\0"));
    assert_eq!(bytes, b"blob 0\0");
}

// @spec OBJ-FRAME-003
#[test]
fn loose_frame_parse_rejects_leading_zero_in_size() {
    assert_eq!(
        Object::parse_loose(b"blob 00\0").unwrap_err(),
        ParseError::InvalidFrame,
    );
    assert_eq!(
        Object::parse_loose(b"blob 05\0hello").unwrap_err(),
        ParseError::InvalidFrame,
    );
}

// @spec OBJ-FRAME-004
#[test]
fn loose_frame_rejects_size_mismatch() {
    assert_eq!(
        Object::parse_loose(b"blob 5\0hi").unwrap_err(),
        ParseError::InvalidSize,
    );
    assert_eq!(
        Object::parse_loose(b"blob 2\0hello").unwrap_err(),
        ParseError::InvalidSize,
    );
}

// @spec OBJ-FRAME-005
#[test]
fn loose_frame_rejects_missing_space() {
    assert_eq!(
        Object::parse_loose(b"blob5\0hello").unwrap_err(),
        ParseError::InvalidFrame,
    );
}

// @spec OBJ-FRAME-005
#[test]
fn loose_frame_rejects_missing_nul() {
    assert_eq!(
        Object::parse_loose(b"blob 5 hello").unwrap_err(),
        ParseError::InvalidFrame,
    );
}

// @spec OBJ-FRAME-006
#[test]
fn frame_hash_includes_header_not_payload_alone() {
    // SHA-1("blob 0\0") == e69de29b... (the canonical empty-blob id).
    // SHA-1("") == da39a3ee... — verify compute uses the framed bytes.
    let id = ObjectId::compute(ObjectKind::Blob, b"");
    assert_eq!(id.to_hex(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    assert_ne!(id.to_hex(), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
}

// ---------------------------------------------------------------------
// Blob
// ---------------------------------------------------------------------

// @spec OBJ-BLOB-001
#[test]
fn blob_is_opaque_byte_sequence() {
    let blob = Blob::new(vec![0u8, 1, 2, 0xff]);
    assert_eq!(blob.data, vec![0, 1, 2, 0xff]);
}

// @spec OBJ-BLOB-002
#[test]
fn empty_blob_has_known_id() {
    let id = Object::Blob(Blob::default()).id();
    assert_eq!(id.to_hex(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
}

// ---------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------

// @spec OBJ-TREE-001
#[test]
fn tree_entry_wire_format() {
    let tree = Tree {
        entries: vec![TreeEntry {
            mode: EntryMode::Regular,
            name: b"hello.txt".to_vec(),
            id: ObjectId::from_hex("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap(),
        }],
    };
    let frame = Object::Tree(tree).serialize();
    let nul_idx = frame.iter().position(|&b| b == 0).unwrap();
    let payload = &frame[nul_idx + 1..];
    assert!(payload.starts_with(b"100644 hello.txt\0"));
    assert_eq!(payload.len(), b"100644 hello.txt\0".len() + 20);
}

// @spec OBJ-TREE-002
#[test]
fn tree_recognizes_all_known_modes() {
    for mode in [
        EntryMode::Regular,
        EntryMode::Executable,
        EntryMode::Tree,
        EntryMode::Symlink,
        EntryMode::Gitlink,
    ] {
        let tree = Tree {
            entries: vec![TreeEntry {
                mode,
                name: b"x".to_vec(),
                id: ObjectId::ZERO,
            }],
        };
        let _ = Object::Tree(tree).serialize();
    }
}

// @spec OBJ-TREE-003
#[test]
fn tree_parses_leading_zero_tree_mode_as_tree() {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"040000 subdir\0");
    payload.extend_from_slice(&[0u8; 20]);
    let obj = Object::parse_payload(ObjectKind::Tree, &payload).unwrap();
    let Object::Tree(tree) = obj else {
        panic!("expected tree")
    };
    assert_eq!(tree.entries.len(), 1);
    assert_eq!(tree.entries[0].mode, EntryMode::Tree);
}

// @spec OBJ-TREE-003
#[test]
fn tree_normalizes_leading_zero_tree_mode_on_serialize() {
    let tree = Tree {
        entries: vec![TreeEntry {
            mode: EntryMode::Tree,
            name: b"subdir".to_vec(),
            id: ObjectId::ZERO,
        }],
    };
    let frame = Object::Tree(tree).serialize();
    let nul_idx = frame.iter().position(|&b| b == 0).unwrap();
    let payload = &frame[nul_idx + 1..];
    assert!(payload.starts_with(b"40000 subdir\0"));
}

// @spec OBJ-TREE-004
#[test]
fn tree_rejects_unknown_mode() {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"999999 x\0");
    payload.extend_from_slice(&[0u8; 20]);
    let err = Object::parse_payload(ObjectKind::Tree, &payload).unwrap_err();
    assert!(matches!(err, ParseError::InvalidMode(_)));
}

// @spec OBJ-TREE-005
#[test]
fn tree_entry_name_preserves_non_utf8_bytes() {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"100644 \xff\xfe\xfd\0");
    payload.extend_from_slice(&[0u8; 20]);
    let obj = Object::parse_payload(ObjectKind::Tree, &payload).unwrap();
    let Object::Tree(tree) = obj else {
        panic!("expected tree")
    };
    assert_eq!(tree.entries[0].name, vec![0xff, 0xfe, 0xfd]);
}

// @spec OBJ-TREE-006
#[test]
fn tree_rejects_empty_entry_name() {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"100644 \0");
    payload.extend_from_slice(&[0u8; 20]);
    let err = Object::parse_payload(ObjectKind::Tree, &payload).unwrap_err();
    assert_eq!(err, ParseError::InvalidTreeEntry);
}

// @spec OBJ-TREE-007
#[test]
fn tree_rejects_slash_in_entry_name() {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"100644 a/b\0");
    payload.extend_from_slice(&[0u8; 20]);
    let err = Object::parse_payload(ObjectKind::Tree, &payload).unwrap_err();
    assert_eq!(err, ParseError::InvalidTreeEntry);
}

// @spec OBJ-TREE-008
#[test]
fn tree_accepts_wrongly_sorted_entries_on_parse() {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"100644 z\0");
    payload.extend_from_slice(&[0u8; 20]);
    payload.extend_from_slice(b"100644 a\0");
    payload.extend_from_slice(&[0u8; 20]);
    let obj = Object::parse_payload(ObjectKind::Tree, &payload).unwrap();
    let Object::Tree(tree) = obj else {
        panic!("expected tree")
    };
    assert_eq!(tree.entries[0].name, b"z");
    assert_eq!(tree.entries[1].name, b"a");
}

// @spec OBJ-TREE-009
#[test]
fn tree_accepts_duplicate_entry_names_on_parse() {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"100644 foo\0");
    payload.extend_from_slice(&[0u8; 20]);
    payload.extend_from_slice(b"100644 foo\0");
    payload.extend_from_slice(&[1u8; 20]);
    let obj = Object::parse_payload(ObjectKind::Tree, &payload).unwrap();
    let Object::Tree(tree) = obj else {
        panic!("expected tree")
    };
    assert_eq!(tree.entries.len(), 2);
}

// @spec OBJ-TREE-010
#[test]
fn tree_serializes_entries_in_mode_aware_sort_order() {
    let tree = Tree {
        entries: vec![
            TreeEntry {
                mode: EntryMode::Regular,
                name: b"foo.txt".to_vec(),
                id: ObjectId::ZERO,
            },
            TreeEntry {
                mode: EntryMode::Tree,
                name: b"foo".to_vec(),
                id: ObjectId::ZERO,
            },
        ],
    };
    let frame = Object::Tree(tree).serialize();
    let nul_idx = frame.iter().position(|&b| b == 0).unwrap();
    let payload = &frame[nul_idx + 1..];
    let blob_pos = find_subsequence(payload, b"100644 foo.txt\0").expect("blob entry present");
    let tree_pos = find_subsequence(payload, b"40000 foo\0").expect("tree entry present");
    // "foo.txt" < "foo/" lexicographically (b'.' < b'/').
    assert!(blob_pos < tree_pos);
}

// @spec OBJ-TREE-011
#[test]
fn tree_dedupe_first_wins_on_serialize_byte_equal_name() {
    let id_a = ObjectId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    let id_b = ObjectId::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
    let tree = Tree {
        entries: vec![
            TreeEntry {
                mode: EntryMode::Regular,
                name: b"foo".to_vec(),
                id: id_a,
            },
            TreeEntry {
                mode: EntryMode::Tree,
                name: b"foo".to_vec(),
                id: id_b,
            },
        ],
    };
    let frame = Object::Tree(tree).serialize();
    let nul_idx = frame.iter().position(|&b| b == 0).unwrap();
    let payload = &frame[nul_idx + 1..];
    assert!(payload.starts_with(b"100644 foo\0"));
    assert_eq!(&payload[b"100644 foo\0".len()..], id_a.as_bytes());
    assert_eq!(payload.len(), b"100644 foo\0".len() + 20);
}

// @spec OBJ-TREE-012
#[test]
fn empty_tree_has_known_id() {
    let id = Object::Tree(Tree::default()).id();
    assert_eq!(id.to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
}

// @spec OBJ-TREE-012
#[test]
fn empty_tree_serializes_to_zero_payload_frame() {
    let frame = Object::Tree(Tree::default()).serialize();
    assert_eq!(frame, b"tree 0\0");
}

// ---------------------------------------------------------------------
// Commit
// ---------------------------------------------------------------------

const COMMIT_FIXTURE: &[u8] = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
parent 0000000000000000000000000000000000000001\n\
author Author Name <author@example.com> 1700000000 +0000\n\
committer Committer Name <committer@example.com> 1700000001 +0000\n\
\n\
Initial commit\n";

// @spec OBJ-COMMIT-001
#[test]
fn commit_parses_required_headers_and_message() {
    let obj = Object::parse_payload(ObjectKind::Commit, COMMIT_FIXTURE).unwrap();
    let Object::Commit(c) = obj else {
        panic!("expected commit")
    };
    assert_eq!(c.tree.to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    assert_eq!(c.parents.len(), 1);
    assert_eq!(
        c.parents[0].to_hex(),
        "0000000000000000000000000000000000000001"
    );
    assert_eq!(c.message, b"Initial commit\n");
}

// @spec OBJ-COMMIT-002
#[test]
fn commit_rejects_missing_tree_header() {
    let payload = b"\
author A <a@e.com> 1700000000 +0000\n\
committer C <c@e.com> 1700000000 +0000\n\
\n\
msg\n";
    let err = Object::parse_payload(ObjectKind::Commit, payload).unwrap_err();
    assert!(matches!(err, ParseError::MissingHeader(_)));
}

// @spec OBJ-COMMIT-002
#[test]
fn commit_rejects_missing_author_header() {
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
committer C <c@e.com> 1700000000 +0000\n\
\n\
msg\n";
    let err = Object::parse_payload(ObjectKind::Commit, payload).unwrap_err();
    assert!(matches!(err, ParseError::MissingHeader(_)));
}

// @spec OBJ-COMMIT-002
#[test]
fn commit_rejects_missing_committer_header() {
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@e.com> 1700000000 +0000\n\
\n\
msg\n";
    let err = Object::parse_payload(ObjectKind::Commit, payload).unwrap_err();
    assert!(matches!(err, ParseError::MissingHeader(_)));
}

// @spec OBJ-COMMIT-003
#[test]
fn commit_rejects_missing_blank_line() {
    // Pending Phase 5 empirical verification (see object LLD § Pending
    // empirical verification). If upstream git accepts this as a
    // commit-with-empty-message, OBJ-COMMIT-003 loosens and this test
    // inverts.
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@e.com> 1700000000 +0000\n\
committer C <c@e.com> 1700000000 +0000\n";
    let err = Object::parse_payload(ObjectKind::Commit, payload).unwrap_err();
    assert!(matches!(err, ParseError::MissingHeader(_)));
}

// @spec OBJ-COMMIT-004
#[test]
fn commit_with_zero_parents_is_valid_root() {
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@e.com> 1700000000 +0000\n\
committer C <c@e.com> 1700000000 +0000\n\
\n\
Root commit\n";
    let obj = Object::parse_payload(ObjectKind::Commit, payload).unwrap();
    let Object::Commit(c) = obj else {
        panic!("expected commit")
    };
    assert!(c.parents.is_empty());
}

// @spec OBJ-COMMIT-005, OBJ-COMMIT-006
#[test]
fn commit_parses_many_parents_in_order() {
    let mut payload: Vec<u8> = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n".to_vec();
    for i in 1u8..=10 {
        payload.extend_from_slice(b"parent ");
        payload.extend_from_slice(format!("{i:040x}").as_bytes());
        payload.push(b'\n');
    }
    payload.extend_from_slice(b"author A <a@e.com> 1700000000 +0000\n");
    payload.extend_from_slice(b"committer C <c@e.com> 1700000000 +0000\n\n");
    payload.extend_from_slice(b"Octopus merge\n");
    let obj = Object::parse_payload(ObjectKind::Commit, &payload).unwrap();
    let Object::Commit(c) = obj else {
        panic!("expected commit")
    };
    assert_eq!(c.parents.len(), 10);
    assert!(c.parents[0].to_hex().ends_with('1'));
    assert!(c.parents[9].to_hex().ends_with('a'));
}

// @spec OBJ-COMMIT-007
#[test]
fn commit_duplicate_tree_header_first_wins_rest_in_extra() {
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
tree e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n\
author A <a@e.com> 1700000000 +0000\n\
committer C <c@e.com> 1700000000 +0000\n\
\n\
msg\n";
    let obj = Object::parse_payload(ObjectKind::Commit, payload).unwrap();
    let Object::Commit(c) = obj else {
        panic!("expected commit")
    };
    assert_eq!(c.tree.to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    assert!(c
        .extra_headers
        .iter()
        .any(|(k, v)| { k == b"tree" && v == b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391" }));
}

// @spec OBJ-COMMIT-008
#[test]
fn commit_preserves_unknown_headers_verbatim_with_order() {
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@e.com> 1700000000 +0000\n\
committer C <c@e.com> 1700000000 +0000\n\
encoding UTF-8\n\
HG:rename foo bar\n\
\n\
msg\n";
    let obj = Object::parse_payload(ObjectKind::Commit, payload).unwrap();
    let Object::Commit(c) = obj else {
        panic!("expected commit")
    };
    assert_eq!(c.extra_headers.len(), 2);
    assert_eq!(c.extra_headers[0].0, b"encoding");
    assert_eq!(c.extra_headers[0].1, b"UTF-8");
    assert_eq!(c.extra_headers[1].0, b"HG:rename");
    assert_eq!(c.extra_headers[1].1, b"foo bar");
}

// @spec OBJ-COMMIT-008
#[test]
fn commit_preserves_repeated_unknown_header_occurrences_separately() {
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@e.com> 1700000000 +0000\n\
committer C <c@e.com> 1700000000 +0000\n\
custom-tag first\n\
custom-tag second\n\
\n\
msg\n";
    let obj = Object::parse_payload(ObjectKind::Commit, payload).unwrap();
    let Object::Commit(c) = obj else {
        panic!("expected commit")
    };
    let custom: Vec<_> = c
        .extra_headers
        .iter()
        .filter(|(k, _)| k == b"custom-tag")
        .collect();
    assert_eq!(custom.len(), 2);
    assert_eq!(custom[0].1, b"first");
    assert_eq!(custom[1].1, b"second");
}

// @spec OBJ-COMMIT-009
#[test]
fn commit_preserves_multiline_header_value_verbatim() {
    // Build the fixture with explicit byte concatenation: Rust's `\`
    // line-continuation inside `b"..."` literals would eat the leading
    // space on each continuation line, which is exactly the byte that
    // signals continuation in the wire format.
    let mut payload: Vec<u8> = Vec::new();
    payload.extend_from_slice(b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n");
    payload.extend_from_slice(b"author A <a@e.com> 1700000000 +0000\n");
    payload.extend_from_slice(b"committer C <c@e.com> 1700000000 +0000\n");
    payload.extend_from_slice(b"gpgsig -----BEGIN PGP SIGNATURE-----\n");
    payload.extend_from_slice(b" \n");
    payload.extend_from_slice(b" iQEzBAABCAAdFiEE\n");
    payload.extend_from_slice(b" -----END PGP SIGNATURE-----\n");
    payload.extend_from_slice(b"\n");
    payload.extend_from_slice(b"msg\n");

    let obj = Object::parse_payload(ObjectKind::Commit, &payload).unwrap();
    let Object::Commit(c) = obj else {
        panic!("expected commit")
    };
    let (_, value) = c
        .extra_headers
        .iter()
        .find(|(k, _)| k == b"gpgsig")
        .expect("gpgsig header present");
    // Stored bytes include the continuation markers — the LFs and the
    // single leading space on each continuation line.
    assert!(value.contains(&b'\n'));
    assert!(value.windows(2).any(|w| w == b"\n "));
}

// @spec OBJ-COMMIT-010
#[test]
fn commit_message_preserves_bytes_byte_for_byte() {
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@e.com> 1700000000 +0000\n\
committer C <c@e.com> 1700000000 +0000\n\
\n\
Subject\r\n\r\nWith CRLF and no trailing LF";
    let obj = Object::parse_payload(ObjectKind::Commit, payload).unwrap();
    let Object::Commit(c) = obj else {
        panic!("expected commit")
    };
    assert_eq!(c.message, b"Subject\r\n\r\nWith CRLF and no trailing LF");
}

// @spec OBJ-COMMIT-011
#[test]
fn commit_empty_message_is_valid() {
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@e.com> 1700000000 +0000\n\
committer C <c@e.com> 1700000000 +0000\n\
\n";
    let obj = Object::parse_payload(ObjectKind::Commit, payload).unwrap();
    let Object::Commit(c) = obj else {
        panic!("expected commit")
    };
    assert!(c.message.is_empty());
}

// @spec OBJ-COMMIT-012
#[test]
fn commit_signature_parses_standard_form() {
    let obj = Object::parse_payload(ObjectKind::Commit, COMMIT_FIXTURE).unwrap();
    let Object::Commit(c) = obj else {
        panic!("expected commit")
    };
    assert_eq!(c.author.name.as_deref(), Some(b"Author Name".as_slice()));
    assert_eq!(
        c.author.email.as_deref(),
        Some(b"author@example.com".as_slice()),
    );
    assert_eq!(c.author.timestamp, Some(1_700_000_000));
    assert_eq!(c.author.timezone.as_deref(), Some(b"+0000".as_slice()));
    assert_eq!(
        c.author.raw,
        b"Author Name <author@example.com> 1700000000 +0000",
    );
}

// @spec OBJ-COMMIT-013
#[test]
fn commit_signature_without_email_brackets_sets_email_none() {
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author John Doe 1700000000 +0000\n\
committer C <c@e.com> 1700000000 +0000\n\
\n\
msg\n";
    let obj = Object::parse_payload(ObjectKind::Commit, payload).unwrap();
    let Object::Commit(c) = obj else {
        panic!("expected commit")
    };
    assert!(c.author.email.is_none());
    assert_eq!(c.author.name.as_deref(), Some(b"John Doe".as_slice()));
}

// @spec OBJ-COMMIT-014
#[test]
fn commit_signature_missing_timestamp_is_none() {
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@e.com>\n\
committer C <c@e.com> 1700000000 +0000\n\
\n\
msg\n";
    let obj = Object::parse_payload(ObjectKind::Commit, payload).unwrap();
    let Object::Commit(c) = obj else {
        panic!("expected commit")
    };
    assert!(c.author.timestamp.is_none());
    assert!(c.author.timezone.is_none());
}

// @spec OBJ-COMMIT-015
#[test]
fn commit_signature_byte_identical_on_reserialize() {
    let obj = Object::parse_payload(ObjectKind::Commit, COMMIT_FIXTURE).unwrap();
    let serialized = obj.serialize();
    let nul_idx = serialized.iter().position(|&b| b == 0).unwrap();
    let payload = &serialized[nul_idx + 1..];
    assert_eq!(payload, COMMIT_FIXTURE);
}

// @spec OBJ-COMMIT-016
#[test]
fn commit_signature_with_no_whitespace_is_invalid() {
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author solidtoken\n\
committer C <c@e.com> 1700000000 +0000\n\
\n\
msg\n";
    let err = Object::parse_payload(ObjectKind::Commit, payload).unwrap_err();
    assert_eq!(err, ParseError::InvalidSignature);
}

// ---------------------------------------------------------------------
// Tag
// ---------------------------------------------------------------------

const TAG_FIXTURE: &[u8] = b"\
object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
type commit\n\
tag v1.0.0\n\
tagger Tagger Name <tagger@example.com> 1700000000 +0000\n\
\n\
Release v1.0.0\n";

// @spec OBJ-TAG-001
#[test]
fn tag_parses_canonical_headers_and_message() {
    let obj = Object::parse_payload(ObjectKind::Tag, TAG_FIXTURE).unwrap();
    let Object::Tag(t) = obj else {
        panic!("expected tag")
    };
    assert_eq!(
        t.object.to_hex(),
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
    );
    assert_eq!(t.object_kind, ObjectKind::Commit);
    assert_eq!(t.name, b"v1.0.0");
    assert_eq!(t.message, b"Release v1.0.0\n");
}

// @spec OBJ-TAG-002
#[test]
fn tag_rejects_missing_object_header() {
    let payload = b"\
type commit\n\
tag v1.0.0\n\
tagger T <t@e.com> 1700000000 +0000\n\
\n\
msg\n";
    let err = Object::parse_payload(ObjectKind::Tag, payload).unwrap_err();
    assert!(matches!(err, ParseError::MissingHeader(_)));
}

// @spec OBJ-TAG-002
#[test]
fn tag_rejects_missing_tagger_header() {
    let payload = b"\
object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
type commit\n\
tag v1.0.0\n\
\n\
msg\n";
    let err = Object::parse_payload(ObjectKind::Tag, payload).unwrap_err();
    assert!(matches!(err, ParseError::MissingHeader(_)));
}

// @spec OBJ-TAG-003
#[test]
fn tag_rejects_unknown_type() {
    let payload = b"\
object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
type snap\n\
tag v1.0.0\n\
tagger T <t@e.com> 1700000000 +0000\n\
\n\
msg\n";
    let err = Object::parse_payload(ObjectKind::Tag, payload).unwrap_err();
    assert!(matches!(err, ParseError::UnknownKind(_)));
}

// @spec OBJ-TAG-003
#[test]
fn tag_rejects_uppercase_type() {
    let payload = b"\
object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
type Commit\n\
tag v1.0.0\n\
tagger T <t@e.com> 1700000000 +0000\n\
\n\
msg\n";
    let err = Object::parse_payload(ObjectKind::Tag, payload).unwrap_err();
    assert!(matches!(err, ParseError::UnknownKind(_)));
}

// @spec OBJ-TAG-004
#[test]
fn tag_name_preserved_as_bytes() {
    let payload = b"\
object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
type commit\n\
tag \xff\xfe\n\
tagger T <t@e.com> 1700000000 +0000\n\
\n\
msg\n";
    let obj = Object::parse_payload(ObjectKind::Tag, payload).unwrap();
    let Object::Tag(t) = obj else {
        panic!("expected tag")
    };
    assert_eq!(t.name, vec![0xff, 0xfe]);
}

// @spec OBJ-TAG-005
#[test]
fn tag_tagger_uses_signature_format() {
    let obj = Object::parse_payload(ObjectKind::Tag, TAG_FIXTURE).unwrap();
    let Object::Tag(t) = obj else {
        panic!("expected tag")
    };
    assert_eq!(t.tagger.name.as_deref(), Some(b"Tagger Name".as_slice()));
    assert_eq!(
        t.tagger.email.as_deref(),
        Some(b"tagger@example.com".as_slice()),
    );
    assert_eq!(t.tagger.timestamp, Some(1_700_000_000));
}

// @spec OBJ-TAG-006
#[test]
fn tag_inline_pgp_signature_stored_in_message() {
    let payload = b"\
object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
type commit\n\
tag v1.0.0\n\
tagger T <t@e.com> 1700000000 +0000\n\
\n\
Release notes\n-----BEGIN PGP SIGNATURE-----\n...\n-----END PGP SIGNATURE-----\n";
    let obj = Object::parse_payload(ObjectKind::Tag, payload).unwrap();
    let Object::Tag(t) = obj else {
        panic!("expected tag")
    };
    assert!(find_subsequence(&t.message, b"-----BEGIN PGP SIGNATURE-----").is_some());
}

// ---------------------------------------------------------------------
// Cross-cutting parse behavior
// ---------------------------------------------------------------------

// @spec OBJ-PARSE-001
#[test]
fn parse_does_not_verify_hash() {
    // Parsing succeeds for any structurally-valid bytes; hash
    // verification belongs to the storage layer.
    let bytes = b"blob 5\0hello";
    let obj = Object::parse_loose(bytes).unwrap();
    let _id = obj.id();
}

// @spec OBJ-PARSE-002
#[test]
fn parse_errors_are_typed_variants() {
    assert!(matches!(
        Object::parse_loose(b"blob5\0hi"),
        Err(ParseError::InvalidFrame),
    ));
    assert!(matches!(
        Object::parse_loose(b"blob 99\0hi"),
        Err(ParseError::InvalidSize),
    ));
    assert!(matches!(
        Object::parse_loose(b"snap 2\0hi"),
        Err(ParseError::UnknownKind(_)),
    ));
}

// @spec OBJ-PARSE-003
#[test]
fn parse_loose_takes_byte_slice_and_returns_owned() {
    let obj = {
        let bytes = b"blob 5\0hello".to_vec();
        Object::parse_loose(&bytes).unwrap()
        // bytes dropped here; obj must own its data.
    };
    let _ = obj.serialize();
}

// @spec OBJ-PARSE-004
#[test]
fn parse_returns_truncated_for_incomplete_tree_entry() {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"100644 foo\0");
    payload.extend_from_slice(&[0u8; 10]); // only 10 SHA bytes; need 20.
    let err = Object::parse_payload(ObjectKind::Tree, &payload).unwrap_err();
    assert_eq!(err, ParseError::Truncated);
}

// ---------------------------------------------------------------------
// Cross-cutting serialize behavior
// ---------------------------------------------------------------------

// @spec OBJ-SERIALIZE-001
#[test]
fn serialize_produces_loose_framed_bytes() {
    let blob = Object::Blob(Blob::new(b"hi".to_vec()));
    let bytes = blob.serialize();
    assert!(bytes.starts_with(b"blob "));
    assert!(bytes.contains(&0));
}

// @spec OBJ-SERIALIZE-002
#[test]
fn serialize_emits_canonical_commit_header_order() {
    let payload = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@e.com> 1700000000 +0000\n\
committer C <c@e.com> 1700000000 +0000\n\
encoding UTF-8\n\
\n\
msg\n";
    let obj = Object::parse_payload(ObjectKind::Commit, payload).unwrap();
    let serialized = obj.serialize();
    let nul_idx = serialized.iter().position(|&b| b == 0).unwrap();
    let body = &serialized[nul_idx + 1..];
    let tree_pos = find_subsequence(body, b"tree ").unwrap();
    let author_pos = find_subsequence(body, b"author ").unwrap();
    let committer_pos = find_subsequence(body, b"committer ").unwrap();
    let encoding_pos = find_subsequence(body, b"encoding ").unwrap();
    assert!(tree_pos < author_pos);
    assert!(author_pos < committer_pos);
    assert!(committer_pos < encoding_pos);
}

// @spec OBJ-SERIALIZE-003
#[test]
fn canonical_round_trip_blob() {
    let bytes = b"blob 5\0hello";
    let obj = Object::parse_loose(bytes).unwrap();
    assert_eq!(obj.serialize(), bytes);
    let reparsed = Object::parse_loose(&obj.serialize()).unwrap();
    assert_eq!(reparsed, obj);
}

// @spec OBJ-SERIALIZE-003
#[test]
fn canonical_round_trip_commit() {
    let mut frame = b"commit ".to_vec();
    frame.extend_from_slice(COMMIT_FIXTURE.len().to_string().as_bytes());
    frame.push(0);
    frame.extend_from_slice(COMMIT_FIXTURE);
    let obj = Object::parse_loose(&frame).unwrap();
    assert_eq!(obj.serialize(), frame);
}

// @spec OBJ-SERIALIZE-003
#[test]
fn canonical_round_trip_tag() {
    let mut frame = b"tag ".to_vec();
    frame.extend_from_slice(TAG_FIXTURE.len().to_string().as_bytes());
    frame.push(0);
    frame.extend_from_slice(TAG_FIXTURE);
    let obj = Object::parse_loose(&frame).unwrap();
    assert_eq!(obj.serialize(), frame);
}
