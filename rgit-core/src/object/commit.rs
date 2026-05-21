use super::{ObjectId, ParseError};

/// A Git commit object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Signature,
    pub committer: Signature,
    /// Headers other than `tree`, `parent`, `author`, `committer`,
    /// preserved verbatim in encounter order. Each occurrence is a
    /// separate entry — repeated headers are not collapsed.
    pub extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// Commit message, byte-for-byte after the blank header separator.
    pub message: Vec<u8>,
}

/// A signature line — author, committer, or tagger.
///
/// Holds both a best-effort structured interpretation and the raw bytes,
/// so the line can be re-emitted byte-identical on serialize even when
/// the structured fields are partial or absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// The raw bytes of the signature line value — the bytes after
    /// `author `, `committer `, or `tagger `, up to but not including
    /// the terminating LF.
    pub raw: Vec<u8>,
    /// Name, best-effort. `None` only in the degenerate single-token
    /// case (which yields `ParseError::InvalidSignature` during parsing).
    pub name: Option<Vec<u8>>,
    /// Email between `<` and `>`. `None` when the brackets are absent.
    pub email: Option<Vec<u8>>,
    /// Unix timestamp. `None` when absent or non-numeric.
    pub timestamp: Option<i64>,
    /// Timezone offset bytes (e.g. `+0000`, `-0500`). `None` when absent.
    pub timezone: Option<Vec<u8>>,
}

// @spec OBJ-COMMIT-001, OBJ-COMMIT-002, OBJ-COMMIT-003, OBJ-COMMIT-004,
//       OBJ-COMMIT-005, OBJ-COMMIT-006, OBJ-COMMIT-007, OBJ-COMMIT-008,
//       OBJ-COMMIT-009, OBJ-COMMIT-010, OBJ-COMMIT-011
pub(super) fn parse_payload(payload: &[u8]) -> Result<Commit, ParseError> {
    let (headers_end, message_start) =
        find_blank_line(payload).ok_or(ParseError::MissingHeader("blank line"))?;
    let header_bytes = &payload[..headers_end];
    let message = payload[message_start..].to_vec();

    let mut tree: Option<ObjectId> = None;
    let mut parents: Vec<ObjectId> = Vec::new();
    let mut author: Option<Signature> = None;
    let mut committer: Option<Signature> = None;
    let mut extra_headers: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

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
            b"tree" => {
                let id = parse_object_id(value)?;
                if tree.is_none() {
                    tree = Some(id);
                } else {
                    extra_headers.push((key.to_vec(), value.to_vec()));
                }
            }
            b"parent" => {
                parents.push(parse_object_id(value)?);
            }
            b"author" => {
                let sig = parse_signature(value)?;
                if author.is_none() {
                    author = Some(sig);
                } else {
                    extra_headers.push((key.to_vec(), value.to_vec()));
                }
            }
            b"committer" => {
                let sig = parse_signature(value)?;
                if committer.is_none() {
                    committer = Some(sig);
                } else {
                    extra_headers.push((key.to_vec(), value.to_vec()));
                }
            }
            _ => {
                extra_headers.push((key.to_vec(), value.to_vec()));
            }
        }

        cursor = logical_end + 1; // skip the terminating LF
    }

    Ok(Commit {
        tree: tree.ok_or(ParseError::MissingHeader("tree"))?,
        parents,
        author: author.ok_or(ParseError::MissingHeader("author"))?,
        committer: committer.ok_or(ParseError::MissingHeader("committer"))?,
        extra_headers,
        message,
    })
}

// @spec OBJ-COMMIT-015
pub(super) fn serialize_payload(commit: &Commit) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"tree ");
    out.extend_from_slice(commit.tree.to_hex().as_bytes());
    out.push(b'\n');
    for parent in &commit.parents {
        out.extend_from_slice(b"parent ");
        out.extend_from_slice(parent.to_hex().as_bytes());
        out.push(b'\n');
    }
    out.extend_from_slice(b"author ");
    out.extend_from_slice(&commit.author.raw);
    out.push(b'\n');
    out.extend_from_slice(b"committer ");
    out.extend_from_slice(&commit.committer.raw);
    out.push(b'\n');
    for (k, v) in &commit.extra_headers {
        out.extend_from_slice(k);
        out.push(b' ');
        out.extend_from_slice(v);
        out.push(b'\n');
    }
    out.push(b'\n');
    out.extend_from_slice(&commit.message);
    out
}

/// Locate the blank line (`\n\n`) separating headers from message.
/// Returns `(headers_end, message_start)` where `headers_end` is one past
/// the LF terminating the last header (i.e., the start of the blank-line
/// LF) and `message_start` is the first byte of the message.
pub(super) fn find_blank_line(payload: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < payload.len() {
        if payload[i] == b'\n' && payload[i + 1] == b'\n' {
            return Some((i + 1, i + 2));
        }
        i += 1;
    }
    None
}

