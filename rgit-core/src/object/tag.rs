use super::commit::{find_blank_line, find_logical_header_end, parse_object_id, parse_signature};
use super::{ObjectId, ObjectKind, ParseError, Signature};

/// A Git annotated tag.
///
/// Lightweight tags — refs pointing directly at commits with no tag
/// object — are handled by the refs layer. This type represents only
/// annotated tags, which have their own object kind on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    /// The object this tag points to.
    pub object: ObjectId,
    /// The kind of the pointed-to object. Strict lowercase on the wire.
    pub object_kind: ObjectKind,
    /// The tag name (the `tag` header value). Bytes, not UTF-8.
    pub name: Vec<u8>,
    pub tagger: Signature,
    /// Tag message. Inline PGP signatures — the older signing scheme —
    /// are stored as part of the message bytes without extraction.
    pub message: Vec<u8>,
}

fn parse_object_kind(value: &[u8]) -> Result<ObjectKind, ParseError> {
    match value {
        b"blob" => Ok(ObjectKind::Blob),
        b"tree" => Ok(ObjectKind::Tree),
        b"commit" => Ok(ObjectKind::Commit),
        b"tag" => Ok(ObjectKind::Tag),
        _ => Err(ParseError::UnknownKind(value.to_vec())),
    }
}

// @spec OBJ-TAG-001, OBJ-TAG-002, OBJ-TAG-003, OBJ-TAG-004, OBJ-TAG-005, OBJ-TAG-006
pub(super) fn parse_payload(payload: &[u8]) -> Result<Tag, ParseError> {
    let (headers_end, message_start) =
        find_blank_line(payload).ok_or(ParseError::MissingHeader("blank line"))?;
    let header_bytes = &payload[..headers_end];
    let message = payload[message_start..].to_vec();

    let mut object: Option<ObjectId> = None;
    let mut object_kind: Option<ObjectKind> = None;
    let mut name: Option<Vec<u8>> = None;
    let mut tagger: Option<Signature> = None;

    let mut cursor = 0;
    while cursor < header_bytes.len() {
        let logical_end = find_logical_header_end(header_bytes, cursor);
        let logical = &header_bytes[cursor..logical_end];

        let space_idx = logical
            .iter()
            .position(|&b| b == b' ')
            .ok_or(ParseError::MissingHeader("header value separator"))?;
        let key = &logical[..space_idx];
        let value = &logical[space_idx + 1..];

        match key {
            b"object" if object.is_none() => {
                object = Some(parse_object_id(value)?);
            }
            b"type" if object_kind.is_none() => {
                object_kind = Some(parse_object_kind(value)?);
            }
            b"tag" if name.is_none() => {
                name = Some(value.to_vec());
            }
            b"tagger" if tagger.is_none() => {
                tagger = Some(parse_signature(value)?);
            }
            _ => {
                // Tag's standard headers don't repeat in practice. Extra
                // or repeated headers are uncommon enough that we ignore
                // them in v1 rather than pretending a richer model.
            }
        }

        cursor = logical_end + 1;
    }

    Ok(Tag {
        object: object.ok_or(ParseError::MissingHeader("object"))?,
        object_kind: object_kind.ok_or(ParseError::MissingHeader("type"))?,
        name: name.ok_or(ParseError::MissingHeader("tag"))?,
        tagger: tagger.ok_or(ParseError::MissingHeader("tagger"))?,
        message,
    })
}

pub(super) fn serialize_payload(tag: &Tag) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"object ");
    out.extend_from_slice(tag.object.to_hex().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(b"type ");
    out.extend_from_slice(tag.object_kind.as_str().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(b"tag ");
    out.extend_from_slice(&tag.name);
    out.push(b'\n');
    out.extend_from_slice(b"tagger ");
    out.extend_from_slice(&tag.tagger.raw);
    out.push(b'\n');
    out.push(b'\n');
    out.extend_from_slice(&tag.message);
    out
}
