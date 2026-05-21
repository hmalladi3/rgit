//! Spec-anchored tests for the pack module.

use super::*;
use crate::object::ObjectId;
use std::io::Write as _;
use std::path::PathBuf;
use tempfile::TempDir;

// ---------------------------------------------------------------------
// Helpers — pack/idx byte construction for fixture-driven tests
// ---------------------------------------------------------------------

fn sha1(bytes: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(bytes);
    let mut out = [0u8; 20];
    out.copy_from_slice(&h.finalize());
    out
}

/// Encode a pack entry's type+size header per PACK-ENTRY-001.
fn encode_size_varint(type_code: u8, size: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    let low4 = (size & 0x0f) as u8;
    let mut rest = size >> 4;
    let first_cont = if rest > 0 { 0x80 } else { 0 };
    let first = first_cont | ((type_code & 0x07) << 4) | low4;
    bytes.push(first);
    while rest > 0 {
        let cont = if rest > 0x7f { 0x80 } else { 0 };
        bytes.push(cont | (rest & 0x7f) as u8);
        rest >>= 7;
    }
    bytes
}

fn zlib_compress(bytes: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

/// Build a pack body (no trailer) from full-object entries.
/// Each entry: `(type_code, payload)`. Type codes: 1=commit, 2=tree,
/// 3=blob, 4=tag, 5=reserved, 6=ofs_delta, 7=ref_delta.
fn pack_body(entries: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (type_code, payload) in entries {
        out.extend_from_slice(&encode_size_varint(*type_code, payload.len() as u64));
        out.extend_from_slice(&zlib_compress(payload));
    }
    out
}

/// Append a SHA-1 trailer to a pack body.
fn pack_with_trailer(body: Vec<u8>) -> Vec<u8> {
    let mut out = body;
    let trailer = sha1(&out);
    out.extend_from_slice(&trailer);
    out
}

fn write_pack(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.path().join(format!("{name}.pack"));
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Round-trip helper: writes the given full-object entries to a temp
/// pack, builds the idx, and opens the pack.
fn write_and_open(dir: &TempDir, entries: &[(ObjectKind, &[u8])]) -> Pack {
    let mut writer = PackWriter::new();
    for (kind, payload) in entries {
        writer.add(*kind, payload);
    }
    let (bytes, pack_sha) = writer.finish().unwrap();
    let pack_path = dir.path().join(format!("pack-{}.pack", pack_sha.to_hex()));
    std::fs::write(&pack_path, &bytes).unwrap();
    let returned = build_index(&pack_path).unwrap();
    assert_eq!(returned, pack_sha);
    Pack::open(&pack_path).unwrap()
}

// ---------------------------------------------------------------------
// PACK-FMT — pack file format
// ---------------------------------------------------------------------

// @spec PACK-FMT-001
#[test]
fn writer_emits_pack_header_with_magic_version_count() {
    let mut w = PackWriter::new();
    w.add(ObjectKind::Blob, b"hello");
    let (bytes, _sha) = w.finish().unwrap();
    assert_eq!(&bytes[0..4], b"PACK");
    assert_eq!(&bytes[4..8], &2u32.to_be_bytes());
    assert_eq!(&bytes[8..12], &1u32.to_be_bytes());
}

// @spec PACK-FMT-002
#[test]
fn open_rejects_pack_with_wrong_version() {
    let dir = TempDir::new().unwrap();
    let mut body = b"PACK".to_vec();
    body.extend_from_slice(&3u32.to_be_bytes()); // version 3
    body.extend_from_slice(&0u32.to_be_bytes());
    let pack_bytes = pack_with_trailer(body);
    let path = write_pack(&dir, "pack-test", &pack_bytes);
    // No idx — but the pack header is rejected first.
    let result = Pack::open(&path);
    assert!(matches!(result, Err(PackError::BadHeader)));
}

// @spec PACK-FMT-003
#[test]
fn writer_appends_sha1_trailer_of_preceding_bytes() {
    let mut w = PackWriter::new();
    w.add(ObjectKind::Blob, b"hello");
    let (bytes, sha) = w.finish().unwrap();
    let body_len = bytes.len() - 20;
    let computed = sha1(&bytes[..body_len]);
    assert_eq!(&bytes[body_len..], &computed[..]);
    assert_eq!(sha.as_bytes(), &computed);
}

// @spec PACK-FMT-004
#[test]
fn open_rejects_pack_with_bad_trailer() {
    let dir = TempDir::new().unwrap();
    let mut body = pack_body(&[(3, b"hi")]);
    // Append a deliberately wrong trailer.
    body.extend_from_slice(&[0u8; 20]);
    let path = write_pack(&dir, "pack-bad-trailer", &body);
    // Build an idx so Pack::open doesn't bail on missing idx first.
    // We can't use build_index (which validates the trailer). Hand-roll
    // a minimal idx that "agrees" with the wrong trailer to exercise
    // the pack-trailer-check path.
    let idx_path = dir.path().join("pack-bad-trailer.idx");
    std::fs::write(&idx_path, minimal_idx_for_test_only()).unwrap();
    let result = Pack::open(&path);
    // Must be either BadPackSha or PackIdxMismatch — both indicate the
    // pack's stored trailer doesn't reflect its contents.
    assert!(matches!(
        result,
        Err(PackError::BadPackSha) | Err(PackError::PackIdxMismatch),
    ));
}

/// Construct a placeholder idx that's structurally a v2 header but
/// doesn't actually index anything. Used only by tests that exercise
/// the pack-side trailer-check path; tests that exercise idx validation
/// construct their own idx bytes.
fn minimal_idx_for_test_only() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\xfftOc");
    out.extend_from_slice(&2u32.to_be_bytes());
    // 256 fanout entries, all zero (zero objects).
    out.extend_from_slice(&[0u8; 1024]);
    // No object names, no crc32, no offset table.
    // Pack sha: zeros.
    out.extend_from_slice(&[0u8; 20]);
    // Idx trailer = SHA-1 of all preceding bytes.
    let trailer = sha1(&out);
    out.extend_from_slice(&trailer);
    out
}

// ---------------------------------------------------------------------
// PACK-ENTRY — entry encoding
// ---------------------------------------------------------------------

// @spec PACK-ENTRY-001, PACK-ENTRY-002, PACK-ENTRY-005
#[test]
fn writer_full_object_entry_round_trip_for_each_kind() {
    let dir = TempDir::new().unwrap();
    let entries: Vec<(ObjectKind, &[u8])> = vec![
        (ObjectKind::Commit, b"commit-bytes"),
        (ObjectKind::Tree, b"tree-bytes"),
        (ObjectKind::Blob, b"blob-bytes"),
        (ObjectKind::Tag, b"tag-bytes"),
    ];
    let pack = write_and_open(&dir, &entries);
    for (kind, payload) in &entries {
        // Compute the id the same way the pack would: SHA-1 of the
        // framed bytes.
        let id = ObjectId::compute(*kind, payload);
        let result = pack.lookup(&id).unwrap();
        let (read_kind, read_payload) = result.expect("entry present in pack");
        assert_eq!(read_kind, *kind);
        assert_eq!(read_payload, *payload);
    }
}

// @spec PACK-ENTRY-003
#[test]
fn pack_rejects_reserved_type_code_5() {
    let dir = TempDir::new().unwrap();
    // Hand-roll a pack with one entry using type code 5.
    let body = pack_body(&[(5, b"reserved")]);
    let pack_bytes = pack_with_trailer(body);
    let path = write_pack(&dir, "pack-type-5", &pack_bytes);
    // build_index would walk entries and reject. If build_index isn't
    // available, expect Pack::open to walk and reject during lookup.
    let result = build_index(&path);
    assert!(matches!(result, Err(PackError::UnknownType(5))));
}

// @spec PACK-ENTRY-004
#[test]
fn pack_rejects_unknown_type_code() {
    let dir = TempDir::new().unwrap();
    // Note: type field is 3 bits, max value 7. To get a truly "unknown"
    // code we need to construct one outside the recognized set. Code 0
    // is also unknown.
    let body = pack_body(&[(0, b"zero-type")]);
    let pack_bytes = pack_with_trailer(body);
    let path = write_pack(&dir, "pack-type-0", &pack_bytes);
    let result = build_index(&path);
    assert!(matches!(result, Err(PackError::UnknownType(0))));
}

// @spec PACK-ENTRY-006
#[test]
fn pack_rejects_corrupt_zlib_payload() {
    let dir = TempDir::new().unwrap();
    // Construct a body where the "compressed payload" is garbage.
    let mut body = Vec::new();
    body.extend_from_slice(b"PACK");
    body.extend_from_slice(&2u32.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes());
    // Type+size header for a 5-byte blob.
    body.extend_from_slice(&encode_size_varint(3, 5));
    // Garbage bytes that don't decompress.
    body.extend_from_slice(b"not valid zlib data here!");
    let pack_bytes = pack_with_trailer(body);
    let path = write_pack(&dir, "pack-bad-zlib", &pack_bytes);
    let result = build_index(&path);
    assert!(matches!(result, Err(PackError::BadInflate)));
}

// ---------------------------------------------------------------------
// PACK-DELTA — delta encoding and resolution
// ---------------------------------------------------------------------
//
// Delta-resolution tests need hand-rolled fixtures with valid delta
// instruction streams. The fixture helpers live alongside the delta
// impl (Phase 6) so they share the encoders. Until then this placeholder
// maintains spec-to-test traceability for the delta family.

// @spec PACK-DELTA-001, PACK-DELTA-002, PACK-DELTA-003, PACK-DELTA-004,
//       PACK-DELTA-005, PACK-DELTA-006, PACK-DELTA-007, PACK-DELTA-008,
//       PACK-DELTA-009, PACK-DELTA-010, PACK-DELTA-011
#[test]
#[ignore = "delta-resolution tests are landed alongside the delta-impl \
            in Phase 6; this placeholder maintains spec-to-test \
            traceability until then"]
fn delta_resolution_test_set_pending_phase_6() {
    // Per-spec tests for the delta encoding, instruction stream, chain
    // depth, target-size cap, and OFS/REF base-resolution paths land
    // when their helpers (synthetic-delta encoder) come online during
    // implementation.
}

// ---------------------------------------------------------------------
// PACK-IDX — index format
// ---------------------------------------------------------------------

// @spec PACK-IDX-001, PACK-IDX-003, PACK-IDX-004, PACK-IDX-006,
//       PACK-IDX-007
#[test]
fn build_index_produces_v2_idx_with_correct_headers_and_trailers() {
    let dir = TempDir::new().unwrap();
    let _pack = write_and_open(&dir, &[(ObjectKind::Blob, b"hello")]);
    // After write_and_open, exactly one .idx exists in the dir.
    let idx_entry = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.path().extension().is_some_and(|ext| ext == "idx"))
        .expect("idx file present after build_index");
    let idx_bytes = std::fs::read(idx_entry.path()).unwrap();
    // PACK-IDX-001: magic + version
    assert_eq!(&idx_bytes[0..4], b"\xfftOc");
    assert_eq!(&idx_bytes[4..8], &2u32.to_be_bytes());
    // PACK-IDX-007: trailer is SHA-1 of all preceding bytes
    let body_len = idx_bytes.len() - 20;
    let trailer = &idx_bytes[body_len..];
    assert_eq!(trailer, &sha1(&idx_bytes[..body_len])[..]);
}

