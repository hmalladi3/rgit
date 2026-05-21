//! Smart HTTP wire protocol — push only in v1.
//!
//! Push speaks the Smart HTTP "v0" receive-pack protocol; clone and
//! fetch (Smart HTTP v2) are deferred. HTTPS only, HTTP Basic auth.

#[cfg(test)]
mod tests;

use crate::object::{EntryMode, Object, ObjectId};
use crate::odb::{OdbError, Repository};
use crate::pack::PackWriter;
use std::collections::HashSet;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct TransportCredentials {
    pub username: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    pub name: String,
    pub id: ObjectId,
}

#[derive(Debug, Clone)]
pub struct RefUpdate {
    pub old_id: ObjectId,
    pub new_id: ObjectId,
    pub ref_name: String,
}

#[derive(Debug)]
pub struct PushResult {
    pub unpack_ok: bool,
    pub per_ref: Vec<(String, Result<(), String>)>,
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("http error: {0}")]
    Http(String),

    #[error("pkt-line decode error")]
    PktLine,

    #[error("server reported: {0}")]
    Server(String),

    #[error("pack error: {0}")]
    Pack(#[from] crate::pack::PackError),

    #[error("odb error: {0}")]
    Odb(#[from] OdbError),

    #[error("object error: {0}")]
    Object(#[from] crate::object::ParseError),

    #[error("refs error: {0}")]
    Refs(#[from] crate::refs::RefError),

    #[error("authentication required")]
    AuthRequired,
}

// ---------------------------------------------------------------------
// pkt-line encoding / decoding
// ---------------------------------------------------------------------

/// Encode a non-flush packet: `NNNN<data>` where `NNNN` is the 4-hex
/// length including the prefix.
// @spec TX-PKTLINE-001
pub fn pkt_line_encode(data: &[u8]) -> Vec<u8> {
    let total_len = data.len() + 4;
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(format!("{total_len:04x}").as_bytes());
    out.extend_from_slice(data);
    out
}

/// Append a flush packet to the buffer.
// @spec TX-PKTLINE-002
pub fn pkt_line_flush() -> &'static [u8] {
    b"0000"
}

/// Decode pkt-line packets from `bytes`, returning a list of payloads.
/// Skips flush packets (`0000`), v2 delimiter packets (`0001`), and v2
/// response-end packets (`0002`) — those are stream markers, not data.
// @spec TX-PKTLINE-003, TX-PKTLINE-004
pub fn pkt_line_decode_all(bytes: &[u8]) -> Result<Vec<Vec<u8>>, TransportError> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while cursor + 4 <= bytes.len() {
        let len_str =
            std::str::from_utf8(&bytes[cursor..cursor + 4]).map_err(|_| TransportError::PktLine)?;
        let len = u32::from_str_radix(len_str, 16).map_err(|_| TransportError::PktLine)? as usize;
        cursor += 4;
        // 0000 = flush, 0001 = delim (v2), 0002 = response-end (v2).
        if len == 0 || len == 1 || len == 2 {
            continue;
        }
        if len < 4 || cursor + len - 4 > bytes.len() {
            return Err(TransportError::PktLine);
        }
        let payload = bytes[cursor..cursor + len - 4].to_vec();
        cursor += len - 4;
        out.push(payload);
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// HTTP auth
// ---------------------------------------------------------------------

// @spec TX-AUTH-001
fn auth_header(creds: &TransportCredentials) -> String {
    use base64::Engine;
    let pair = format!("{}:{}", creds.username, creds.token);
    let encoded = base64::engine::general_purpose::STANDARD.encode(pair);
    format!("Basic {encoded}")
}

// ---------------------------------------------------------------------
// ls-refs
// ---------------------------------------------------------------------

/// List the refs advertised by the remote receive-pack service.
// @spec TX-LSREF-001, TX-LSREF-002, TX-LSREF-003, TX-AUTH-001, TX-AUTH-002
pub fn list_remote_refs(
    url: &str,
    creds: Option<&TransportCredentials>,
) -> Result<Vec<RemoteRef>, TransportError> {
    let info_url = format!(
        "{}/info/refs?service=git-receive-pack",
        url.trim_end_matches('/')
    );
    let mut req = ureq::get(&info_url)
        .set("Accept", "application/x-git-receive-pack-advertisement")
        .set("User-Agent", "rgit/0.1.0");
    if let Some(c) = creds {
        req = req.set("Authorization", &auth_header(c));
    }
    let response = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(401, _)) => return Err(TransportError::AuthRequired),
        Err(e) => return Err(TransportError::Http(e.to_string())),
    };
    let mut body = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut body)
        .map_err(TransportError::Io)?;
    parse_ref_advertisement(&body)
}

