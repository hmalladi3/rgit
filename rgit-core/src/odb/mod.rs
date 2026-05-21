//! Object database.
//!
//! Bridges the in-memory [`Object`] model and on-disk storage. Every
//! operation in rgit that names an object by id flows through this module,
//! which owns hash verification on read, atomic writes for loose objects,
//! and transparent dispatch between loose and packed storage.

#[cfg(test)]
mod tests;

use crate::object::{Object, ObjectId, ObjectKind, ParseError};
use crate::pack::{Pack, PackError};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

/// A handle to a Git repository.
pub struct Repository {
    git_dir: PathBuf,
    work_dir: Option<PathBuf>,
    /// Packs discovered at open time, ordered newest mtime first
    /// (ODB-PACK-006). Empty for repos opened via `init`; populated by
    /// `open` from `.git/objects/pack/`.
    packs: Vec<Pack>,
    /// Packed-refs entries loaded at open time. Empty for repos opened
    /// via `init` (no packed-refs file in a fresh repo).
    pub(crate) packed_refs: std::collections::HashMap<String, ObjectId>,
}

impl Repository {
    /// Open an existing repository at `path`. Walks parent directories
    /// looking for a valid git dir, stopping at filesystem boundaries.
    // @spec ODB-REPO-001, ODB-REPO-002, ODB-REPO-003, ODB-REPO-004
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OdbError> {
        let original = path.as_ref().to_path_buf();
        let canonical = original
            .canonicalize()
            .map_err(|_| OdbError::NotARepository(original.clone()))?;
        let start_dev = device_id(&canonical)?;

        let mut current = canonical;
        loop {
            let in_work_tree = current.join(".git");
            if is_valid_git_dir(&in_work_tree) {
                let packs = discover_packs(&in_work_tree)?;
                let packed_refs = crate::refs::load_packed_refs(&in_work_tree)?;
                return Ok(Repository {
                    git_dir: in_work_tree,
                    work_dir: Some(current),
                    packs,
                    packed_refs,
                });
            }
            if is_valid_git_dir(&current) {
                let packs = discover_packs(&current)?;
                let packed_refs = crate::refs::load_packed_refs(&current)?;
                return Ok(Repository {
                    git_dir: current,
                    work_dir: None,
                    packs,
                    packed_refs,
                });
            }
            let parent = match current.parent() {
                Some(p) if p != current => p.to_path_buf(),
                _ => return Err(OdbError::NotARepository(original)),
            };
            if device_id(&parent)? != start_dev {
                return Err(OdbError::NotARepository(original));
            }
            current = parent;
        }
    }

    /// Create a new repository at `path`. If `bare`, `path` itself
    /// becomes the git dir; otherwise `path / ".git"` is created.
    // @spec ODB-REPO-005, ODB-REPO-006, ODB-REPO-007, ODB-REPO-008
    pub fn init(path: impl AsRef<Path>, bare: bool) -> Result<Self, OdbError> {
        let path = path.as_ref();
        let (git_dir, work_dir) = if bare {
            (path.to_path_buf(), None)
        } else {
            (path.join(".git"), Some(path.to_path_buf()))
        };

        fs::create_dir_all(&git_dir)?;

        // ODB-REPO-007: re-init on an already-valid git dir is a no-op.
        if is_valid_git_dir(&git_dir) {
            let packs = discover_packs(&git_dir)?;
            let packed_refs = crate::refs::load_packed_refs(&git_dir)?;
            return Ok(Repository {
                git_dir,
                work_dir,
                packs,
                packed_refs,
            });
        }

        // ODB-REPO-005/006/008: create missing components without overwriting.
        fs::create_dir_all(git_dir.join("objects"))?;
        fs::create_dir_all(git_dir.join("refs/heads"))?;
        fs::create_dir_all(git_dir.join("refs/tags"))?;
        let head_path = git_dir.join("HEAD");
        if !head_path.exists() {
            fs::write(&head_path, b"ref: refs/heads/main\n")?;
        }

        Ok(Repository {
            git_dir,
            work_dir,
            packs: Vec::new(),
            packed_refs: std::collections::HashMap::new(),
        })
    }