// @spec PACK-IDX-002
#[test]
fn open_rejects_idx_with_wrong_version() {
    let dir = TempDir::new().unwrap();
    // Write a valid pack first.
    let mut w = PackWriter::new();
    w.add(ObjectKind::Blob, b"hi");
    let (pack_bytes, _) = w.finish().unwrap();
    let pack_path = dir.path().join("pack-test.pack");
    std::fs::write(&pack_path, &pack_bytes).unwrap();
    // Hand-roll an idx with version 3 instead of 2.
    let mut idx = Vec::new();
    idx.extend_from_slice(b"\xfftOc");
    idx.extend_from_slice(&3u32.to_be_bytes());
    idx.extend_from_slice(&[0u8; 1024]); // fanout
    idx.extend_from_slice(&[0u8; 20]); // pack-sha (zeros)
    let trailer = sha1(&idx);
    idx.extend_from_slice(&trailer);
    std::fs::write(dir.path().join("pack-test.idx"), &idx).unwrap();
    let result = Pack::open(&pack_path);
    assert!(matches!(result, Err(PackError::BadIndexHeader)));
}

// @spec PACK-IDX-005
#[test]
#[ignore = "8-byte offset path requires constructing a >2 GiB pack — \
            covered by a manual fixture in the build_index path tests \
            once a synthetic large-offset helper lands"]
