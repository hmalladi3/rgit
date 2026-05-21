//! Git object model — blob, tree, commit, tag.
//!
//! Owns the in-memory representation, parsing, and serialization of Git's
//! four object kinds. The serialization format matches upstream Git
//! byte-for-byte: an object's identity is SHA-1 of its loose-framed bytes,
//! and that hash is the contract this module preserves.
//!
//! Hash verification — confirming that a byte sequence hashes to a known
//! [`ObjectId`] — belongs to the storage layer, not this module.

mod blob;
mod commit;
mod tag;
mod tree;

#[cfg(test)]
mod tests;

pub use blob::Blob;
pub use commit::{Commit, Signature};
pub use tag::Tag;
pub use tree::{EntryMode, Tree, TreeEntry};

use thiserror::Error;

/// A 20-byte SHA-1 object identifier.
///
/// Opaque: the underlying byte layout is not part of the public type
/// signature, so a future migration to SHA-256 can land without spreading
/// across call sites.
// @spec OBJ-ID-001, OBJ-ID-008
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId([u8; 20]);

// @spec OBJ-ID-002, OBJ-ID-003, OBJ-ID-004, OBJ-ID-005, OBJ-ID-006, OBJ-ID-007
impl ObjectId {
    /// The all-zero object id, used by the wire protocol as the null ref.
    pub const ZERO: Self = ObjectId([0u8; 20]);

    /// Parse a 40-character hex string. Mixed case is accepted on input;
    /// [`Self::to_hex`] always emits lowercase.
    pub fn from_hex(s: &str) -> Result<Self, ParseError> {
        if s.len() != 40 {
            return Err(ParseError::InvalidHex);
        }
        let bytes = s.as_bytes();
        let mut out = [0u8; 20];
        for (i, slot) in out.iter_mut().enumerate() {
            let hi = decode_hex_nibble(bytes[2 * i])?;
            let lo = decode_hex_nibble(bytes[2 * i + 1])?;
            *slot = (hi << 4) | lo;
        }
        Ok(ObjectId(out))
    }

    /// Render this id as a 40-character lowercase hex string.
    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(40);
        for &b in &self.0 {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0x0f) as usize] as char);
        }
        s
    }

    /// Borrow the underlying 20-byte digest.
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Construct an id directly from its 20-byte digest. Used by packfile
    /// readers and other call sites that already have the raw bytes.
    pub fn from_bytes(bytes: [u8; 20]) -> Self {
        ObjectId(bytes)
    }

    /// True only for the all-zero id ([`Self::ZERO`]).
    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 20]
    }

    /// Compute the id of a frameless payload of the given kind.
    ///
    /// Hashes the loose-framed bytes (`kind ' ' size '\0' payload`)
    /// internally so the result matches upstream `git hash-object`.
    pub fn compute(kind: ObjectKind, payload: &[u8]) -> Self {
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(kind.as_str().as_bytes());
        hasher.update(b" ");
        hasher.update(payload.len().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(payload);
        let digest = hasher.finalize();
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(&digest);
        ObjectId(bytes)
    }
}

fn decode_hex_nibble(b: u8) -> Result<u8, ParseError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(ParseError::InvalidHex),
    }
}

impl std::fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ObjectId({})", self.to_hex())
    }
}

impl std::fmt::Display for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// The four kinds of Git object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectKind {
    /// The lowercase ASCII token used in loose-object frames and tag
    /// `type` headers.
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Blob => "blob",
            ObjectKind::Tree => "tree",
            ObjectKind::Commit => "commit",
            ObjectKind::Tag => "tag",
        }
    }
}

/// A Git object — one of blob, tree, commit, or tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Object {
    Blob(Blob),
    Tree(Tree),
    Commit(Commit),
    Tag(Tag),
}

impl Object {
    /// Parse a loose-framed object: `kind ' ' decimal_size '\0' payload`.
    // @spec OBJ-FRAME-001, OBJ-FRAME-002, OBJ-FRAME-003, OBJ-FRAME-004,
    //       OBJ-FRAME-005, OBJ-PARSE-002, OBJ-PARSE-003
    pub fn parse_loose(bytes: &[u8]) -> Result<Self, ParseError> {
        let space_idx = bytes
            .iter()
            .position(|&b| b == b' ')
            .ok_or(ParseError::InvalidFrame)?;
        let kind = parse_kind_token(&bytes[..space_idx])?;

        let after_space = &bytes[space_idx + 1..];
        let nul_offset = after_space
            .iter()
            .position(|&b| b == 0)
            .ok_or(ParseError::InvalidFrame)?;
        let size_bytes = &after_space[..nul_offset];
        let size = parse_frame_size(size_bytes)?;

        let payload = &after_space[nul_offset + 1..];
        if payload.len() != size {
            return Err(ParseError::InvalidSize);
        }

        Self::parse_payload(kind, payload)
    }

    /// Parse a frameless payload of known kind. Used by the pack reader,
    /// where the frame is reconstructed on the side for hashing.
    // @spec OBJ-PARSE-001, OBJ-PARSE-002, OBJ-PARSE-003
    pub fn parse_payload(kind: ObjectKind, payload: &[u8]) -> Result<Self, ParseError> {
        match kind {
            ObjectKind::Blob => Ok(Object::Blob(Blob {
                data: payload.to_vec(),
            })),
            ObjectKind::Tree => Ok(Object::Tree(tree::parse_payload(payload)?)),
            ObjectKind::Commit => Ok(Object::Commit(commit::parse_payload(payload)?)),
            ObjectKind::Tag => Ok(Object::Tag(tag::parse_payload(payload)?)),
        }
    }