fn parse_ref_advertisement(body: &[u8]) -> Result<Vec<RemoteRef>, TransportError> {
    let pkts = pkt_line_decode_all(body)?;
    let mut refs = Vec::new();
    let mut saw_service_line = false;
    for pkt in pkts {
        // Strip trailing LF if present.
        let line = if pkt.last() == Some(&b'\n') {
            &pkt[..pkt.len() - 1]
        } else {
            &pkt[..]
        };
        if !saw_service_line {
            // First non-flush packet is "# service=...".
            if line.starts_with(b"# service=") {
                saw_service_line = true;
                continue;
            }
        }
        // Skip "version" line if present (some servers send it).
        if line.starts_with(b"version ") {
            continue;
        }
        // Ref line: "<40-hex> <ref-name>[\0<capabilities>]".
        if line.len() < 41 {
            continue;
        }
        let hex = std::str::from_utf8(&line[..40]).map_err(|_| TransportError::PktLine)?;
        let id = ObjectId::from_hex(hex).map_err(|_| TransportError::PktLine)?;
        if line[40] != b' ' {
            continue;
        }
        let rest = &line[41..];
        let name_end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        let name = String::from_utf8_lossy(&rest[..name_end]).into_owned();
        refs.push(RemoteRef { name, id });
    }
    Ok(refs)
}

// ---------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------

/// Push the given ref updates to a remote.
///
/// Dispatches on URL scheme: HTTPS uses Smart HTTP v0 receive-pack
/// (requires `creds`); SSH (`git@host:path` or `ssh://`) shells out to
/// the system `ssh` client and speaks raw receive-pack over the pipe
/// (no credentials argument needed — SSH handles auth via the user's
/// ssh-agent / ~/.ssh/config).
// @spec TX-PUSH-001, TX-PUSH-002, TX-PUSH-003, TX-PUSH-004,
//       TX-PUSH-005, TX-PUSH-006, TX-AUTH-001, TX-AUTH-002
pub fn push(
    repo: &Repository,
    url: &str,
    creds: Option<&TransportCredentials>,
    updates: &[RefUpdate],
) -> Result<PushResult, TransportError> {
    if url.starts_with("git@") || url.starts_with("ssh://") {
        push_ssh(repo, url, updates)
    } else if url.starts_with("http://") || url.starts_with("https://") {
        let creds = creds.ok_or(TransportError::AuthRequired)?;
        push_http(repo, url, creds, updates)
    } else {
        Err(TransportError::Http(format!(
            "unsupported url scheme: {url}",
        )))
    }
}

fn push_http(
    repo: &Repository,
    url: &str,
    creds: &TransportCredentials,
    updates: &[RefUpdate],
) -> Result<PushResult, TransportError> {
    let _remote_refs = list_remote_refs(url, Some(creds))?;

    let body = build_push_body(repo, updates)?;

    let post_url = format!("{}/git-receive-pack", url.trim_end_matches('/'));
    let response = match ureq::post(&post_url)
        .set("Content-Type", "application/x-git-receive-pack-request")
        .set("Accept", "application/x-git-receive-pack-result")
        .set("Authorization", &auth_header(creds))
        .set("User-Agent", "rgit/0.1.0")
        .send_bytes(&body)
    {
        Ok(r) => r,
        Err(ureq::Error::Status(401, _)) => return Err(TransportError::AuthRequired),
        Err(e) => return Err(TransportError::Http(e.to_string())),
    };

    let mut resp_bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut resp_bytes)
        .map_err(TransportError::Io)?;
    parse_push_result(&resp_bytes)
}

