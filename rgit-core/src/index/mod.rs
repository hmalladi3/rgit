//! Git index — the staging area between the working tree and the next
//! commit. Format v2 only.

#[cfg(test)]
mod tests;

use crate::object::ObjectId;
use crate::odb::Repository;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

const MAGIC: &[u8; 4] = b"DIRC";
const VERSION: u32 = 2;
const HEADER_LEN: usize = 12;
const FIXED_PREFIX_LEN: usize = 62;
const TRAILER_LEN: usize = 20;

/// Time stamp as stored in the index (seconds + nanoseconds since epoch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Time {
    pub secs: u32,
    pub nanos: u32,
}

/// One staged path's metadata + content id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub ctime: Time,
    pub mtime: Time,
    pub dev: u32,
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
    pub id: ObjectId,
    pub assume_valid: bool,
    pub stage: u8,
    pub path: Vec<u8>,
}

/// The on-disk index file as an in-memory structure. Entries are
/// canonically ordered (ascending path, ascending stage). Unknown
/// extension blocks are preserved verbatim across read/write.
#[derive(Debug, Clone, Default)]
pub struct Index {
    entries: Vec<IndexEntry>,
    extensions: Vec<(String, Vec<u8>)>,
}

/// Errors returned by the index module.
#[derive(Debug, Error)]
pub enum IndexError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("not an index file (bad magic or version)")]
    BadHeader,

    #[error("index trailer hash mismatch")]
    BadTrailer,

    #[error("entry count {declared} does not match parsed entries {actual}")]
    EntryCountMismatch { declared: u32, actual: usize },

    #[error("malformed entry")]
    MalformedEntry,

    #[error("path not found in index: {0:?}")]
    PathNotFound(Vec<u8>),
}

// @spec INDEX-API-001, INDEX-API-002, INDEX-API-003, INDEX-API-004,
//       INDEX-ORDER-001, INDEX-ORDER-002, INDEX-ORDER-003
impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    pub fn extensions(&self) -> &[(String, Vec<u8>)] {
        &self.extensions
    }

    pub fn lookup(&self, path: &[u8]) -> Option<&IndexEntry> {
        self.entries.iter().find(|e| e.path == path && e.stage == 0)
    }

    pub fn upsert(&mut self, entry: IndexEntry) {
        if entry.stage == 0 {
            // Stage 0 replaces all stages for this path (merge resolved).
            self.entries.retain(|e| e.path != entry.path);
        } else {
            // Conflict-stage entry: drop any stage 0 for this path, plus
            // any existing entry at the same (path, stage).
            self.entries
                .retain(|e| !(e.path == entry.path && (e.stage == 0 || e.stage == entry.stage)));
        }
        self.entries.push(entry);
        self.entries
            .sort_by(|a, b| a.path.cmp(&b.path).then(a.stage.cmp(&b.stage)));
    }

    pub fn remove(&mut self, path: &[u8]) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| e.path != path);
        before - self.entries.len()
    }

    /// Add an extension block. Intended for round-trip preservation —
    /// callers that didn't read an existing extension shouldn't be
    /// authoring new ones in v1.
    pub fn push_extension(&mut self, sig: String, data: Vec<u8>) {
        self.extensions.push((sig, data));
    }
}

impl Repository {
    /// Read the repository's `.git/index` file. Returns an empty `Index`
    /// if the file does not exist (fresh repo).
    // @spec INDEX-READ-001, INDEX-READ-002, INDEX-READ-003,
    //       INDEX-FMT-001, INDEX-FMT-002, INDEX-FMT-004,
    //       INDEX-ENTRY-001, INDEX-ENTRY-002, INDEX-ENTRY-003,
    //       INDEX-ENTRY-004, INDEX-ENTRY-005,
    //       INDEX-EXT-001
    pub fn read_index(&self) -> Result<Index, IndexError> {
        let path = self.git_dir().join("index");
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Index::new()),
            Err(e) => return Err(IndexError::Io(e)),
        };
        parse_index(&bytes)
    }

    /// Atomically write the index to `.git/index`.
    // @spec INDEX-WRITE-001, INDEX-WRITE-002, INDEX-WRITE-003,
    //       INDEX-FMT-001, INDEX-FMT-003,
    //       INDEX-ENTRY-001, INDEX-ENTRY-006,
    //       INDEX-EXT-002, INDEX-EXT-003
    pub fn write_index(&self, index: &Index) -> Result<(), IndexError> {
        let bytes = serialize_index(index);
        let path = self.git_dir().join("index");
        atomic_write(&path, &bytes)?;
        Ok(())
    }
}