    /// Read and parse an object by id. Hash-verifies before returning.
    // @spec ODB-READ-001, ODB-READ-002, ODB-READ-004, ODB-READ-005,
    //       ODB-READ-006, ODB-HASH-001, ODB-HASH-004,
    //       ODB-PACK-001, ODB-PACK-002
    pub fn read_object(&self, id: &ObjectId) -> Result<Object, OdbError> {
        if let Some(inflated) = self.read_loose_validated(id)? {
            return Object::parse_loose(&inflated).map_err(OdbError::InvalidObject);
        }
        let (kind, payload) = self.read_packed_payload(id)?;
        Object::parse_payload(kind, &payload).map_err(OdbError::InvalidObject)
    }

    /// Read the frameless payload + kind of an object by id, without
    /// constructing the structured representation. Hash-verifies.
    // @spec ODB-READ-003, ODB-HASH-001, ODB-PACK-001, ODB-PACK-002
    pub fn read_object_raw(&self, id: &ObjectId) -> Result<(ObjectKind, Vec<u8>), OdbError> {
        if let Some(inflated) = self.read_loose_validated(id)? {
            let (kind, payload_start) = parse_loose_kind_and_payload_start(&inflated)?;
            return Ok((kind, inflated[payload_start..].to_vec()));
        }
        self.read_packed_payload(id)
    }

    /// Search packs in registration order for `id`, hash-verifying the
    /// reconstructed loose-frame against the requested id.
    // @spec ODB-PACK-001, ODB-PACK-002
    fn read_packed_payload(&self, id: &ObjectId) -> Result<(ObjectKind, Vec<u8>), OdbError> {
        for pack in &self.packs {
            if let Some((kind, payload)) = pack.lookup(id)? {
                let computed = ObjectId::compute(kind, &payload);
                if computed != *id {
                    return Err(OdbError::HashMismatch {
                        expected: *id,
                        computed,
                    });
                }
                return Ok((kind, payload));
            }
        }
        Err(OdbError::ObjectNotFound(*id))
    }

    /// Store the object as a loose file atomically. Returns the id.
    // @spec ODB-WRITE-001, ODB-WRITE-002, ODB-WRITE-003, ODB-WRITE-004,
    //       ODB-WRITE-005, ODB-WRITE-006, ODB-LOOSE-001, ODB-LOOSE-002
    pub fn write_object(&self, object: &Object) -> Result<ObjectId, OdbError> {
        let frame = object.serialize();
        let id = object.id();
        let path = loose_path(&self.git_dir, &id);

        // Defense in depth: if the destination already exists, hash-verify
        // its content before declaring success (ODB-WRITE-004 / ODB-WRITE-005).
        if path.exists() {
            return verify_existing_loose(&path, &id);
        }

        let parent = path.parent().expect("loose path has parent");
        fs::create_dir_all(parent)?;

        let tmp_path = parent.join(tmp_name());
        {
            let mut f = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp_path)?;
            let mut enc = flate2::write::ZlibEncoder::new(&mut f, flate2::Compression::default());
            enc.write_all(&frame)?;
            enc.finish()?;
            f.sync_all()?;
        }

        match fs::rename(&tmp_path, &path) {
            Ok(()) => {}
            Err(_) if path.exists() => {
                // Concurrent writer beat us. Best-effort cleanup, then
                // verify the existing file matches.
                let _ = fs::remove_file(&tmp_path);
                return verify_existing_loose(&path, &id);
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(OdbError::Io(e));
            }
        }

        // Make the rename durable.
        if let Ok(dir_handle) = fs::File::open(parent) {
            let _ = dir_handle.sync_all();
        }