/// Build the receive-pack request body: pkt-line ref-update commands,
/// flush packet, raw pack bytes. Same for HTTP and SSH transports.
fn build_push_body(repo: &Repository, updates: &[RefUpdate]) -> Result<Vec<u8>, TransportError> {
    let mut ids: HashSet<ObjectId> = HashSet::new();
    for update in updates {
        if update.new_id.is_zero() {
            continue;
        }
        collect_reachable_objects(repo, &update.new_id, &mut ids)?;
    }

    let mut writer = PackWriter::new();
    for id in &ids {
        let (kind, payload) = repo.read_object_raw(id)?;
        writer.add(kind, &payload);
    }
    let (pack_bytes, _pack_sha) = writer.finish()?;

    let mut body = Vec::new();
    for (i, update) in updates.iter().enumerate() {
        let mut line = format!(
            "{} {} {}",
            update.old_id.to_hex(),
            update.new_id.to_hex(),
            update.ref_name,
        );
        if i == 0 {
            line.push('\0');
            line.push_str("report-status");
        }
        line.push('\n');
        body.extend_from_slice(&pkt_line_encode(line.as_bytes()));
    }
    body.extend_from_slice(pkt_line_flush());
    body.extend_from_slice(&pack_bytes);
    Ok(body)
}

#[derive(Debug)]
struct SshUrl {
    user: String,
    host: String,
    path: String,
}

fn parse_ssh_url(url: &str) -> Result<SshUrl, TransportError> {
    // `ssh://user@host[:port]/path`
    if let Some(rest) = url.strip_prefix("ssh://") {
        let (user_host_port, path) = rest
            .split_once('/')
            .ok_or_else(|| TransportError::Http(format!("ssh url missing path: {url}")))?;
        let (user, host_port) = user_host_port
            .split_once('@')
            .unwrap_or(("git", user_host_port));
        let host = host_port.split(':').next().unwrap_or(host_port);
        return Ok(SshUrl {
            user: user.to_string(),
            host: host.to_string(),
            path: format!("/{path}"),
        });
    }
    // `user@host:path` (the scp-like form GitHub uses)
    if let Some((user_host, path)) = url.split_once(':') {
        if let Some((user, host)) = user_host.split_once('@') {
            return Ok(SshUrl {
                user: user.to_string(),
                host: host.to_string(),
                path: path.to_string(),
            });
        }
    }
    Err(TransportError::Http(
        format!("unrecognized ssh url: {url}",),
    ))
}

