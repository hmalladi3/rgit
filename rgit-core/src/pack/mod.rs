//! Pack file format.
//!
//! Owns parsing and decoding of Git packfiles and their index files,
//! plus building an index for a freshly-received pack. Consumed by the
//! `odb` module for packed-object reads and `import_pack`.

#[cfg(test)]
mod tests;

use crate::object::{ObjectId, ObjectKind};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Maximum delta-chain depth tolerated during resolution.
pub const MAX_DELTA_CHAIN: usize = 50;

/// Maximum reconstructed delta target size (1 GiB). Larger values reject.
pub const DELTA_TARGET_CAP: u64 = 1024 * 1024 * 1024;

/// An open pack file with its companion `.idx`.
pub struct Pack {
    pack_path: PathBuf,
    pack_sha: ObjectId,
    // Backing buffers for the pack and the (parsed) index. In v1 we
    // load both into memory; mmap is deferred.
    pack_bytes: Vec<u8>,
    ids: Vec<ObjectId>,
    offsets: Vec<u64>,
}

impl Pack {
    /// Open a pack file. Validates pack + idx headers, both trailer
    /// hashes, and the pack-sha agreement between pack and idx.
    // @spec PACK-FMT-001, PACK-FMT-002, PACK-FMT-004, PACK-IDX-001,
    //       PACK-IDX-002, PACK-IDX-008, PACK-IDX-009,
    //       PACK-READ-001, PACK-READ-007
    pub fn open(pack_path: &Path) -> Result<Self, PackError> {
        let pack_bytes = std::fs::read(pack_path)?;
        let pack_sha = validate_pack_bytes(&pack_bytes)?;

        let idx_path = pack_path.with_extension("idx");
        let idx_bytes = std::fs::read(&idx_path)?;
        let parsed = parse_idx(&idx_bytes)?;
        if parsed.pack_sha != pack_sha {
            return Err(PackError::PackIdxMismatch);
        }

        Ok(Self {
            pack_path: pack_path.to_path_buf(),
            pack_sha,
            pack_bytes,
            ids: parsed.ids,
            offsets: parsed.offsets,
        })
    }

    /// Look up an object by id. Returns `Ok(None)` if this pack does
    /// not contain the id.
    // @spec PACK-READ-002, PACK-READ-003, PACK-ENTRY-001, PACK-ENTRY-002,
    //       PACK-ENTRY-003, PACK-ENTRY-004, PACK-ENTRY-005, PACK-ENTRY-006,
    //       PACK-DELTA-001, PACK-DELTA-002, PACK-DELTA-003, PACK-DELTA-004,
    //       PACK-DELTA-005, PACK-DELTA-006, PACK-DELTA-007, PACK-DELTA-008,
    //       PACK-DELTA-009, PACK-DELTA-010, PACK-DELTA-011,
    //       PACK-PARSE-001
    pub fn lookup(&self, id: &ObjectId) -> Result<Option<(ObjectKind, Vec<u8>)>, PackError> {
        let entry_idx = match find_id(&self.ids, id) {
            Some(i) => i,
            None => return Ok(None),
        };
        let offset = self.offsets[entry_idx] as usize;
        let (kind, payload) = self.read_entry_at(offset, 0)?;
        Ok(Some((kind, payload)))
    }

    /// True if this pack indexes the given id.
    // @spec PACK-READ-004
    pub fn contains(&self, id: &ObjectId) -> bool {
        find_id(&self.ids, id).is_some()
    }