        Ok(id)
    }

    /// True if an object with this id is present in loose or pack storage.
    // @spec ODB-CONTAINS-001, ODB-HASH-003, ODB-PACK-001
    pub fn contains(&self, id: &ObjectId) -> bool {
        if loose_path(&self.git_dir, id).exists() {
            return true;
        }
        self.packs.iter().any(|p| p.contains(id))
    }

    /// Resolve a 4-to-40-character hex prefix to a full object id.
    // @spec ODB-RESOLVE-001, ODB-RESOLVE-002, ODB-RESOLVE-003,
    //       ODB-RESOLVE-004, ODB-RESOLVE-005, ODB-RESOLVE-006
    pub fn resolve_id(&self, prefix: &str) -> Result<ObjectId, OdbError> {
        if !(4..=40).contains(&prefix.len()) || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(OdbError::InvalidPrefix(prefix.to_owned()));
        }
        let prefix_lc: String = prefix.chars().map(|c| c.to_ascii_lowercase()).collect();

        // Full 40-char prefix: parse directly; never AmbiguousId.
        if prefix_lc.len() == 40 {
            let id = ObjectId::from_hex(&prefix_lc)
                .map_err(|_| OdbError::InvalidPrefix(prefix.to_owned()))?;
            return if self.contains(&id) {
                Ok(id)
            } else {
                Err(OdbError::ObjectNotFound(id))
            };
        }

        let mut candidates: Vec<ObjectId> = Vec::new();
        let dir_part = &prefix_lc[..2];
        let file_prefix = &prefix_lc[2..];
        let sub_dir = self.git_dir.join("objects").join(dir_part);
        if sub_dir.is_dir() {
            for entry in fs::read_dir(&sub_dir)? {
                let entry = entry?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if name.len() == 38 && name.starts_with(file_prefix) {
                    let hex = format!("{dir_part}{name}");
                    if let Ok(id) = ObjectId::from_hex(&hex) {
                        candidates.push(id);
                    }
                }
            }
        }

        // Scan packs for matching prefixes.
        for pack in &self.packs {
            for pack_id in pack.iter_ids() {
                if pack_id.to_hex().starts_with(prefix_lc.as_str()) {
                    candidates.push(*pack_id);
                }
            }
        }

        // ODB-RESOLVE-006: dedupe across loose + pack storage.
        candidates.sort_unstable_by_key(|id| *id.as_bytes());
        candidates.dedup();

        match candidates.len() {
            0 => Err(OdbError::ObjectNotFound(ObjectId::ZERO)),
            1 => Ok(candidates[0]),
            _ => Err(OdbError::AmbiguousId {
                prefix: prefix.to_owned(),
                candidates,
            }),
        }
    }

    /// Import a pack file received over the wire into this repository.
    ///
    /// Note: the imported pack is moved to its canonical path under
    /// `.git/objects/pack/` and indexed, but the current `Repository`
    /// handle does not auto-rescan. A subsequent `Repository::open`
    /// observes the newly-imported pack.
    // @spec ODB-IMPORT-001, ODB-IMPORT-002, ODB-IMPORT-003,
    //       ODB-IMPORT-004, ODB-IMPORT-005
    pub fn import_pack(&self, pack_path: impl AsRef<Path>) -> Result<(), OdbError> {
        let pack_path = pack_path.as_ref();

        // ODB-IMPORT-001 / 002: validate the pack and build the idx
        // before moving anything. Both operations are pure file reads
        // until build_index writes the idx alongside the pack.
        let pack_sha = crate::pack::build_index(pack_path)?;
        let src_idx = pack_path.with_extension("idx");

        // ODB-IMPORT-003: canonical pack-pair names under
        // .git/objects/pack/. Build the destination dir if absent so
        // first-import-into-a-fresh-repo works.
        let dest_dir = self.git_dir.join("objects").join("pack");
        fs::create_dir_all(&dest_dir)?;
        let dest_pack = dest_dir.join(format!("pack-{}.pack", pack_sha.to_hex()));
        let dest_idx = dest_dir.join(format!("pack-{}.idx", pack_sha.to_hex()));

        // ODB-IMPORT-005: move atomically. If either rename fails, roll
        // back the one that succeeded so we never leave a half-imported
        // pack pair.
        if let Err(e) = fs::rename(pack_path, &dest_pack) {
            let _ = fs::remove_file(&src_idx);
            return Err(OdbError::Io(e));
        }
        if let Err(e) = fs::rename(&src_idx, &dest_idx) {
            let _ = fs::rename(&dest_pack, pack_path);
            let _ = fs::remove_file(&src_idx);
            return Err(OdbError::Io(e));
        }

        Ok(())
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub fn work_dir(&self) -> Option<&Path> {
        self.work_dir.as_deref()
    }

    /// Load and validate a loose object: file → inflate → undersize check
    /// → hash verify. Returns `Ok(None)` if the loose file is absent so
    /// callers can decide whether to fall back to pack storage.
    fn read_loose_validated(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, OdbError> {
        let path = loose_path(&self.git_dir, id);
        let raw = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(OdbError::Io(e)),
        };

        let mut inflated = Vec::new();
        if flate2::read::ZlibDecoder::new(&raw[..])
            .read_to_end(&mut inflated)
            .is_err()
        {
            return Err(OdbError::CorruptObject {
                id: *id,
                reason: CorruptReason::Inflate,
            });
        }

        let (_header_end, declared_total) = parse_loose_frame_extents(&inflated)?;
        if inflated.len() < declared_total {
            return Err(OdbError::CorruptObject {
                id: *id,
                reason: CorruptReason::UndersizedPayload,
            });
        }

        let computed = sha1_of(&inflated);
        if computed != *id {
            return Err(OdbError::HashMismatch {
                expected: *id,
                computed,
            });
        }

        Ok(Some(inflated))
    }
}