fn push_ssh(
    repo: &Repository,
    url: &str,
    updates: &[RefUpdate],
) -> Result<PushResult, TransportError> {
    use std::io::{Read as _, Write as _};
    use std::process::{Command, Stdio};

    let parsed = parse_ssh_url(url)?;
    let mut child = Command::new("ssh")
        .arg(format!("{}@{}", parsed.user, parsed.host))
        .arg("git-receive-pack")
        .arg(&parsed.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| TransportError::Http(format!("ssh spawn failed: {e}")))?;

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stdin = child.stdin.take().expect("piped stdin");

    // Read the server's ref advertisement (no `# service=` line over SSH;
    // server writes ref pkt-lines directly until flush).
    let remote_refs = read_ssh_ref_advertisement(&mut stdout)?;

    // Resolve old_id from the advertisement for each update (caller may
    // have left it as ZERO).
    let resolved_updates: Vec<RefUpdate> = updates
        .iter()
        .map(|u| {
            let old_id = if u.old_id.is_zero() {
                remote_refs
                    .iter()
                    .find(|r| r.name == u.ref_name)
                    .map(|r| r.id)
                    .unwrap_or(ObjectId::ZERO)
            } else {
                u.old_id
            };
            RefUpdate {
                old_id,
                new_id: u.new_id,
                ref_name: u.ref_name.clone(),
            }
        })
        .collect();

    // If everything's already up to date, send a no-op flush so the
    // server closes cleanly, then return.
    if resolved_updates.iter().all(|u| u.old_id == u.new_id) {
        stdin
            .write_all(pkt_line_flush())
            .map_err(TransportError::Io)?;
        drop(stdin);
        return Ok(PushResult {
            unpack_ok: true,
            per_ref: Vec::new(),
        });
    }

    let body = build_push_body(repo, &resolved_updates)?;
    stdin.write_all(&body).map_err(TransportError::Io)?;
    drop(stdin); // signal EOF so the server starts processing.

    let mut response = Vec::new();
    stdout
        .read_to_end(&mut response)
        .map_err(TransportError::Io)?;

    let status = child.wait().map_err(TransportError::Io)?;
    if !status.success() {
        let mut stderr_bytes = Vec::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_end(&mut stderr_bytes);
        }
        return Err(TransportError::Server(format!(
            "ssh exited with {}: {}",
            status,
            String::from_utf8_lossy(&stderr_bytes).trim(),
        )));
    }

    parse_push_result(&response)
}

// ---------------------------------------------------------------------
// Clone (Smart HTTP v2 fetch)
// ---------------------------------------------------------------------

/// Clone the repository at `url` into `target_dir`. Initializes a new
/// repository, fetches HEAD plus reachable history, writes the received
/// pack, sets up local refs, and checks out the default branch.
///
/// `creds` is optional. Public repos can be cloned anonymously; private
/// repos need a PAT for HTTP Basic auth (same as `push`).
pub fn clone_http(
    url: &str,
    target_dir: &std::path::Path,
    creds: Option<&TransportCredentials>,
) -> Result<Repository, TransportError> {
    use crate::object::Object;

    let refs = ls_refs_v2(url, creds)?;
    let head_ref = refs
        .iter()
        .find(|r| r.name == "HEAD")
        .ok_or_else(|| TransportError::Server("remote did not advertise HEAD".into()))?;
    let head_id = head_ref.id;

    // Public repos sometimes advertise HEAD pointing at a sha that's
    // unborn (empty repo). Bail with a clear error in that case.
    if head_id.is_zero() {
        return Err(TransportError::Server(
            "remote HEAD is unborn (empty repo); nothing to clone".into(),
        ));
    }

    let pack_bytes = fetch_pack_v2(url, &[head_id], creds)?;

    let repo = Repository::init(target_dir, false)
        .map_err(|e| TransportError::Server(format!("init: {e}")))?;

    // Write the received pack into the repository and build its index.
    let pack_dir = target_dir.join(".git").join("objects").join("pack");
    std::fs::create_dir_all(&pack_dir).map_err(TransportError::Io)?;
    let temp_pack = pack_dir.join("rgit-incoming.pack");
    std::fs::write(&temp_pack, &pack_bytes).map_err(TransportError::Io)?;
    repo.import_pack(&temp_pack)
        .map_err(|e| TransportError::Server(format!("import_pack: {e}")))?;

    // Write every non-HEAD ref the server advertised.
    for r in &refs {
        if r.name == "HEAD" {
            continue;
        }
        // Skip refs that aren't under a top-level namespace we recognize
        // (e.g., refs/pull/…). v1 mirrors only branches and tags.
        if !(r.name.starts_with("refs/heads/") || r.name.starts_with("refs/tags/")) {
            continue;
        }
        repo.write_ref(&r.name, &r.id)
            .map_err(|e| TransportError::Server(format!("write_ref {}: {e}", r.name)))?;
    }

    // Choose a HEAD: prefer a branch whose id matches the advertised HEAD.
    let head_branch = refs
        .iter()
        .filter(|r| r.name.starts_with("refs/heads/") && r.id == head_id)
        .map(|r| r.name.clone())
        .min();
    let repo = match head_branch {
        Some(branch) => {
            repo.set_head_symbolic(&branch)
                .map_err(|e| TransportError::Server(format!("set HEAD: {e}")))?;
            // Re-open so the packs list is repopulated cleanly. (The
            // existing handle may not see the just-imported pack until
            // `Repository::open` re-scans `.git/objects/pack/`.)
            Repository::open(target_dir)
                .map_err(|e| TransportError::Server(format!("reopen: {e}")))?
        }
        None => {
            repo.set_head_detached(&head_id)
                .map_err(|e| TransportError::Server(format!("set HEAD: {e}")))?;
            Repository::open(target_dir)
                .map_err(|e| TransportError::Server(format!("reopen: {e}")))?
        }
    };

    // Check out HEAD's tree into the working tree.
    let head_obj = repo.read_object(&head_id)?;
    if let Object::Commit(commit) = head_obj {
        let index = repo
            .checkout(&commit.tree)
            .map_err(|e| TransportError::Server(format!("checkout: {e}")))?;
        repo.write_index(&index)
            .map_err(|e| TransportError::Server(format!("write_index: {e}")))?;
    }

    Ok(repo)
}