    /// Recursively read and (if necessary) reconstruct an entry at the
    /// given byte offset. `depth` tracks delta-chain recursion.
    fn read_entry_at(
        &self,
        offset: usize,
        depth: usize,
    ) -> Result<(ObjectKind, Vec<u8>), PackError> {
        if depth > MAX_DELTA_CHAIN {
            return Err(PackError::DeltaChainTooDeep(MAX_DELTA_CHAIN));
        }
        let (type_code, size, header_len) = read_size_varint(&self.pack_bytes, offset)?;
        let after_header = offset + header_len;
        match type_code {
            TYPE_COMMIT | TYPE_TREE | TYPE_BLOB | TYPE_TAG => {
                let (payload, _) = zlib_decompress(&self.pack_bytes, after_header)?;
                if payload.len() != size as usize {
                    return Err(PackError::BadDelta);
                }
                Ok((full_type_code_to_kind(type_code)?, payload))
            }
            TYPE_OFS_DELTA => {
                let (back_off, ofs_len) = read_offset_varint(&self.pack_bytes, after_header)?;
                if back_off as usize >= offset || back_off == 0 {
                    return Err(PackError::BadDelta);
                }
                let base_offset = offset - back_off as usize;
                let (base_kind, base_payload) = self.read_entry_at(base_offset, depth + 1)?;
                let (instructions, _) = zlib_decompress(&self.pack_bytes, after_header + ofs_len)?;
                let target = apply_delta(&base_payload, &instructions)?;
                Ok((base_kind, target))
            }
            TYPE_REF_DELTA => {
                if after_header + 20 > self.pack_bytes.len() {
                    return Err(PackError::TruncatedEntry);
                }
                let mut id_bytes = [0u8; 20];
                id_bytes.copy_from_slice(&self.pack_bytes[after_header..after_header + 20]);
                let base = ObjectId::from_bytes(id_bytes);
                let base_idx = find_id(&self.ids, &base).ok_or(PackError::MissingBase(base))?;
                let base_offset = self.offsets[base_idx] as usize;
                let (base_kind, base_payload) = self.read_entry_at(base_offset, depth + 1)?;
                let (instructions, _) = zlib_decompress(&self.pack_bytes, after_header + 20)?;
                let target = apply_delta(&base_payload, &instructions)?;
                Ok((base_kind, target))
            }
            5 => Err(PackError::UnknownType(5)),
            other => Err(PackError::UnknownType(other)),
        }
    }

    /// Iterate every id in this pack in ascending id-sort order.
    // @spec PACK-READ-005, PACK-IDX-004
    pub fn iter_ids(&self) -> std::slice::Iter<'_, ObjectId> {
        self.ids.iter()
    }

    /// The pack file's trailer SHA-1 (also the `pack-sha` stored in the idx).
    // @spec PACK-READ-006
    pub fn pack_sha(&self) -> ObjectId {
        self.pack_sha
    }

    pub fn path(&self) -> &Path {
        &self.pack_path
    }
}