fn idx_uses_8_byte_offset_for_entries_beyond_2gib() {}

// @spec PACK-IDX-008
#[test]
fn open_rejects_idx_with_bad_trailer() {
    let dir = TempDir::new().unwrap();
    let mut w = PackWriter::new();
    w.add(ObjectKind::Blob, b"hi");
    let (pack_bytes, _) = w.finish().unwrap();
    let pack_path = dir.path().join("pack-test.pack");
    std::fs::write(&pack_path, &pack_bytes).unwrap();
    // Hand-roll an idx with a corrupt trailer.
    let mut idx = Vec::new();
    idx.extend_from_slice(b"\xfftOc");
    idx.extend_from_slice(&2u32.to_be_bytes());
    idx.extend_from_slice(&[0u8; 1024]);
    idx.extend_from_slice(&[0u8; 20]);
    idx.extend_from_slice(&[0u8; 20]); // deliberately wrong trailer
    std::fs::write(dir.path().join("pack-test.idx"), &idx).unwrap();
    let result = Pack::open(&pack_path);
    assert!(matches!(result, Err(PackError::BadIndexSha)));
}

// @spec PACK-IDX-009
#[test]
fn open_rejects_when_pack_and_idx_disagree_on_pack_sha() {
    let dir = TempDir::new().unwrap();
    let mut w = PackWriter::new();
    w.add(ObjectKind::Blob, b"hi");
    let (pack_bytes, _) = w.finish().unwrap();
    let pack_path = dir.path().join("pack-test.pack");
    std::fs::write(&pack_path, &pack_bytes).unwrap();
    // Hand-roll an idx whose embedded pack-sha doesn't match the pack.
    let mut idx = Vec::new();
    idx.extend_from_slice(b"\xfftOc");
    idx.extend_from_slice(&2u32.to_be_bytes());
    idx.extend_from_slice(&[0u8; 1024]);
    idx.extend_from_slice(&[0xff; 20]); // deliberately wrong pack-sha
    let trailer = sha1(&idx);
    idx.extend_from_slice(&trailer);
    std::fs::write(dir.path().join("pack-test.idx"), &idx).unwrap();
    let result = Pack::open(&pack_path);
    assert!(matches!(result, Err(PackError::PackIdxMismatch)));
}