fn ls_refs_v2(
    url: &str,
    creds: Option<&TransportCredentials>,
) -> Result<Vec<RemoteRef>, TransportError> {
    let post_url = format!("{}/git-upload-pack", url.trim_end_matches('/'));

    // command=ls-refs   delim   peel\n   symrefs\n   ref-prefix refs/   flush
    let mut body = Vec::new();
    body.extend_from_slice(&pkt_line_encode(b"command=ls-refs\n"));
    body.extend_from_slice(b"0001"); // delimiter
    body.extend_from_slice(&pkt_line_encode(b"peel\n"));
    body.extend_from_slice(&pkt_line_encode(b"symrefs\n"));
    body.extend_from_slice(b"0000"); // flush

    let mut req = ureq::post(&post_url)
        .set("Content-Type", "application/x-git-upload-pack-request")
        .set("Accept", "application/x-git-upload-pack-result")
        .set("Git-Protocol", "version=2")
        .set("User-Agent", "rgit/0.1.0");
    if let Some(c) = creds {
        req = req.set("Authorization", &auth_header(c));
    }

    let response = match req.send_bytes(&body) {
        Ok(r) => r,
        Err(ureq::Error::Status(401, _)) => return Err(TransportError::AuthRequired),
        Err(e) => return Err(TransportError::Http(e.to_string())),
    };

    let mut resp_bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut resp_bytes)
        .map_err(TransportError::Io)?;

    parse_ls_refs_v2(&resp_bytes)
}

fn parse_ls_refs_v2(bytes: &[u8]) -> Result<Vec<RemoteRef>, TransportError> {
    let pkts = pkt_line_decode_all(bytes)?;
    let mut refs = Vec::new();
    for pkt in pkts {
        let line = if pkt.last() == Some(&b'\n') {
            &pkt[..pkt.len() - 1]
        } else {
            &pkt[..]
        };
        if line.len() < 41 || line[40] != b' ' {
            continue;
        }
        let hex = std::str::from_utf8(&line[..40]).map_err(|_| TransportError::PktLine)?;
        let id = match ObjectId::from_hex(hex) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let rest = &line[41..];
        // Ref name runs to either end-of-line or a space (attributes follow).
        let name_end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
        let name = String::from_utf8_lossy(&rest[..name_end]).into_owned();
        refs.push(RemoteRef { name, id });
    }
    Ok(refs)
}