/// Build a `.idx` file alongside the given `.pack` by walking entries,
/// resolving deltas, and recording the `(id, offset, crc32)` table.
/// Returns the pack's trailer SHA so callers can rename the file
/// canonically.
// @spec PACK-BUILD-001, PACK-BUILD-002, PACK-BUILD-003, PACK-BUILD-004,
//       PACK-BUILD-005, PACK-BUILD-006, PACK-BUILD-007,
//       PACK-IDX-003, PACK-IDX-005, PACK-IDX-006, PACK-IDX-007
pub fn build_index(pack_path: &Path) -> Result<ObjectId, PackError> {
    let pack_bytes = std::fs::read(pack_path)?;
    let pack_sha = validate_pack_bytes(&pack_bytes)?;
    let nobj = u32::from_be_bytes(pack_bytes[8..12].try_into().unwrap()) as usize;

    // Walk every entry, recording the type, declared size, raw byte
    // range, and any delta-base hints.
    let mut raw: Vec<RawEntry> = Vec::with_capacity(nobj);
    let mut cursor = HEADER_LEN;
    for _ in 0..nobj {
        let entry_start = cursor;
        let (type_code, size, header_len) = read_size_varint(&pack_bytes, cursor)?;
        cursor += header_len;

        let (ofs_base, ref_base) = match type_code {
            TYPE_COMMIT | TYPE_TREE | TYPE_BLOB | TYPE_TAG => (None, None),
            TYPE_OFS_DELTA => {
                let (back_off, used) = read_offset_varint(&pack_bytes, cursor)?;
                cursor += used;
                if back_off as usize >= entry_start || back_off == 0 {
                    return Err(PackError::BadDelta);
                }
                (Some(entry_start as u64 - back_off), None)
            }
            TYPE_REF_DELTA => {
                if cursor + 20 > pack_bytes.len() {
                    return Err(PackError::TruncatedEntry);
                }
                let mut id_bytes = [0u8; 20];
                id_bytes.copy_from_slice(&pack_bytes[cursor..cursor + 20]);
                cursor += 20;
                (None, Some(ObjectId::from_bytes(id_bytes)))
            }
            5 => return Err(PackError::UnknownType(5)),
            n => return Err(PackError::UnknownType(n)),
        };

        let (_, used) = zlib_decompress(&pack_bytes, cursor)?;
        cursor += used;
        let entry_end = cursor;

        raw.push(RawEntry {
            offset: entry_start as u64,
            byte_range: (entry_start, entry_end),
            type_code,
            size,
            ofs_base,
            ref_base,
        });
    }

    // Resolve each entry to an `(ObjectKind, ObjectId, crc32)` tuple.
    let offset_to_idx: std::collections::HashMap<u64, usize> =
        raw.iter().enumerate().map(|(i, e)| (e.offset, i)).collect();

    let mut resolved: Vec<Option<(ObjectKind, ObjectId)>> = vec![None; nobj];
    // First pass: resolve everything except REF_DELTA entries whose base
    // we cannot yet name. This catches all full objects and all OFS_DELTA
    // chains (which never depend on REF lookup).
    for i in 0..nobj {
        if resolved[i].is_some() {
            continue;
        }
        if matches!(raw[i].type_code, TYPE_REF_DELTA) {
            continue;
        }
        let (kind, payload) = resolve_payload(i, &raw, &pack_bytes, &offset_to_idx, &resolved, 0)?;
        let id = ObjectId::compute(kind, &payload);
        resolved[i] = Some((kind, id));
    }

    // Second pass: iterate to fixed point on REF_DELTAs, since their
    // bases may themselves be REF_DELTAs that have just resolved.
    loop {
        let mut progressed = false;
        for i in 0..nobj {
            if resolved[i].is_some() {
                continue;
            }
            let ref_base = raw[i].ref_base.expect("REF_DELTA has ref_base");
            // Look up base id in already-resolved entries.
            let base_idx = resolved
                .iter()
                .position(|slot| matches!(slot, Some((_, id)) if *id == ref_base));
            let Some(base_idx) = base_idx else {
                continue;
            };
            // We can resolve this entry now.
            let (kind, payload) =
                resolve_ref_delta_payload(i, base_idx, &raw, &pack_bytes, &resolved)?;
            let id = ObjectId::compute(kind, &payload);
            resolved[i] = Some((kind, id));
            progressed = true;
        }
        if resolved.iter().all(Option::is_some) {
            break;
        }
        if !progressed {
            // Some REF_DELTA's base is not in this pack — thin pack.
            let unresolved_idx = resolved.iter().position(Option::is_none).unwrap();
            let base = raw[unresolved_idx]
                .ref_base
                .expect("only unresolved REF_DELTAs remain");
            return Err(PackError::ThinPackUnsupported(base));
        }
    }

    // Detect duplicate ids.
    let mut entries: Vec<(ObjectId, u64, u32)> = Vec::with_capacity(nobj);
    {
        let mut seen: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
        for (i, slot) in resolved.iter().enumerate() {
            let (_kind, id) = slot.expect("all entries resolved");
            if !seen.insert(id) {
                return Err(PackError::DuplicateIdInPack(id));
            }
            let (s, e) = raw[i].byte_range;
            let crc = crc32fast::hash(&pack_bytes[s..e]);
            entries.push((id, raw[i].offset, crc));
        }
    }

    // Sort by id ascending.
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

    // Write idx.
    let idx_bytes = write_idx(&entries, &pack_sha);
    let idx_path = pack_path.with_extension("idx");
    std::fs::write(&idx_path, &idx_bytes)?;

    Ok(pack_sha)
}