fn parse_index(bytes: &[u8]) -> Result<Index, IndexError> {
    if bytes.len() < HEADER_LEN + TRAILER_LEN {
        return Err(IndexError::BadHeader);
    }
    if &bytes[..4] != MAGIC {
        return Err(IndexError::BadHeader);
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
    if version != VERSION {
        return Err(IndexError::BadHeader);
    }
    let entry_count = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;

    let body_len = bytes.len() - TRAILER_LEN;
    let stored: &[u8; 20] = bytes[body_len..].try_into().unwrap();
    let computed = sha1_bytes(&bytes[..body_len]);
    if stored != &computed {
        return Err(IndexError::BadTrailer);
    }

    let mut entries = Vec::with_capacity(entry_count);
    let mut cursor = HEADER_LEN;
    for _ in 0..entry_count {
        let entry_start = cursor;
        if cursor + FIXED_PREFIX_LEN > body_len {
            return Err(IndexError::MalformedEntry);
        }
        let ctime = Time {
            secs: be32(bytes, cursor),
            nanos: be32(bytes, cursor + 4),
        };
        let mtime = Time {
            secs: be32(bytes, cursor + 8),
            nanos: be32(bytes, cursor + 12),
        };
        let dev = be32(bytes, cursor + 16);
        let ino = be32(bytes, cursor + 20);
        let mode = be32(bytes, cursor + 24);
        let uid = be32(bytes, cursor + 28);
        let gid = be32(bytes, cursor + 32);
        let size = be32(bytes, cursor + 36);
        let mut sha = [0u8; 20];
        sha.copy_from_slice(&bytes[cursor + 40..cursor + 60]);
        let flags = u16::from_be_bytes(bytes[cursor + 60..cursor + 62].try_into().unwrap());
        let assume_valid = (flags & 0x8000) != 0;
        let extended = (flags & 0x4000) != 0;
        if extended {
            return Err(IndexError::MalformedEntry);
        }
        let stage = ((flags >> 12) & 0x3) as u8;
        let name_length = (flags & 0xfff) as usize;

        cursor += FIXED_PREFIX_LEN;
        let path_start = cursor;
        let path_end = if name_length < 0xfff {
            let end = path_start + name_length;
            if end > body_len || bytes[end] != 0 {
                return Err(IndexError::MalformedEntry);
            }
            end
        } else {
            bytes[path_start..body_len]
                .iter()
                .position(|&b| b == 0)
                .map(|p| path_start + p)
                .ok_or(IndexError::MalformedEntry)?
        };
        let path = bytes[path_start..path_end].to_vec();
        cursor = path_end + 1; // skip NUL terminator

        // Pad to 8-byte alignment from entry_start.
        let entry_len = cursor - entry_start;
        let pad = (8 - (entry_len % 8)) % 8;
        cursor += pad;

        entries.push(IndexEntry {
            ctime,
            mtime,
            dev,
            ino,
            mode,
            uid,
            gid,
            size,
            id: ObjectId::from_bytes(sha),
            assume_valid,
            stage,
            path,
        });
    }

    if entries.len() != entry_count {
        return Err(IndexError::EntryCountMismatch {
            declared: entry_count as u32,
            actual: entries.len(),
        });
    }

    let mut extensions = Vec::new();
    while cursor + 8 <= body_len {
        let sig: [u8; 4] = bytes[cursor..cursor + 4].try_into().unwrap();
        let ext_size =
            u32::from_be_bytes(bytes[cursor + 4..cursor + 8].try_into().unwrap()) as usize;
        cursor += 8;
        if cursor + ext_size > body_len {
            return Err(IndexError::MalformedEntry);
        }
        let data = bytes[cursor..cursor + ext_size].to_vec();
        cursor += ext_size;
        let signature = String::from_utf8_lossy(&sig).into_owned();
        extensions.push((signature, data));
    }

    Ok(Index {
        entries,
        extensions,
    })
}

fn serialize_index(index: &Index) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_be_bytes());

    let mut entries: Vec<&IndexEntry> = index.entries.iter().collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.stage.cmp(&b.stage)));
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());

    for entry in entries {
        let entry_start = out.len();
        out.extend_from_slice(&entry.ctime.secs.to_be_bytes());
        out.extend_from_slice(&entry.ctime.nanos.to_be_bytes());
        out.extend_from_slice(&entry.mtime.secs.to_be_bytes());
        out.extend_from_slice(&entry.mtime.nanos.to_be_bytes());
        out.extend_from_slice(&entry.dev.to_be_bytes());
        out.extend_from_slice(&entry.ino.to_be_bytes());
        out.extend_from_slice(&entry.mode.to_be_bytes());
        out.extend_from_slice(&entry.uid.to_be_bytes());
        out.extend_from_slice(&entry.gid.to_be_bytes());
        out.extend_from_slice(&entry.size.to_be_bytes());
        out.extend_from_slice(entry.id.as_bytes());

        let mut flags: u16 = 0;
        if entry.assume_valid {
            flags |= 0x8000;
        }
        flags |= u16::from(entry.stage & 0x3) << 12;
        let name_len = entry.path.len().min(0xfff);
        flags |= name_len as u16;
        out.extend_from_slice(&flags.to_be_bytes());

        out.extend_from_slice(&entry.path);
        out.push(0);

        let entry_len = out.len() - entry_start;
        let pad = (8 - (entry_len % 8)) % 8;
        out.resize(out.len() + pad, 0);
    }

    for (sig, data) in &index.extensions {
        let sig_bytes = sig.as_bytes();
        if sig_bytes.len() != 4 {
            continue;
        }
        out.extend_from_slice(sig_bytes);
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
    }

    let trailer = sha1_bytes(&out);
    out.extend_from_slice(&trailer);
    out
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn sha1_bytes(bytes: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(bytes);
    let mut out = [0u8; 20];
    out.copy_from_slice(&h.finalize());
    out
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), IndexError> {
    let parent = path
        .parent()
        .ok_or_else(|| IndexError::Io(io::Error::other("index path has no parent")))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(tmp_name());
    {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(IndexError::Io(e));
    }
    if let Ok(d) = fs::File::open(parent) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn tmp_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("tmp-index-{}-{}", std::process::id(), n)
}
