/// A Git blob — an opaque byte sequence with no internal structure.
// @spec OBJ-BLOB-001, OBJ-BLOB-002
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Blob {
    pub data: Vec<u8>,
}

impl Blob {
    /// Create a blob wrapping the given bytes.
    pub fn new(data: impl Into<Vec<u8>>) -> Self {
        Blob { data: data.into() }
    }
}