/// Internal accumulator while walking pack entries during build_index.
struct RawEntry {
    offset: u64,
    /// Byte range in the pack for this entry: header + compressed payload.
    byte_range: (usize, usize),
    type_code: u8,
    #[allow(dead_code)]
    size: u64,
    /// For OFS_DELTA: the resolved base entry offset within the pack.
    ofs_base: Option<u64>,
    /// For REF_DELTA: the base id named by the entry's 20 SHA bytes.
    ref_base: Option<ObjectId>,
}

fn resolve_payload(
    idx: usize,
    raw: &[RawEntry],
    pack_bytes: &[u8],
    offset_to_idx: &std::collections::HashMap<u64, usize>,
    _resolved: &[Option<(ObjectKind, ObjectId)>],
    depth: usize,
) -> Result<(ObjectKind, Vec<u8>), PackError> {
    if depth > MAX_DELTA_CHAIN {
        return Err(PackError::DeltaChainTooDeep(MAX_DELTA_CHAIN));
    }
    let entry = &raw[idx];
    match entry.type_code {
        TYPE_COMMIT | TYPE_TREE | TYPE_BLOB | TYPE_TAG => {
            let kind = full_type_code_to_kind(entry.type_code)?;
            let (_, _, header_len) = read_size_varint(pack_bytes, entry.offset as usize)?;
            let after_header = entry.offset as usize + header_len;
            let (payload, _) = zlib_decompress(pack_bytes, after_header)?;
            Ok((kind, payload))
        }
        TYPE_OFS_DELTA => {
            let base_offset = entry.ofs_base.expect("OFS_DELTA has ofs_base");
            let base_idx = *offset_to_idx.get(&base_offset).ok_or(PackError::BadDelta)?;
            let (base_kind, base_payload) = resolve_payload(
                base_idx,
                raw,
                pack_bytes,
                offset_to_idx,
                _resolved,
                depth + 1,
            )?;
            let (_, _, header_len) = read_size_varint(pack_bytes, entry.offset as usize)?;
            let after_header = entry.offset as usize + header_len;
            let (_, ofs_len) = read_offset_varint(pack_bytes, after_header)?;
            let (instructions, _) = zlib_decompress(pack_bytes, after_header + ofs_len)?;
            let target = apply_delta(&base_payload, &instructions)?;
            Ok((base_kind, target))
        }
        TYPE_REF_DELTA => {
            // REF_DELTA resolution requires the resolved-id map; the
            // caller invokes `resolve_ref_delta_payload` directly.
            unreachable!("REF_DELTA not handled by resolve_payload — use resolve_ref_delta_payload")
        }
        _ => unreachable!("validated during raw walk"),
    }
}

fn resolve_ref_delta_payload(
    idx: usize,
    base_idx: usize,
    raw: &[RawEntry],
    pack_bytes: &[u8],
    resolved: &[Option<(ObjectKind, ObjectId)>],
) -> Result<(ObjectKind, Vec<u8>), PackError> {
    // Recompute the base's payload (memoizing payloads is too expensive
    // for big packs; we re-resolve on demand).
    let offset_to_idx: std::collections::HashMap<u64, usize> =
        raw.iter().enumerate().map(|(i, e)| (e.offset, i)).collect();
    let (base_kind, base_payload) = if matches!(raw[base_idx].type_code, TYPE_REF_DELTA) {
        // Base is itself a REF_DELTA — its base must already be resolved.
        let inner_base_id = raw[base_idx].ref_base.expect("REF_DELTA has ref_base");
        let inner_base_idx = resolved
            .iter()
            .position(|slot| matches!(slot, Some((_, id)) if *id == inner_base_id))
            .ok_or(PackError::ThinPackUnsupported(inner_base_id))?;
        resolve_ref_delta_payload(base_idx, inner_base_idx, raw, pack_bytes, resolved)?
    } else {
        resolve_payload(base_idx, raw, pack_bytes, &offset_to_idx, resolved, 0)?
    };

    let entry = &raw[idx];
    let (_, _, header_len) = read_size_varint(pack_bytes, entry.offset as usize)?;
    let after_header = entry.offset as usize + header_len;
    let (instructions, _) = zlib_decompress(pack_bytes, after_header + 20)?;
    let target = apply_delta(&base_payload, &instructions)?;
    Ok((base_kind, target))
}

