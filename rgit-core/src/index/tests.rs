//! Spec-anchored tests for the index module.

use super::*;
use tempfile::TempDir;

fn make_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path(), false).unwrap();
    (dir, repo)
}

fn entry(path: &[u8], byte: u8) -> IndexEntry {
    let mut sha = [0u8; 20];
    sha.fill(byte);
    IndexEntry {
        ctime: Time::default(),
        mtime: Time::default(),
        dev: 0,
        ino: 0,
        mode: 0o100644,
        uid: 0,
        gid: 0,
        size: 0,
        id: ObjectId::from_bytes(sha),
        assume_valid: false,
        stage: 0,
        path: path.to_vec(),
    }
}

// ---------------------------------------------------------------------
// FMT
// ---------------------------------------------------------------------

// @spec INDEX-FMT-001, INDEX-FMT-003
#[test]
fn writer_emits_dirc_header_and_trailer() {
    let (_d, repo) = make_repo();
    let index = Index::new();
    repo.write_index(&index).unwrap();
    let bytes = std::fs::read(repo.git_dir().join("index")).unwrap();
    assert_eq!(&bytes[0..4], b"DIRC");
    assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), 2);
    assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 0);
    // Trailer is SHA-1 of body.
    let body_len = bytes.len() - 20;
    assert_eq!(&bytes[body_len..], &sha1_bytes(&bytes[..body_len])[..]);
}

// @spec INDEX-FMT-002
#[test]
fn read_rejects_wrong_version() {
    let (_d, repo) = make_repo();
    // Write a v3 index header by hand.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"DIRC");
    bytes.extend_from_slice(&3u32.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    let trailer = sha1_bytes(&bytes);
    bytes.extend_from_slice(&trailer);
    std::fs::write(repo.git_dir().join("index"), &bytes).unwrap();
    assert!(matches!(repo.read_index(), Err(IndexError::BadHeader)));
}

// @spec INDEX-FMT-004
#[test]
fn read_rejects_bad_trailer() {
    let (_d, repo) = make_repo();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"DIRC");
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 20]); // deliberately wrong trailer
    std::fs::write(repo.git_dir().join("index"), &bytes).unwrap();
    assert!(matches!(repo.read_index(), Err(IndexError::BadTrailer)));
}

// ---------------------------------------------------------------------
// READ / WRITE round-trip
// ---------------------------------------------------------------------

// @spec INDEX-READ-001
#[test]
fn read_returns_empty_index_when_file_absent() {
    let (_d, repo) = make_repo();
    let index = repo.read_index().unwrap();
    assert_eq!(index.entries().len(), 0);
}

// @spec INDEX-READ-003, INDEX-WRITE-001, INDEX-WRITE-002, INDEX-WRITE-003
#[test]
fn single_entry_round_trips() {
    let (_d, repo) = make_repo();
    let mut index = Index::new();
    index.upsert(entry(b"file.txt", 0x42));
    repo.write_index(&index).unwrap();
    let read = repo.read_index().unwrap();
    assert_eq!(read.entries().len(), 1);
    assert_eq!(read.entries()[0].path, b"file.txt");
    assert_eq!(read.entries()[0].id, entry(b"file.txt", 0x42).id);
}

// @spec INDEX-ENTRY-001, INDEX-ENTRY-002, INDEX-ENTRY-005
#[test]
fn multiple_entries_round_trip_preserving_paths_and_ids() {
    let (_d, repo) = make_repo();
    let mut index = Index::new();
    index.upsert(entry(b"a.txt", 0x01));
    index.upsert(entry(b"dir/b.txt", 0x02));
    index.upsert(entry(b"non-utf8-\xff\xfe", 0x03));
    repo.write_index(&index).unwrap();
    let read = repo.read_index().unwrap();
    assert_eq!(read.entries().len(), 3);
    assert_eq!(read.entries()[0].path, b"a.txt");
    assert_eq!(read.entries()[1].path, b"dir/b.txt");
    assert_eq!(read.entries()[2].path, b"non-utf8-\xff\xfe");
}

// @spec INDEX-ENTRY-003
#[test]
fn flags_field_encodes_assume_valid_and_stage() {
    let (_d, repo) = make_repo();
    let mut index = Index::new();
    let mut e = entry(b"merged.txt", 0x10);
    e.assume_valid = true;
    e.stage = 2;
    index.upsert(e);
    repo.write_index(&index).unwrap();
    let read = repo.read_index().unwrap();
    let first = &read.entries()[0];
    assert!(first.assume_valid);
    assert_eq!(first.stage, 2);
}

// @spec INDEX-ENTRY-004
#[test]
fn extended_flag_in_v2_is_rejected() {
    let (_d, repo) = make_repo();
    // Hand-build a single-entry index with the extended bit set.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"DIRC");
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(&1u32.to_be_bytes());
    // Entry fixed prefix.
    bytes.extend_from_slice(&[0u8; 40]); // ctime+mtime+dev+ino+mode+uid+gid+size
    bytes.extend_from_slice(&[0u8; 20]); // sha
    let flags: u16 = 0x4000 | 1; // extended bit set, name_length=1
    bytes.extend_from_slice(&flags.to_be_bytes());
    bytes.extend_from_slice(b"x\0"); // 1-byte path + NUL
                                     // Pad to 8-byte boundary.
    while (bytes.len() - 12) % 8 != 0 {
        bytes.push(0);
    }
    let trailer = sha1_bytes(&bytes);
    bytes.extend_from_slice(&trailer);
    std::fs::write(repo.git_dir().join("index"), &bytes).unwrap();
    assert!(matches!(repo.read_index(), Err(IndexError::MalformedEntry)));
}