fn fetch_pack_v2(
    url: &str,
    wants: &[ObjectId],
    creds: Option<&TransportCredentials>,
) -> Result<Vec<u8>, TransportError> {
    let post_url = format!("{}/git-upload-pack", url.trim_end_matches('/'));

    let mut body = Vec::new();
    body.extend_from_slice(&pkt_line_encode(b"command=fetch\n"));
    body.extend_from_slice(b"0001"); // delimiter
    body.extend_from_slice(&pkt_line_encode(b"no-progress\n"));
    body.extend_from_slice(&pkt_line_encode(b"ofs-delta\n"));
    for want in wants {
        let line = format!("want {}\n", want.to_hex());
        body.extend_from_slice(&pkt_line_encode(line.as_bytes()));
    }
    body.extend_from_slice(&pkt_line_encode(b"done\n"));
    body.extend_from_slice(b"0000"); // flush

    let mut req = ureq::post(&post_url)
        .set("Content-Type", "application/x-git-upload-pack-request")
        .set("Accept", "application/x-git-upload-pack-result")
        .set("Git-Protocol", "version=2")
        .set("User-Agent", "rgit/0.1.0");
    if let Some(c) = creds {
        req = req.set("Authorization", &auth_header(c));
    }

    let response = match req.send_bytes(&body) {
        Ok(r) => r,
        Err(ureq::Error::Status(401, _)) => return Err(TransportError::AuthRequired),
        Err(e) => return Err(TransportError::Http(e.to_string())),
    };

    let mut resp_bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut resp_bytes)
        .map_err(TransportError::Io)?;

    extract_pack_from_v2_fetch(&resp_bytes)
}

/// Pull the packfile bytes out of a v2 fetch response. The response is
/// a sequence of pkt-lines: optionally an acknowledgments section,
/// optionally a "packfile" marker, then sideband-prefixed pkt-lines
/// where byte 1 = pack data, 2 = progress, 3 = error.
fn extract_pack_from_v2_fetch(bytes: &[u8]) -> Result<Vec<u8>, TransportError> {
    let pkts = pkt_line_decode_all(bytes)?;
    let mut in_packfile = false;
    let mut pack = Vec::new();
    for pkt in pkts {
        if !in_packfile {
            // "packfile" marker line (possibly with trailing LF).
            let trimmed = if pkt.last() == Some(&b'\n') {
                &pkt[..pkt.len() - 1]
            } else {
                &pkt[..]
            };
            if trimmed == b"packfile" {
                in_packfile = true;
            }
            continue;
        }
        if pkt.is_empty() {
            continue;
        }
        match pkt[0] {
            1 => pack.extend_from_slice(&pkt[1..]),
            2 => { /* progress channel, swallow */ }
            3 => {
                return Err(TransportError::Server(
                    String::from_utf8_lossy(&pkt[1..]).trim().to_string(),
                ))
            }
            other => {
                return Err(TransportError::Server(format!(
                    "unexpected sideband channel: {other}",
                )))
            }
        }
    }
    if pack.is_empty() {
        return Err(TransportError::Server(
            "fetch response contained no packfile".into(),
        ));
    }
    Ok(pack)
}

/// Read a stream of pkt-line ref entries from the SSH stdout until a
/// flush packet. Unlike the HTTPS advertisement, there is no
/// `# service=…` preamble.
fn read_ssh_ref_advertisement(
    stdout: &mut impl std::io::Read,
) -> Result<Vec<RemoteRef>, TransportError> {
    let mut refs = Vec::new();
    loop {
        let mut len_bytes = [0u8; 4];
        match stdout.read_exact(&mut len_bytes) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(TransportError::Io(e)),
        }
        let len_str = std::str::from_utf8(&len_bytes).map_err(|_| TransportError::PktLine)?;
        let len = u32::from_str_radix(len_str, 16).map_err(|_| TransportError::PktLine)? as usize;
        if len == 0 {
            break; // flush
        }
        if len < 4 {
            return Err(TransportError::PktLine);
        }
        let mut payload = vec![0u8; len - 4];
        stdout
            .read_exact(&mut payload)
            .map_err(TransportError::Io)?;
        let line = if payload.last() == Some(&b'\n') {
            &payload[..payload.len() - 1]
        } else {
            &payload[..]
        };
        if line.len() < 41 || line[40] != b' ' {
            continue;
        }
        let hex = std::str::from_utf8(&line[..40]).map_err(|_| TransportError::PktLine)?;
        let id = match ObjectId::from_hex(hex) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let rest = &line[41..];
        let name_end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        let name = String::from_utf8_lossy(&rest[..name_end]).into_owned();
        // GitHub advertises a synthetic "capabilities^{}" entry for empty
        // repos. Skip it.
        if name == "capabilities^{}" {
            continue;
        }
        refs.push(RemoteRef { name, id });
    }
    Ok(refs)
}