// ---------------------------------------------------------------------
// PACK-READ — Pack public API
// ---------------------------------------------------------------------

// @spec PACK-READ-001
#[test]
fn open_succeeds_for_well_formed_pack_and_idx() {
    let dir = TempDir::new().unwrap();
    let _pack = write_and_open(&dir, &[(ObjectKind::Blob, b"hi")]);
    // No assertion beyond "no error" — exhaustive validation specs are
    // PACK-FMT-* and PACK-IDX-*; this exercises the success path.
}

// @spec PACK-READ-002
#[test]
fn lookup_returns_some_for_present_id() {
    let dir = TempDir::new().unwrap();
    let pack = write_and_open(&dir, &[(ObjectKind::Blob, b"present")]);
    let id = ObjectId::compute(ObjectKind::Blob, b"present");
    let (kind, payload) = pack.lookup(&id).unwrap().expect("id present in pack");
    assert_eq!(kind, ObjectKind::Blob);
    assert_eq!(payload, b"present");
}

// @spec PACK-READ-003
#[test]
fn lookup_returns_none_for_missing_id() {
    let dir = TempDir::new().unwrap();
    let pack = write_and_open(&dir, &[(ObjectKind::Blob, b"only-entry")]);
    let other = ObjectId::compute(ObjectKind::Blob, b"not-in-pack");
    assert_eq!(pack.lookup(&other).unwrap(), None);
}