// ---------------------------------------------------------------------
// ORDER + API
// ---------------------------------------------------------------------

// @spec INDEX-ORDER-001, INDEX-WRITE-002
#[test]
fn entries_emitted_in_path_sort_order_regardless_of_insertion() {
    let (_d, repo) = make_repo();
    let mut index = Index::new();
    index.upsert(entry(b"zeta", 0x01));
    index.upsert(entry(b"alpha", 0x02));
    index.upsert(entry(b"middle", 0x03));
    repo.write_index(&index).unwrap();
    let read = repo.read_index().unwrap();
    let paths: Vec<&[u8]> = read.entries().iter().map(|e| e.path.as_slice()).collect();
    assert_eq!(paths, vec![b"alpha".as_slice(), b"middle", b"zeta"]);
}

// @spec INDEX-ORDER-002
#[test]
fn stage_0_upsert_drops_conflict_stages_for_same_path() {
    let mut index = Index::new();
    let mut e1 = entry(b"conflicted", 0x01);
    e1.stage = 1;
    index.upsert(e1);
    let mut e2 = entry(b"conflicted", 0x02);
    e2.stage = 2;
    index.upsert(e2);
    let resolved = entry(b"conflicted", 0xff);
    index.upsert(resolved);
    assert_eq!(index.entries().len(), 1);
    assert_eq!(index.entries()[0].stage, 0);
}

// @spec INDEX-ORDER-003
#[test]
fn conflict_stage_upsert_drops_stage_0_for_same_path() {
    let mut index = Index::new();
    index.upsert(entry(b"clean", 0x01));
    let mut conflict = entry(b"clean", 0x02);
    conflict.stage = 1;
    index.upsert(conflict);
    assert_eq!(index.entries().len(), 1);
    assert_eq!(index.entries()[0].stage, 1);
}

// @spec INDEX-API-001
#[test]
fn lookup_returns_stage_0_entry_only() {
    let mut index = Index::new();
    index.upsert(entry(b"present", 0x01));
    assert!(index.lookup(b"present").is_some());
    assert!(index.lookup(b"absent").is_none());
}

// @spec INDEX-API-002
#[test]
fn remove_drops_all_stages_for_path() {
    let mut index = Index::new();
    let mut e1 = entry(b"path", 0x01);
    e1.stage = 1;
    index.upsert(e1);
    let mut e2 = entry(b"path", 0x02);
    e2.stage = 2;
    index.upsert(e2);
    assert_eq!(index.remove(b"path"), 2);
    assert_eq!(index.entries().len(), 0);
}

// @spec INDEX-API-004
#[test]
fn new_returns_empty_index() {
    let i = Index::new();
    assert_eq!(i.entries().len(), 0);
    assert_eq!(i.extensions().len(), 0);
}

// ---------------------------------------------------------------------
// EXT
// ---------------------------------------------------------------------

// @spec INDEX-EXT-001, INDEX-EXT-002
#[test]
fn extensions_round_trip_verbatim() {
    let (_d, repo) = make_repo();
    let mut index = Index::new();
    index.upsert(entry(b"file", 0x42));
    index.push_extension("link".to_string(), b"opaque-extension-data".to_vec());
    index.push_extension("TREE".to_string(), b"\x01\x02\x03\x04".to_vec());
    repo.write_index(&index).unwrap();
    let read = repo.read_index().unwrap();
    assert_eq!(read.extensions().len(), 2);
    assert_eq!(read.extensions()[0].0, "link");
    assert_eq!(read.extensions()[0].1, b"opaque-extension-data");
    assert_eq!(read.extensions()[1].0, "TREE");
    assert_eq!(read.extensions()[1].1, vec![0x01, 0x02, 0x03, 0x04]);
}

// ---------------------------------------------------------------------
// Big-name and 8-byte padding alignment
// ---------------------------------------------------------------------

// @spec INDEX-ENTRY-001 (padding correctness)
#[test]
fn paths_of_various_lengths_round_trip() {
    let (_d, repo) = make_repo();
    let mut index = Index::new();
    for len in [1usize, 7, 8, 9, 17, 100, 1000] {
        let path = vec![b'x'; len];
        index.upsert(entry(&path, 0x42));
    }
    repo.write_index(&index).unwrap();
    let read = repo.read_index().unwrap();
    assert_eq!(read.entries().len(), 7);
    for entry in read.entries() {
        // Every path byte is 'x'.
        assert!(entry.path.iter().all(|&b| b == b'x'));
    }
}

// @spec INDEX-ENTRY-006
#[test]
fn long_path_uses_0xfff_in_flags() {
    let (_d, repo) = make_repo();
    let mut index = Index::new();
    let path = vec![b'x'; 5000];
    index.upsert(entry(&path, 0x42));
    repo.write_index(&index).unwrap();
    let read = repo.read_index().unwrap();
    assert_eq!(read.entries()[0].path.len(), 5000);
}