/// Compute the canonical loose-object path for an id within a git dir.
/// Exposed at module scope so tests can plant fixtures at predictable paths.
// @spec ODB-LOOSE-001
pub(crate) fn loose_path(git_dir: &Path, id: &ObjectId) -> PathBuf {
    let hex = id.to_hex();
    git_dir.join("objects").join(&hex[..2]).join(&hex[2..])
}

fn is_valid_git_dir(p: &Path) -> bool {
    p.join("HEAD").is_file() && p.join("objects").is_dir() && p.join("refs").is_dir()
}

/// Discover and open every pack under `git_dir/objects/pack/`, ordered
/// newest mtime first (ODB-PACK-006). Missing `.idx` files are built
/// lazily before opening (ODB-PACK-003).
// @spec ODB-PACK-003, ODB-PACK-006
fn discover_packs(git_dir: &Path) -> Result<Vec<Pack>, OdbError> {
    let pack_dir = git_dir.join("objects").join("pack");
    if !pack_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut pack_files: Vec<(PathBuf, std::time::SystemTime, std::ffi::OsString)> = Vec::new();
    for entry in fs::read_dir(&pack_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("pack") {
            continue;
        }
        let idx_path = path.with_extension("idx");
        if !idx_path.exists() {
            crate::pack::build_index(&path)?;
        }
        let mtime = entry
            .metadata()?
            .modified()
            .unwrap_or(std::time::UNIX_EPOCH);
        pack_files.push((path, mtime, entry.file_name()));
    }

    // Newest mtime first; ties broken by ascending filename.
    pack_files.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));

    let mut packs = Vec::with_capacity(pack_files.len());
    for (path, _, _) in pack_files {
        packs.push(Pack::open(&path)?);
    }
    Ok(packs)
}

#[cfg(unix)]
fn device_id(p: &Path) -> Result<u64, OdbError> {
    use std::os::unix::fs::MetadataExt;
    Ok(p.metadata()?.dev())
}

#[cfg(not(unix))]
fn device_id(_p: &Path) -> Result<u64, OdbError> {
    // Non-Unix is out of scope per HLD; return a fixed value so the
    // boundary check is a no-op (still safe — the walk eventually hits
    // the filesystem root and stops there).
    Ok(0)
}

fn tmp_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("tmp-{}-{}", std::process::id(), n)
}

fn sha1_of(bytes: &[u8]) -> ObjectId {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(bytes);
    let mut out = [0u8; 20];
    out.copy_from_slice(&h.finalize());
    ObjectId::from_bytes(out)
}