// @spec PACK-READ-004
#[test]
fn contains_matches_lookup() {
    let dir = TempDir::new().unwrap();
    let pack = write_and_open(&dir, &[(ObjectKind::Blob, b"x")]);
    let present = ObjectId::compute(ObjectKind::Blob, b"x");
    let absent = ObjectId::compute(ObjectKind::Blob, b"y");
    assert!(pack.contains(&present));
    assert!(!pack.contains(&absent));
}

// @spec PACK-READ-005
#[test]
fn iter_ids_yields_each_id_in_ascending_order() {
    let dir = TempDir::new().unwrap();
    let entries: &[(ObjectKind, &[u8])] = &[
        (ObjectKind::Blob, b"alpha"),
        (ObjectKind::Blob, b"beta"),
        (ObjectKind::Blob, b"gamma"),
    ];
    let pack = write_and_open(&dir, entries);
    let ids: Vec<&ObjectId> = pack.iter_ids().collect();
    assert_eq!(ids.len(), entries.len());
    for window in ids.windows(2) {
        assert!(window[0].as_bytes() <= window[1].as_bytes());
    }
}

// @spec PACK-READ-006
#[test]
fn pack_sha_matches_writer_returned_sha() {
    let dir = TempDir::new().unwrap();
    let mut w = PackWriter::new();
    w.add(ObjectKind::Blob, b"x");
    let (bytes, writer_sha) = w.finish().unwrap();
    let pack_path = dir.path().join("pack-sha-test.pack");
    std::fs::write(&pack_path, &bytes).unwrap();
    let _ = build_index(&pack_path).unwrap();
    let pack = Pack::open(&pack_path).unwrap();
    assert_eq!(pack.pack_sha(), writer_sha);
}

// @spec PACK-READ-007
#[test]
fn open_propagates_io_error_when_idx_missing() {
    let dir = TempDir::new().unwrap();
    let mut w = PackWriter::new();
    w.add(ObjectKind::Blob, b"orphan-pack");
    let (bytes, _) = w.finish().unwrap();
    let pack_path = dir.path().join("orphan.pack");
    std::fs::write(&pack_path, &bytes).unwrap();
    // Deliberately do not call build_index.
    let result = Pack::open(&pack_path);
    assert!(matches!(result, Err(PackError::Io(_))));
}

// ---------------------------------------------------------------------
// PACK-WRITE — PackWriter
// ---------------------------------------------------------------------

// @spec PACK-WRITE-001
#[test]
fn writer_add_appends_to_pending_list() {
    let mut w = PackWriter::new();
    assert_eq!(w.len(), 0);
    assert!(w.is_empty());
    w.add(ObjectKind::Blob, b"first");
    assert_eq!(w.len(), 1);
    assert!(!w.is_empty());
    w.add(ObjectKind::Tree, b"second");
    assert_eq!(w.len(), 2);
}

// @spec PACK-WRITE-002
#[test]
fn writer_finish_emits_v2_pack_with_header_and_trailer() {
    let mut w = PackWriter::new();
    w.add(ObjectKind::Blob, b"x");
    w.add(ObjectKind::Blob, b"y");
    let (bytes, _sha) = w.finish().unwrap();
    assert!(bytes.len() > 12 + 20); // header + at least one byte of body + trailer
    assert_eq!(&bytes[0..4], b"PACK");
    assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), 2);
    assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 2);
}