/// Validate pack bytes' header (magic, version) and trailer hash.
/// Returns the pack's trailer SHA as `ObjectId`.
fn validate_pack_bytes(pack_bytes: &[u8]) -> Result<ObjectId, PackError> {
    if pack_bytes.len() < HEADER_LEN + TRAILER_LEN {
        return Err(PackError::BadHeader);
    }
    if &pack_bytes[..4] != PACK_MAGIC {
        return Err(PackError::BadHeader);
    }
    let version = u32::from_be_bytes(pack_bytes[4..8].try_into().unwrap());
    if version != PACK_VERSION {
        return Err(PackError::BadHeader);
    }
    let body_len = pack_bytes.len() - TRAILER_LEN;
    let stored: &[u8; 20] = pack_bytes[body_len..].try_into().unwrap();
    let computed = sha1_bytes(&pack_bytes[..body_len]);
    if stored != &computed {
        return Err(PackError::BadPackSha);
    }
    Ok(ObjectId::from_bytes(computed))
}

/// Read a delta-stream size varint (low-bits-first, like PACK-ENTRY-001
/// but with no type-code prefix).
fn read_delta_size_varint(data: &[u8], at: usize) -> Result<(u64, usize), PackError> {
    let mut value: u64 = 0;
    let mut shift = 0;
    let mut idx = at;
    loop {
        if idx >= data.len() {
            return Err(PackError::BadDelta);
        }
        let byte = data[idx];
        value |= u64::from(byte & 0x7f) << shift;
        idx += 1;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Ok((value, idx - at))
}

/// Apply delta instructions to a base payload, returning the reconstructed
/// target.
fn apply_delta(base: &[u8], instructions: &[u8]) -> Result<Vec<u8>, PackError> {
    let (source_size, idx1) = read_delta_size_varint(instructions, 0)?;
    let (target_size, idx2) = read_delta_size_varint(instructions, idx1)?;
    if source_size as usize != base.len() {
        return Err(PackError::BadDelta);
    }
    if target_size > DELTA_TARGET_CAP {
        return Err(PackError::DeltaTargetTooLarge(
            target_size,
            DELTA_TARGET_CAP,
        ));
    }
    let mut out = Vec::with_capacity(target_size as usize);
    let mut cursor = idx2;
    while cursor < instructions.len() {
        let op = instructions[cursor];
        cursor += 1;
        if op & 0x80 != 0 {
            let mut offset: u32 = 0;
            for bit in 0..4 {
                if op & (1 << bit) != 0 {
                    if cursor >= instructions.len() {
                        return Err(PackError::BadDelta);
                    }
                    offset |= u32::from(instructions[cursor]) << (bit * 8);
                    cursor += 1;
                }
            }
            let mut size: u32 = 0;
            for bit in 0..3 {
                if op & (1 << (4 + bit)) != 0 {
                    if cursor >= instructions.len() {
                        return Err(PackError::BadDelta);
                    }
                    size |= u32::from(instructions[cursor]) << (bit * 8);
                    cursor += 1;
                }
            }
            let size = if size == 0 { 0x10000 } else { size };
            let start = offset as usize;
            let end = start
                .checked_add(size as usize)
                .ok_or(PackError::BadDelta)?;
            if end > base.len() {
                return Err(PackError::BadDelta);
            }
            out.extend_from_slice(&base[start..end]);
        } else if op == 0 {
            return Err(PackError::BadDelta);
        } else {
            let n = op as usize;
            if cursor + n > instructions.len() {
                return Err(PackError::BadDelta);
            }
            out.extend_from_slice(&instructions[cursor..cursor + n]);
            cursor += n;
        }
    }
    if out.len() != target_size as usize {
        return Err(PackError::BadDelta);
    }
    Ok(out)
}

/// Binary search a sorted id list for the given id.
fn find_id(ids: &[ObjectId], target: &ObjectId) -> Option<usize> {
    ids.binary_search_by(|id| id.as_bytes().cmp(target.as_bytes()))
        .ok()
}

// ---------------------------------------------------------------------
// Index file parsing and writing
// ---------------------------------------------------------------------

struct ParsedIdx {
    pack_sha: ObjectId,
    ids: Vec<ObjectId>,
    offsets: Vec<u64>,
}

fn parse_idx(idx_bytes: &[u8]) -> Result<ParsedIdx, PackError> {
    if idx_bytes.len() < 8 + FANOUT_LEN + TRAILER_LEN + TRAILER_LEN {
        return Err(PackError::BadIndexHeader);
    }
    if &idx_bytes[..4] != IDX_MAGIC {
        return Err(PackError::BadIndexHeader);
    }
    let version = u32::from_be_bytes(idx_bytes[4..8].try_into().unwrap());
    if version != IDX_VERSION {
        return Err(PackError::BadIndexHeader);
    }

    // Validate trailer first.
    let body_len = idx_bytes.len() - TRAILER_LEN;
    let stored: &[u8; 20] = idx_bytes[body_len..].try_into().unwrap();
    let computed = sha1_bytes(&idx_bytes[..body_len]);
    if stored != &computed {
        return Err(PackError::BadIndexSha);
    }

    // Fanout table.
    let fanout_start = 8;
    let mut fanout = [0u32; 256];
    for (i, slot) in fanout.iter_mut().enumerate() {
        let off = fanout_start + i * 4;
        *slot = u32::from_be_bytes(idx_bytes[off..off + 4].try_into().unwrap());
    }
    let nobj = fanout[255] as usize;

    // Object names.
    let names_start = fanout_start + FANOUT_LEN;
    let mut ids = Vec::with_capacity(nobj);
    for i in 0..nobj {
        let off = names_start + i * 20;
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(&idx_bytes[off..off + 20]);
        ids.push(ObjectId::from_bytes(bytes));
    }

    // Skip CRC32 table.
    let crc_start = names_start + nobj * 20;
    let off4_start = crc_start + nobj * 4;
    let off8_start = off4_start + nobj * 4;

    // Read 4-byte offsets; route MSB-set slots through 8-byte table.
    let mut offsets = Vec::with_capacity(nobj);
    let mut large_offset_cursor = off8_start;
    for i in 0..nobj {
        let off = off4_start + i * 4;
        let raw = u32::from_be_bytes(idx_bytes[off..off + 4].try_into().unwrap());
        if raw & 0x8000_0000 != 0 {
            if large_offset_cursor + 8 > body_len - 20 {
                return Err(PackError::BadIndexHeader);
            }
            let big = u64::from_be_bytes(
                idx_bytes[large_offset_cursor..large_offset_cursor + 8]
                    .try_into()
                    .unwrap(),
            );
            offsets.push(big);
            large_offset_cursor += 8;
        } else {
            offsets.push(u64::from(raw));
        }
    }

    // Pack-sha lives in the final 40 bytes (pack-sha + idx-sha).
    let pack_sha_start = body_len - 20;
    let mut pack_sha_bytes = [0u8; 20];
    pack_sha_bytes.copy_from_slice(&idx_bytes[pack_sha_start..pack_sha_start + 20]);

    Ok(ParsedIdx {
        pack_sha: ObjectId::from_bytes(pack_sha_bytes),
        ids,
        offsets,
    })
}

fn write_idx(entries: &[(ObjectId, u64, u32)], pack_sha: &ObjectId) -> Vec<u8> {
    let nobj = entries.len();
    let mut out = Vec::new();

    out.extend_from_slice(IDX_MAGIC);
    out.extend_from_slice(&IDX_VERSION.to_be_bytes());

    // Fanout: 256 cumulative counts.
    let mut fanout = [0u32; 256];
    for (id, _, _) in entries {
        let first = id.as_bytes()[0] as usize;
        fanout[first] += 1;
    }
    let mut cum: u32 = 0;
    for slot in fanout.iter_mut() {
        cum += *slot;
        *slot = cum;
    }
    for n in fanout {
        out.extend_from_slice(&n.to_be_bytes());
    }

    // Object names.
    for (id, _, _) in entries {
        out.extend_from_slice(id.as_bytes());
    }
    // CRC32 table.
    for (_, _, crc) in entries {
        out.extend_from_slice(&crc.to_be_bytes());
    }

    // Build 4-byte and 8-byte offset tables.
    let mut large_offsets: Vec<u64> = Vec::new();
    for (_, off, _) in entries {
        if *off >= 0x8000_0000 {
            let large_idx = large_offsets.len() as u32;
            out.extend_from_slice(&(large_idx | 0x8000_0000).to_be_bytes());
            large_offsets.push(*off);
        } else {
            out.extend_from_slice(&(*off as u32).to_be_bytes());
        }
    }
    for off in &large_offsets {
        out.extend_from_slice(&off.to_be_bytes());
    }

    // Pack sha (matches pack trailer).
    out.extend_from_slice(pack_sha.as_bytes());

    // Idx trailer.
    let trailer = sha1_bytes(&out);
    out.extend_from_slice(&trailer);

    let _ = nobj;
    out
}

/// A pack being assembled in memory for push.
#[derive(Default)]
pub struct PackWriter {
    entries: Vec<(ObjectKind, Vec<u8>)>,
}

impl PackWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a full-object entry. v1 does not delta-encode; every
    /// entry is a full copy of the payload.
    // @spec PACK-WRITE-001
    pub fn add(&mut self, kind: ObjectKind, payload: &[u8]) {
        self.entries.push((kind, payload.to_vec()));
    }

    /// Finalize the pack. Returns `(pack_bytes, pack_sha)`.
    // @spec PACK-FMT-001, PACK-FMT-003, PACK-WRITE-002, PACK-WRITE-003,
    //       PACK-WRITE-004, PACK-WRITE-005
    pub fn finish(self) -> Result<(Vec<u8>, ObjectId), PackError> {
        let mut out = Vec::new();
        out.extend_from_slice(b"PACK");
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        for (kind, payload) in &self.entries {
            let type_code = kind_to_type_code(*kind);
            write_size_varint(&mut out, type_code, payload.len() as u64);
            let compressed = zlib_compress(payload)?;
            out.extend_from_slice(&compressed);
        }
        let trailer = sha1_bytes(&out);
        out.extend_from_slice(&trailer);
        Ok((out, ObjectId::from_bytes(trailer)))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------
// Format helpers
// ---------------------------------------------------------------------

const PACK_MAGIC: &[u8; 4] = b"PACK";
const IDX_MAGIC: &[u8; 4] = b"\xfftOc";
const PACK_VERSION: u32 = 2;
const IDX_VERSION: u32 = 2;
const HEADER_LEN: usize = 12;
const TRAILER_LEN: usize = 20;
const FANOUT_LEN: usize = 256 * 4;

const TYPE_COMMIT: u8 = 1;
const TYPE_TREE: u8 = 2;
const TYPE_BLOB: u8 = 3;
const TYPE_TAG: u8 = 4;
const TYPE_OFS_DELTA: u8 = 6;
const TYPE_REF_DELTA: u8 = 7;

fn kind_to_type_code(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Commit => TYPE_COMMIT,
        ObjectKind::Tree => TYPE_TREE,
        ObjectKind::Blob => TYPE_BLOB,
        ObjectKind::Tag => TYPE_TAG,
    }
}

fn full_type_code_to_kind(code: u8) -> Result<ObjectKind, PackError> {
    match code {
        TYPE_COMMIT => Ok(ObjectKind::Commit),
        TYPE_TREE => Ok(ObjectKind::Tree),
        TYPE_BLOB => Ok(ObjectKind::Blob),
        TYPE_TAG => Ok(ObjectKind::Tag),
        n => Err(PackError::UnknownType(n)),
    }
}

/// Encode the pack entry type+size header per PACK-ENTRY-001.
fn write_size_varint(out: &mut Vec<u8>, type_code: u8, size: u64) {
    let low4 = (size & 0x0f) as u8;
    let mut rest = size >> 4;
    let first_cont = if rest > 0 { 0x80 } else { 0 };
    out.push(first_cont | ((type_code & 0x07) << 4) | low4);
    while rest > 0 {
        let cont = if rest > 0x7f { 0x80 } else { 0 };
        out.push(cont | (rest & 0x7f) as u8);
        rest >>= 7;
    }
}

/// Decode an entry type+size header from `data[at..]`. Returns
/// `(type_code, size, bytes_consumed)`.
fn read_size_varint(data: &[u8], at: usize) -> Result<(u8, u64, usize), PackError> {
    if at >= data.len() {
        return Err(PackError::TruncatedEntry);
    }
    let first = data[at];
    let type_code = (first >> 4) & 0x07;
    let mut size = (first & 0x0f) as u64;
    let mut shift = 4;
    let mut idx = at;
    let mut byte = first;
    while byte & 0x80 != 0 {
        idx += 1;
        if idx >= data.len() {
            return Err(PackError::TruncatedEntry);
        }
        byte = data[idx];
        size |= u64::from(byte & 0x7f) << shift;
        shift += 7;
    }
    Ok((type_code, size, idx + 1 - at))
}

/// Decode the OFS_DELTA back-offset varint per PACK-DELTA-001. Returns
/// `(value, bytes_consumed)`.
fn read_offset_varint(data: &[u8], at: usize) -> Result<(u64, usize), PackError> {
    if at >= data.len() {
        return Err(PackError::TruncatedEntry);
    }
    let mut value: u64 = u64::from(data[at] & 0x7f);
    let mut idx = at;
    let mut byte = data[at];
    while byte & 0x80 != 0 {
        idx += 1;
        if idx >= data.len() {
            return Err(PackError::TruncatedEntry);
        }
        byte = data[idx];
        value = ((value + 1) << 7) | u64::from(byte & 0x7f);
    }
    Ok((value, idx + 1 - at))
}

fn sha1_bytes(bytes: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(bytes);
    let mut out = [0u8; 20];
    out.copy_from_slice(&h.finalize());
    out
}

fn zlib_compress(bytes: &[u8]) -> Result<Vec<u8>, PackError> {
    use std::io::Write as _;
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(bytes)?;
    enc.finish().map_err(PackError::from)
}

/// Decompress a zlib stream beginning at `data[at..]`. Returns the
/// decompressed bytes and the number of input bytes consumed.
fn zlib_decompress(data: &[u8], at: usize) -> Result<(Vec<u8>, usize), PackError> {
    use std::io::Read as _;
    let mut dec = flate2::read::ZlibDecoder::new(&data[at..]);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|_| PackError::BadInflate)?;
    Ok((out, dec.total_in() as usize))
}