/// Parse the loose-frame header bytes to extract `(header_end, total_size)`.
/// `header_end` is the byte index just past the NUL separator; `total_size`
/// is `header_end + declared_payload_size`.
fn parse_loose_frame_extents(inflated: &[u8]) -> Result<(usize, usize), OdbError> {
    let space = inflated
        .iter()
        .position(|&b| b == b' ')
        .ok_or(OdbError::InvalidObject(ParseError::InvalidFrame))?;
    let nul_rel = inflated[space + 1..]
        .iter()
        .position(|&b| b == 0)
        .ok_or(OdbError::InvalidObject(ParseError::InvalidFrame))?;
    let nul = space + 1 + nul_rel;
    let size_bytes = &inflated[space + 1..nul];
    let size_str = std::str::from_utf8(size_bytes)
        .map_err(|_| OdbError::InvalidObject(ParseError::InvalidFrame))?;
    let size: usize = size_str
        .parse()
        .map_err(|_| OdbError::InvalidObject(ParseError::InvalidFrame))?;
    Ok((nul + 1, nul + 1 + size))
}

/// Parse the kind token and return `(kind, payload_start)` for an
/// already-validated loose-frame buffer.
fn parse_loose_kind_and_payload_start(inflated: &[u8]) -> Result<(ObjectKind, usize), OdbError> {
    let space = inflated
        .iter()
        .position(|&b| b == b' ')
        .expect("validated frame contains space");
    let kind = match &inflated[..space] {
        b"blob" => ObjectKind::Blob,
        b"tree" => ObjectKind::Tree,
        b"commit" => ObjectKind::Commit,
        b"tag" => ObjectKind::Tag,
        other => {
            return Err(OdbError::InvalidObject(ParseError::UnknownKind(
                other.to_vec(),
            )))
        }
    };
    let nul = inflated[space + 1..]
        .iter()
        .position(|&b| b == 0)
        .expect("validated frame contains NUL")
        + space
        + 1;
    Ok((kind, nul + 1))
}

fn verify_existing_loose(path: &Path, expected: &ObjectId) -> Result<ObjectId, OdbError> {
    let raw = fs::read(path)?;
    let mut inflated = Vec::new();
    flate2::read::ZlibDecoder::new(&raw[..])
        .read_to_end(&mut inflated)
        .map_err(|_| OdbError::CorruptObject {
            id: *expected,
            reason: CorruptReason::Inflate,
        })?;
    let computed = sha1_of(&inflated);
    if computed == *expected {
        Ok(*expected)
    } else {
        Err(OdbError::HashMismatch {
            expected: *expected,
            computed,
        })
    }
}

/// Errors returned by the object database.
#[derive(Debug, Error)]
pub enum OdbError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not a git repository: {0:?}")]
    NotARepository(PathBuf),

    #[error("object not found: {0}")]
    ObjectNotFound(ObjectId),

    #[error("hash mismatch: expected {expected}, computed {computed}")]
    HashMismatch {
        expected: ObjectId,
        computed: ObjectId,
    },

    #[error("ambiguous prefix {prefix:?} (matches {} objects)", candidates.len())]
    AmbiguousId {
        prefix: String,
        candidates: Vec<ObjectId>,
    },

    #[error("invalid object: {0}")]
    InvalidObject(#[from] ParseError),

    #[error("invalid pack: {0}")]
    InvalidPack(#[from] PackError),

    #[error("invalid prefix: {0:?}")]
    InvalidPrefix(String),

    #[error("corrupt loose object {id}: {reason:?}")]
    CorruptObject { id: ObjectId, reason: CorruptReason },

    #[error("refs error: {0}")]
    Refs(#[from] crate::refs::RefError),
}

/// The structural failure modes detectable by loose-storage reads
/// independent of the object's parse logic.
#[derive(Debug, PartialEq, Eq)]
pub enum CorruptReason {
    /// zlib inflate failed — bytes on disk are not valid deflate.
    Inflate,
    /// Inflated bytes are shorter than the loose frame's declared size.
    UndersizedPayload,
    /// A specific structural failure not captured by the variants above.
    Other(&'static str),
}