    /// Serialize to loose-framed bytes. The result is what gets
    /// zlib-compressed for on-disk storage and what [`Self::id`] hashes.
    // @spec OBJ-SERIALIZE-001, OBJ-SERIALIZE-002, OBJ-SERIALIZE-003
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Object::Blob(b) => {
                write_frame_header(&mut out, ObjectKind::Blob, b.data.len());
                out.extend_from_slice(&b.data);
            }
            Object::Tree(t) => {
                let payload = tree::serialize_payload(t);
                write_frame_header(&mut out, ObjectKind::Tree, payload.len());
                out.extend_from_slice(&payload);
            }
            Object::Commit(c) => {
                let payload = commit::serialize_payload(c);
                write_frame_header(&mut out, ObjectKind::Commit, payload.len());
                out.extend_from_slice(&payload);
            }
            Object::Tag(t) => {
                let payload = tag::serialize_payload(t);
                write_frame_header(&mut out, ObjectKind::Tag, payload.len());
                out.extend_from_slice(&payload);
            }
        }
        out
    }

    /// Compute this object's id by serializing and hashing.
    // @spec OBJ-FRAME-006
    pub fn id(&self) -> ObjectId {
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(self.serialize());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(&digest);
        ObjectId(bytes)
    }

    /// The kind of this object.
    pub fn kind(&self) -> ObjectKind {
        match self {
            Object::Blob(_) => ObjectKind::Blob,
            Object::Tree(_) => ObjectKind::Tree,
            Object::Commit(_) => ObjectKind::Commit,
            Object::Tag(_) => ObjectKind::Tag,
        }
    }
}

fn parse_kind_token(bytes: &[u8]) -> Result<ObjectKind, ParseError> {
    match bytes {
        b"blob" => Ok(ObjectKind::Blob),
        b"tree" => Ok(ObjectKind::Tree),
        b"commit" => Ok(ObjectKind::Commit),
        b"tag" => Ok(ObjectKind::Tag),
        _ => Err(ParseError::UnknownKind(bytes.to_vec())),
    }
}

fn parse_frame_size(bytes: &[u8]) -> Result<usize, ParseError> {
    if bytes.is_empty() {
        return Err(ParseError::InvalidFrame);
    }
    // Reject leading zero (per OBJ-FRAME-003). The single byte "0" is the
    // only valid form for zero.
    if bytes.len() > 1 && bytes[0] == b'0' {
        return Err(ParseError::InvalidFrame);
    }
    let mut n: usize = 0;
    for &b in bytes {
        let d = match b {
            b'0'..=b'9' => (b - b'0') as usize,
            _ => return Err(ParseError::InvalidFrame),
        };
        n = n.checked_mul(10).ok_or(ParseError::InvalidFrame)?;
        n = n.checked_add(d).ok_or(ParseError::InvalidFrame)?;
    }
    Ok(n)
}

fn write_frame_header(out: &mut Vec<u8>, kind: ObjectKind, payload_len: usize) {
    out.extend_from_slice(kind.as_str().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload_len.to_string().as_bytes());
    out.push(0);
}

/// Errors returned by the object parsers.
///
/// Variants distinguish structural failure modes. The storage layer
/// reports hash mismatches separately; this module never verifies hashes
/// at parse time.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// Structurally broken loose-object frame: missing space between kind
    /// and size, missing NUL between size and payload, or a size field
    /// with leading zeros.
    #[error("invalid loose-object frame")]
    InvalidFrame,

    /// The frame's declared size does not equal the actual payload byte
    /// length.
    #[error("declared size does not match payload length")]
    InvalidSize,

    /// The frame's kind token is not one of `blob`, `tree`, `commit`,
    /// `tag` (strict lowercase).
    #[error("unknown object kind: {0:?}")]
    UnknownKind(Vec<u8>),

    /// A tree entry is structurally broken: missing space, missing NUL,
    /// short SHA, empty name, or `/` in the name.
    #[error("invalid tree entry")]
    InvalidTreeEntry,

    /// A tree entry's mode is not one of the recognized octal values.
    #[error("invalid tree-entry mode: {0:?}")]
    InvalidMode(Vec<u8>),

    /// A commit or tag is missing a required header — `tree`, `author`,
    /// `committer`, `object`, `type`, `tag`, `tagger`, or the blank line
    /// separating headers from the message.
    #[error("missing required header: {0}")]
    MissingHeader(&'static str),

    /// A signature line (author / committer / tagger) is so malformed
    /// that no structured extraction is possible — a single token with
    /// no whitespace.
    #[error("invalid signature line")]
    InvalidSignature,

    /// A hex-encoded value is not a valid 40-character hex string.
    #[error("invalid hex string")]
    InvalidHex,

    /// Input ends mid-structure: a header line without a terminating LF,
    /// a tree entry whose SHA byte run is incomplete, etc.
    #[error("truncated input")]
    Truncated,
}