/// Errors returned by the pack module.
#[derive(Debug, Error)]
pub enum PackError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not a pack file (bad magic or version)")]
    BadHeader,

    #[error("not an index file (bad magic or version)")]
    BadIndexHeader,

    #[error("pack trailer hash mismatch")]
    BadPackSha,

    #[error("index trailer hash mismatch")]
    BadIndexSha,

    #[error("pack and idx disagree on pack-sha")]
    PackIdxMismatch,

    #[error("unknown object type code: {0}")]
    UnknownType(u8),

    #[error("truncated entry")]
    TruncatedEntry,

    #[error("zlib inflate failed")]
    BadInflate,

    #[error("invalid delta instruction")]
    BadDelta,

    #[error("delta base not found in pack: {0}")]
    MissingBase(ObjectId),

    #[error("delta chain depth exceeds limit ({0})")]
    DeltaChainTooDeep(usize),

    #[error("delta target size {0} exceeds the {1}-byte cap")]
    DeltaTargetTooLarge(u64, u64),

    #[error("pack contains duplicate id: {0}")]
    DuplicateIdInPack(ObjectId),

    #[error("thin pack not supported (base object {0} not present in pack)")]
    ThinPackUnsupported(ObjectId),

    #[error("pack module not yet implemented")]
    NotImplemented,
}