// @spec PACK-WRITE-003
#[test]
fn writer_output_is_deterministic_for_same_add_sequence() {
    let mut w1 = PackWriter::new();
    w1.add(ObjectKind::Blob, b"a");
    w1.add(ObjectKind::Tree, b"b");
    let (bytes1, sha1_) = w1.finish().unwrap();
    let mut w2 = PackWriter::new();
    w2.add(ObjectKind::Blob, b"a");
    w2.add(ObjectKind::Tree, b"b");
    let (bytes2, sha2) = w2.finish().unwrap();
    assert_eq!(bytes1, bytes2);
    assert_eq!(sha1_, sha2);
}

// @spec PACK-WRITE-004
#[test]
fn writer_finish_returns_trailer_sha_as_object_id() {
    let mut w = PackWriter::new();
    w.add(ObjectKind::Blob, b"x");
    let (bytes, sha) = w.finish().unwrap();
    let trailer = &bytes[bytes.len() - 20..];
    assert_eq!(sha.as_bytes(), trailer);
}

// @spec PACK-WRITE-005
#[test]
fn writer_with_zero_entries_produces_valid_empty_pack() {
    let w = PackWriter::new();
    let (bytes, _sha) = w.finish().unwrap();
    // Header 12 bytes + zero-byte body + 20-byte trailer = 32 bytes.
    assert_eq!(bytes.len(), 32);
    assert_eq!(&bytes[0..4], b"PACK");
    assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), 2);
    assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 0);
    let trailer = &bytes[12..];
    assert_eq!(trailer, &sha1(&bytes[..12])[..]);
}

// ---------------------------------------------------------------------
// PACK-BUILD — build_index
// ---------------------------------------------------------------------

// @spec PACK-BUILD-001, PACK-BUILD-005, PACK-BUILD-006
#[test]
fn build_index_produces_consumable_idx_for_full_object_pack() {
    let dir = TempDir::new().unwrap();
    // This is the end-to-end round-trip — if build_index produces a
    // valid v2 idx and the (id, offset) entries are sorted correctly,
    // Pack::open succeeds and lookup returns the entries.
    let pack = write_and_open(
        &dir,
        &[(ObjectKind::Blob, b"first"), (ObjectKind::Blob, b"second")],
    );
    let id1 = ObjectId::compute(ObjectKind::Blob, b"first");
    let id2 = ObjectId::compute(ObjectKind::Blob, b"second");
    assert!(pack.contains(&id1));
    assert!(pack.contains(&id2));
}

// @spec PACK-BUILD-002, PACK-BUILD-003, PACK-BUILD-004, PACK-BUILD-007
#[test]
#[ignore = "delta-resolution + duplicate-id + thin-pack + 8-byte-offset \
            paths require hand-rolled fixtures with valid deltas or \
            multi-GiB packs; these tests land in Phase 6 alongside the \
            synthetic-fixture helpers"]
fn build_index_delta_and_extreme_cases_pending_phase_6() {}

// ---------------------------------------------------------------------
// PACK-PARSE — cross-cutting
// ---------------------------------------------------------------------

// @spec PACK-PARSE-001
#[test]
fn parse_returns_truncated_for_incomplete_entry_header() {
    let dir = TempDir::new().unwrap();
    // Construct a pack with a body that ends mid-varint.
    let mut body = Vec::new();
    body.extend_from_slice(b"PACK");
    body.extend_from_slice(&2u32.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes());
    // A single byte with the continuation bit set — implies more bytes
    // follow, but none do.
    body.push(0xff);
    let pack_bytes = pack_with_trailer(body);
    let path = write_pack(&dir, "pack-truncated", &pack_bytes);
    let result = build_index(&path);
    assert!(matches!(result, Err(PackError::TruncatedEntry)));
}
