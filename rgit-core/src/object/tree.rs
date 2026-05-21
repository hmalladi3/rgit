use super::{ObjectId, ParseError};

/// A Git tree — an ordered sequence of named entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    /// Entries in encounter order. Serialization sorts and deduplicates
    /// per Git's canonical rules; callers reading a tree from disk see
    /// the entries as they appeared, including any malformed ordering
    /// or duplicates.
    pub entries: Vec<TreeEntry>,
}

/// A single entry within a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: EntryMode,
    /// Entry name. Bytes, not UTF-8 — Linux filenames are byte strings
    /// and Git stores them verbatim.
    pub name: Vec<u8>,
    pub id: ObjectId,
}

/// The set of tree-entry modes Git recognizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryMode {
    /// Regular file (`100644`).
    Regular,
    /// Executable file (`100755`).
    Executable,
    /// Subtree (`40000`).
    Tree,
    /// Symbolic link (`120000`).
    Symlink,
    /// Gitlink — submodule commit pointer (`160000`).
    Gitlink,
}

impl EntryMode {
    /// The canonical octal ASCII representation, no leading zero.
    pub fn as_octal(self) -> &'static [u8] {
        match self {
            EntryMode::Regular => b"100644",
            EntryMode::Executable => b"100755",
            EntryMode::Tree => b"40000",
            EntryMode::Symlink => b"120000",
            EntryMode::Gitlink => b"160000",
        }
    }
}

fn parse_mode(bytes: &[u8]) -> Result<EntryMode, ParseError> {
    match bytes {
        b"100644" => Ok(EntryMode::Regular),
        b"100755" => Ok(EntryMode::Executable),
        // OBJ-TREE-003: accept legacy `040000` and normalize to `40000`
        // on serialize.
        b"40000" | b"040000" => Ok(EntryMode::Tree),
        b"120000" => Ok(EntryMode::Symlink),
        b"160000" => Ok(EntryMode::Gitlink),
        _ => Err(ParseError::InvalidMode(bytes.to_vec())),
    }
}

// @spec OBJ-TREE-001, OBJ-TREE-002, OBJ-TREE-003, OBJ-TREE-004, OBJ-TREE-005,
//       OBJ-TREE-006, OBJ-TREE-007, OBJ-TREE-008, OBJ-TREE-009, OBJ-PARSE-004
pub(super) fn parse_payload(payload: &[u8]) -> Result<Tree, ParseError> {
    let mut entries = Vec::new();
    let mut cursor = 0;
    while cursor < payload.len() {
        let rest = &payload[cursor..];

        // Mode runs up to the first space.
        let space_offset = rest
            .iter()
            .position(|&b| b == b' ')
            .ok_or(ParseError::InvalidTreeEntry)?;
        let mode = parse_mode(&rest[..space_offset])?;

        // Name runs from after the space up to the next NUL.
        let after_space = &rest[space_offset + 1..];
        let nul_offset = after_space
            .iter()
            .position(|&b| b == 0)
            .ok_or(ParseError::Truncated)?;
        let name = &after_space[..nul_offset];
        if name.is_empty() {
            return Err(ParseError::InvalidTreeEntry);
        }
        if name.contains(&b'/') {
            return Err(ParseError::InvalidTreeEntry);
        }

        // 20 raw SHA bytes follow.
        let sha_start = space_offset + 1 + nul_offset + 1;
        if sha_start + 20 > rest.len() {
            return Err(ParseError::Truncated);
        }
        let mut sha = [0u8; 20];
        sha.copy_from_slice(&rest[sha_start..sha_start + 20]);

        entries.push(TreeEntry {
            mode,
            name: name.to_vec(),
            id: ObjectId::from_bytes(sha),
        });
        cursor += sha_start + 20;
    }
    Ok(Tree { entries })
}

// @spec OBJ-TREE-003, OBJ-TREE-010, OBJ-TREE-011, OBJ-TREE-012
pub(super) fn serialize_payload(tree: &Tree) -> Vec<u8> {
    // Dedupe by byte-equal name, mode-independent, first-wins (OBJ-TREE-011).
    // We walk the original order and keep an entry only the first time its
    // name appears.
    let mut kept: Vec<&TreeEntry> = Vec::with_capacity(tree.entries.len());
    for entry in &tree.entries {
        if !kept.iter().any(|e| e.name == entry.name) {
            kept.push(entry);
        }
    }

    // Sort by Git's mode-aware key: `name + ('/' if tree else '')`
    // (OBJ-TREE-010). Cache the keys to avoid recomputing.
    kept.sort_by_cached_key(|e| {
        let mut key = e.name.clone();
        if e.mode == EntryMode::Tree {
            key.push(b'/');
        }
        key
    });

    let mut out = Vec::new();
    for entry in kept {
        out.extend_from_slice(entry.mode.as_octal());
        out.push(b' ');
        out.extend_from_slice(&entry.name);
        out.push(0);
        out.extend_from_slice(entry.id.as_bytes());
    }
    out
}