fn parse_push_result(body: &[u8]) -> Result<PushResult, TransportError> {
    let pkts = pkt_line_decode_all(body)?;
    let mut unpack_ok = false;
    let mut per_ref: Vec<(String, Result<(), String>)> = Vec::new();
    for pkt in pkts {
        let line = if pkt.last() == Some(&b'\n') {
            &pkt[..pkt.len() - 1]
        } else {
            &pkt[..]
        };
        let line_str = std::str::from_utf8(line).unwrap_or("");
        if let Some(rest) = line_str.strip_prefix("unpack ") {
            unpack_ok = rest == "ok";
            if !unpack_ok {
                return Err(TransportError::Server(format!("unpack failed: {rest}")));
            }
        } else if let Some(rest) = line_str.strip_prefix("ok ") {
            per_ref.push((rest.to_string(), Ok(())));
        } else if let Some(rest) = line_str.strip_prefix("ng ") {
            // "ng <refname> <reason>"
            let mut parts = rest.splitn(2, ' ');
            let name = parts.next().unwrap_or("").to_string();
            let reason = parts.next().unwrap_or("").to_string();
            per_ref.push((name, Err(reason)));
        }
    }
    Ok(PushResult { unpack_ok, per_ref })
}

// ---------------------------------------------------------------------
// Object reachability walk
// ---------------------------------------------------------------------

/// Enumerate every object reachable from `start_id` (treated as a commit
/// root). Walks commits, their trees, and every transitive blob and
/// subtree. Gitlink entries are skipped.
// @spec TX-OBJWALK-001, TX-OBJWALK-002, TX-OBJWALK-003
pub fn collect_reachable_objects(
    repo: &Repository,
    start_id: &ObjectId,
    out: &mut HashSet<ObjectId>,
) -> Result<(), TransportError> {
    let mut commit_queue: Vec<ObjectId> = vec![*start_id];
    while let Some(id) = commit_queue.pop() {
        if !out.insert(id) {
            continue;
        }
        let obj = repo.read_object(&id)?;
        match obj {
            Object::Commit(c) => {
                walk_tree_collecting(repo, &c.tree, out)?;
                for parent in c.parents {
                    if !out.contains(&parent) {
                        commit_queue.push(parent);
                    }
                }
            }
            Object::Tag(t) => {
                if !out.contains(&t.object) {
                    commit_queue.push(t.object);
                }
            }
            Object::Tree(_) | Object::Blob(_) => {
                // Encountered directly (e.g., when start_id is a tag's
                // target). Walk if it's a tree; nothing more to do for blobs.
                if let Object::Tree(t) = repo.read_object(&id)? {
                    for entry in t.entries {
                        match entry.mode {
                            EntryMode::Tree => walk_tree_collecting(repo, &entry.id, out)?,
                            EntryMode::Gitlink => {} // skip submodules
                            _ => {
                                out.insert(entry.id);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn walk_tree_collecting(
    repo: &Repository,
    tree_id: &ObjectId,
    out: &mut HashSet<ObjectId>,
) -> Result<(), TransportError> {
    if !out.insert(*tree_id) {
        return Ok(());
    }
    let obj = repo.read_object(tree_id)?;
    let Object::Tree(tree) = obj else {
        return Ok(());
    };
    for entry in tree.entries {
        match entry.mode {
            EntryMode::Tree => walk_tree_collecting(repo, &entry.id, out)?,
            EntryMode::Gitlink => {} // skip per TX-OBJWALK-003
            _ => {
                out.insert(entry.id);
            }
        }
    }
    Ok(())
}

use std::io::Read as _;