/// Given a cursor at the start of a logical header inside `header_bytes`,
/// return the index of the LF that terminates the entire logical header
/// (after any continuation lines that begin with a single space).
pub(super) fn find_logical_header_end(header_bytes: &[u8], start: usize) -> usize {
    let len = header_bytes.len();
    let mut line_end = header_bytes[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(len, |p| start + p);

    // Extend across continuation lines (those beginning with a single space).
    while line_end + 1 < len && header_bytes[line_end + 1] == b' ' {
        let next = &header_bytes[line_end + 1..];
        let next_lf = next
            .iter()
            .position(|&b| b == b'\n')
            .map_or(len, |p| line_end + 1 + p);
        line_end = next_lf;
    }
    line_end
}

pub(super) fn parse_object_id(value: &[u8]) -> Result<ObjectId, ParseError> {
    let s = std::str::from_utf8(value).map_err(|_| ParseError::InvalidHex)?;
    ObjectId::from_hex(s)
}

/// Best-effort split of a signature line into name / email / timestamp /
/// timezone, while preserving the original bytes in `raw` so the line can
/// be re-emitted byte-identical.
// @spec OBJ-COMMIT-012, OBJ-COMMIT-013, OBJ-COMMIT-014, OBJ-COMMIT-016
pub(super) fn parse_signature(value: &[u8]) -> Result<Signature, ParseError> {
    // OBJ-COMMIT-016: a signature with no whitespace at all is unparseable.
    if !value.contains(&b' ') {
        return Err(ParseError::InvalidSignature);
    }

    let raw = value.to_vec();

    // Try to extract the email between '<' and the first subsequent '>'.
    let email_brackets = value.iter().position(|&b| b == b'<').and_then(|lt| {
        value[lt + 1..]
            .iter()
            .position(|&b| b == b'>')
            .map(|rel| (lt, lt + 1 + rel))
    });

    let (name, email, after_email) = match email_brackets {
        Some((lt, gt)) => {
            let name_bytes = trim_trailing_space(&value[..lt]);
            let name = if name_bytes.is_empty() {
                None
            } else {
                Some(name_bytes.to_vec())
            };
            let email = Some(value[lt + 1..gt].to_vec());
            (name, email, &value[gt + 1..])
        }
        None => {
            // No email. Split the value into name + (timestamp[+tz]) tail
            // by looking for a trailing " <digits> <[+-]digits>" pattern,
            // or a trailing " <digits>".
            let (name_bytes, tail) = split_unbracketed(value);
            let name = if name_bytes.is_empty() {
                None
            } else {
                Some(name_bytes.to_vec())
            };
            (name, None, tail)
        }
    };

    let (timestamp, timezone) = parse_timestamp_and_tz(after_email);

    Ok(Signature {
        raw,
        name,
        email,
        timestamp,
        timezone,
    })
}

fn trim_trailing_space(bytes: &[u8]) -> &[u8] {
    let end = bytes.iter().rposition(|&b| b != b' ').map_or(0, |i| i + 1);
    &bytes[..end]
}

fn trim_leading_space(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&b| b != b' ').unwrap_or(bytes.len());
    &bytes[start..]
}

fn looks_like_timestamp(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let start = usize::from(bytes[0] == b'-');
    if bytes.len() <= start {
        return false;
    }
    bytes[start..].iter().all(u8::is_ascii_digit)
}

fn looks_like_timezone(bytes: &[u8]) -> bool {
    if bytes.is_empty() || (bytes[0] != b'+' && bytes[0] != b'-') {
        return false;
    }
    bytes.len() > 1 && bytes[1..].iter().all(u8::is_ascii_digit)
}

fn split_unbracketed(value: &[u8]) -> (&[u8], &[u8]) {
    // Walk back: if the tail looks like " ts tz" or " ts", peel it off
    // and call the remainder the name.
    if let Some(last_sp) = value.iter().rposition(|&b| b == b' ') {
        let after_last = &value[last_sp + 1..];
        let before_last = &value[..last_sp];
        if looks_like_timezone(after_last) {
            if let Some(prev_sp) = before_last.iter().rposition(|&b| b == b' ') {
                let between = &value[prev_sp + 1..last_sp];
                if looks_like_timestamp(between) {
                    return (&value[..prev_sp], &value[prev_sp + 1..]);
                }
            }
        }
        if looks_like_timestamp(after_last) {
            return (&value[..last_sp], &value[last_sp + 1..]);
        }
    }
    (value, b"")
}

fn parse_timestamp_and_tz(tail: &[u8]) -> (Option<i64>, Option<Vec<u8>>) {
    let tail = trim_leading_space(tail);
    if tail.is_empty() {
        return (None, None);
    }
    if let Some(sp) = tail.iter().position(|&b| b == b' ') {
        let ts_bytes = &tail[..sp];
        let tz_bytes = trim_leading_space(&tail[sp..]);
        let ts = std::str::from_utf8(ts_bytes)
            .ok()
            .and_then(|s| s.parse::<i64>().ok());
        let tz = if tz_bytes.is_empty() {
            None
        } else {
            Some(tz_bytes.to_vec())
        };
        (ts, tz)
    } else {
        let ts = std::str::from_utf8(tail)
            .ok()
            .and_then(|s| s.parse::<i64>().ok());
        (ts, None)
    }
}
