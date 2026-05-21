//! Spec-anchored tests for the refs module.

use super::*;
use std::fs;
use tempfile::TempDir;

fn make_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path(), false).unwrap();
    (dir, repo)
}

fn fake_id(byte: u8) -> ObjectId {
    let mut bytes = [0u8; 20];
    bytes.fill(byte);
    ObjectId::from_bytes(bytes)
}

// ---------------------------------------------------------------------
// REFS-NAME — validation
// ---------------------------------------------------------------------

// @spec REFS-NAME-001
#[test]
fn validates_empty_name() {
    let (_d, repo) = make_repo();
    assert!(matches!(
        repo.read_ref(""),
        Err(RefError::InvalidRefName(_))
    ));
    assert!(matches!(
        repo.write_ref("", &fake_id(0)),
        Err(RefError::InvalidRefName(_))
    ));
}

// @spec REFS-NAME-002
#[test]
fn validates_double_dot() {
    let (_d, repo) = make_repo();
    assert!(matches!(
        repo.read_ref("refs/heads/..evil"),
        Err(RefError::InvalidRefName(_))
    ));
}

// @spec REFS-NAME-003
#[test]
fn validates_special_chars() {
    let (_d, repo) = make_repo();
    for bad in [
        "refs/heads/x~y",
        "refs/heads/x^y",
        "refs/heads/x:y",
        "refs/heads/x?y",
        "refs/heads/x*y",
        "refs/heads/x[y",
        "refs/heads/x\\y",
    ] {
        assert!(
            matches!(repo.read_ref(bad), Err(RefError::InvalidRefName(_))),
            "expected reject for {bad:?}",
        );
    }
}

// @spec REFS-NAME-004
#[test]
fn validates_leading_dash_or_dot() {
    let (_d, repo) = make_repo();
    assert!(matches!(
        repo.read_ref("-evil"),
        Err(RefError::InvalidRefName(_))
    ));
    assert!(matches!(
        repo.read_ref(".evil"),
        Err(RefError::InvalidRefName(_))
    ));
}

// @spec REFS-NAME-005
#[test]
fn validates_trailing_slash_or_dotlock() {
    let (_d, repo) = make_repo();
    assert!(matches!(
        repo.read_ref("refs/heads/main/"),
        Err(RefError::InvalidRefName(_))
    ));
    assert!(matches!(
        repo.read_ref("refs/heads/main.lock"),
        Err(RefError::InvalidRefName(_))
    ));
}

// @spec REFS-NAME-006
#[test]
fn validates_double_slash() {
    let (_d, repo) = make_repo();
    assert!(matches!(
        repo.read_ref("refs//heads/main"),
        Err(RefError::InvalidRefName(_))
    ));
}

// @spec REFS-NAME-007
#[test]
fn validates_whitespace_and_control() {
    let (_d, repo) = make_repo();
    assert!(matches!(
        repo.read_ref("refs/heads/has space"),
        Err(RefError::InvalidRefName(_))
    ));
    assert!(matches!(
        repo.read_ref("refs/heads/has\ttab"),
        Err(RefError::InvalidRefName(_))
    ));
}

// @spec REFS-NAME-008
#[test]
fn validates_on_read_blocks_path_traversal() {
    let (_d, repo) = make_repo();
    // Without read-side validation, this would escape the git dir.
    assert!(matches!(
        repo.read_ref("../../etc/passwd"),
        Err(RefError::InvalidRefName(_))
    ));
}

// ---------------------------------------------------------------------
// REFS-LOOSE — loose ref file format
// ---------------------------------------------------------------------

// @spec REFS-LOOSE-001, REFS-LOOSE-002
#[test]
fn write_then_read_roundtrip() {
    let (_d, repo) = make_repo();
    let id = fake_id(0x42);
    repo.write_ref("refs/heads/main", &id).unwrap();
    let path = repo.git_dir().join("refs/heads/main");
    let contents = fs::read_to_string(&path).unwrap();
    assert_eq!(contents, format!("{}\n", id.to_hex()));
    let read = repo.read_ref("refs/heads/main").unwrap();
    assert_eq!(read, id);
}

// @spec REFS-LOOSE-003
#[test]
fn read_empty_loose_file_returns_invalid_content() {
    let (_d, repo) = make_repo();
    let path = repo.git_dir().join("refs/heads/empty");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"").unwrap();
    assert!(matches!(
        repo.read_ref("refs/heads/empty"),
        Err(RefError::InvalidRefContent(_))
    ));
}

// @spec REFS-LOOSE-004
#[test]
fn write_ref_uses_atomic_temp_and_rename() {
    let (_d, repo) = make_repo();
    let id = fake_id(0x55);
    repo.write_ref("refs/heads/atomic", &id).unwrap();
    let dir = repo.git_dir().join("refs/heads");
    // No tmp- files should remain after a successful write.
    for entry in fs::read_dir(&dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with("tmp-"),
            "unexpected tmp file remained: {name}",
        );
    }
}

// ---------------------------------------------------------------------
// REFS-PACKED — packed-refs handling
// ---------------------------------------------------------------------

// @spec REFS-PACKED-001, REFS-PACKED-002
#[test]
fn read_falls_back_to_packed_refs() {
    let (dir, _initial) = make_repo();
    let packed = "\
# pack-refs with: peeled fully-peeled sorted
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/from-packed
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/tags/v1.0
";
    fs::write(dir.path().join(".git/packed-refs"), packed).unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let id = repo.read_ref("refs/heads/from-packed").unwrap();
    assert_eq!(id.to_hex(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let tag = repo.read_ref("refs/tags/v1.0").unwrap();
    assert_eq!(tag.to_hex(), "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
}

// @spec REFS-PACKED-003, REFS-PACKED-005
#[test]
fn malformed_packed_lines_are_skipped() {
    let (dir, _initial) = make_repo();
    let packed = "\
# header comment
not a valid line
cccccccccccccccccccccccccccccccccccccccc refs/heads/good
xyz-not-hex refs/heads/bad-hex
dddddddddddddddddddddddddddddddddddddddd refs/heads/has space
";
    fs::write(dir.path().join(".git/packed-refs"), packed).unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.read_ref("refs/heads/good").is_ok());
    // bad-hex line was skipped silently — ref isn't there.
    assert!(matches!(
        repo.read_ref("refs/heads/bad-hex"),
        Err(RefError::RefNotFound(_))
    ));
}

// @spec REFS-PACKED-004
#[test]
fn peeled_tag_hint_lines_are_skipped() {
    let (dir, _initial) = make_repo();
    let packed = "\
# pack-refs with: peeled
eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee refs/tags/annotated
^ffffffffffffffffffffffffffffffffffffffff
";
    fs::write(dir.path().join(".git/packed-refs"), packed).unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let tag = repo.read_ref("refs/tags/annotated").unwrap();
    // The tag id is the line itself, not the peeled-hint id.
    assert_eq!(tag.to_hex(), "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
}

// ---------------------------------------------------------------------
// REFS-HEAD
// ---------------------------------------------------------------------

// @spec REFS-HEAD-001
#[test]
fn read_head_after_init_returns_symbolic() {
    let (_d, repo) = make_repo();
    match repo.read_head().unwrap() {
        HeadState::Symbolic(target) => assert_eq!(target, "refs/heads/main"),
        other => panic!("expected symbolic HEAD, got {other:?}"),
    }
}

// @spec REFS-HEAD-002, REFS-HEAD-006
#[test]
fn set_head_detached_then_read_returns_detached() {
    let (_d, repo) = make_repo();
    let id = fake_id(0x33);
    repo.set_head_detached(&id).unwrap();
    match repo.read_head().unwrap() {
        HeadState::Detached(read) => assert_eq!(read, id),
        other => panic!("expected detached HEAD, got {other:?}"),
    }
}

// @spec REFS-HEAD-003
#[test]
fn read_head_returns_not_found_when_missing() {
    let dir = TempDir::new().unwrap();
    // Build a valid-ish git dir but no HEAD.
    let git = dir.path().join(".git");
    fs::create_dir_all(git.join("objects")).unwrap();
    fs::create_dir_all(git.join("refs")).unwrap();
    // Can't use Repository::open — it would reject the dir without HEAD.
    // Instead, init creates HEAD then we delete it.
    Repository::init(dir.path(), false).unwrap();
    fs::remove_file(git.join("HEAD")).unwrap();
    let repo = Repository::init(dir.path(), false).unwrap();
    // Re-init would create HEAD if missing, so delete it again post-init.
    fs::remove_file(git.join("HEAD")).unwrap();
    assert!(matches!(repo.read_head(), Err(RefError::RefNotFound(_))));
}

// @spec REFS-HEAD-004
#[test]
fn read_head_returns_invalid_content_when_empty() {
    let (_d, repo) = make_repo();
    fs::write(repo.git_dir().join("HEAD"), b"").unwrap();
    assert!(matches!(
        repo.read_head(),
        Err(RefError::InvalidRefContent(_))
    ));
}

// @spec REFS-HEAD-005
#[test]
fn set_head_symbolic_writes_ref_line() {
    let (_d, repo) = make_repo();
    repo.set_head_symbolic("refs/heads/feature").unwrap();
    let head = fs::read_to_string(repo.git_dir().join("HEAD")).unwrap();
    assert_eq!(head, "ref: refs/heads/feature\n");
}

// @spec REFS-HEAD-007
#[test]
fn set_head_symbolic_rejects_non_refs_target() {
    let (_d, repo) = make_repo();
    assert!(matches!(
        repo.set_head_symbolic("not-refs/foo"),
        Err(RefError::InvalidRefName(_))
    ));
}

// ---------------------------------------------------------------------
// REFS-READ — read_ref behavior
// ---------------------------------------------------------------------

// @spec REFS-READ-001
#[test]
fn read_ref_prefers_loose_over_packed() {
    let (dir, _initial) = make_repo();
    let packed = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main
";
    fs::write(dir.path().join(".git/packed-refs"), packed).unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let loose_id = fake_id(0xbb);
    repo.write_ref("refs/heads/main", &loose_id).unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    // Loose shadows packed.
    let read = repo.read_ref("refs/heads/main").unwrap();
    assert_eq!(read, loose_id);
}

// @spec REFS-READ-002
#[test]
fn read_ref_returns_not_found_for_unknown_ref() {
    let (_d, repo) = make_repo();
    assert!(matches!(
        repo.read_ref("refs/heads/never-was"),
        Err(RefError::RefNotFound(_))
    ));
}

// @spec REFS-READ-003
#[test]
fn read_ref_resolves_symbolic_chain() {
    let (_d, repo) = make_repo();
    let target = fake_id(0xee);
    repo.write_ref("refs/heads/main", &target).unwrap();
    // HEAD is already symbolic → refs/heads/main from init.
    let id = repo.read_ref("HEAD").unwrap();
    assert_eq!(id, target);
}

// @spec REFS-READ-004
#[test]
fn read_ref_detects_symbolic_loop_at_depth_limit() {
    let (_d, repo) = make_repo();
    // Build a chain HEAD → a → b → c → d → e → f (depth 6, exceeds 5).
    fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/a\n").unwrap();
    for (from, to) in [
        ("refs/heads/a", "refs/heads/b"),
        ("refs/heads/b", "refs/heads/c"),
        ("refs/heads/c", "refs/heads/d"),
        ("refs/heads/d", "refs/heads/e"),
        ("refs/heads/e", "refs/heads/f"),
        ("refs/heads/f", "refs/heads/g"),
    ] {
        let path = repo.git_dir().join(from);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("ref: {to}\n")).unwrap();
    }
    assert!(matches!(
        repo.read_ref("HEAD"),
        Err(RefError::SymRefLoop(5))
    ));
}

// @spec REFS-READ-005
#[test]
fn read_ref_propagates_not_found_for_missing_symbolic_target() {
    let (_d, repo) = make_repo();
    // HEAD points at refs/heads/main but main doesn't exist yet.
    let result = repo.read_ref("HEAD");
    assert!(matches!(result, Err(RefError::RefNotFound(name)) if name == "refs/heads/main"));
}

// ---------------------------------------------------------------------
// REFS-WRITE — write_ref behavior
// ---------------------------------------------------------------------

// @spec REFS-WRITE-002
#[test]
fn write_ref_creates_parent_directories() {
    let (_d, repo) = make_repo();
    let id = fake_id(0x10);
    repo.write_ref("refs/heads/deep/nested/branch", &id)
        .unwrap();
    assert!(repo
        .git_dir()
        .join("refs/heads/deep/nested/branch")
        .is_file());
}

// @spec REFS-WRITE-003
#[test]
fn write_ref_overwrites_existing() {
    let (_d, repo) = make_repo();
    let id1 = fake_id(0x01);
    let id2 = fake_id(0x02);
    repo.write_ref("refs/heads/main", &id1).unwrap();
    repo.write_ref("refs/heads/main", &id2).unwrap();
    let read = repo.read_ref("refs/heads/main").unwrap();
    assert_eq!(read, id2);
}

// @spec REFS-WRITE-004
#[test]
fn write_ref_errors_when_parent_is_a_file() {
    let (_d, repo) = make_repo();
    // Create refs/heads/foo as a file.
    repo.write_ref("refs/heads/foo", &fake_id(0xaa)).unwrap();
    // Now try to write refs/heads/foo/bar — parent is a file.
    let result = repo.write_ref("refs/heads/foo/bar", &fake_id(0xbb));
    assert!(matches!(result, Err(RefError::Io(_))));
}

// ---------------------------------------------------------------------
// REFS-DELETE
// ---------------------------------------------------------------------

// @spec REFS-DELETE-001
#[test]
fn delete_ref_removes_loose_file() {
    let (_d, repo) = make_repo();
    repo.write_ref("refs/heads/doomed", &fake_id(0xdd)).unwrap();
    repo.delete_ref("refs/heads/doomed").unwrap();
    assert!(matches!(
        repo.read_ref("refs/heads/doomed"),
        Err(RefError::RefNotFound(_))
    ));
}

// @spec REFS-DELETE-002
#[test]
fn delete_ref_packed_only_errors() {
    let (dir, _initial) = make_repo();
    fs::write(
        dir.path().join(".git/packed-refs"),
        "cccccccccccccccccccccccccccccccccccccccc refs/heads/packed-only\n",
    )
    .unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let result = repo.delete_ref("refs/heads/packed-only");
    assert!(matches!(result, Err(RefError::CannotDeletePackedOnly(_))));
}

// @spec REFS-DELETE-003
#[test]
fn delete_ref_rejects_head() {
    let (_d, repo) = make_repo();
    assert!(matches!(
        repo.delete_ref("HEAD"),
        Err(RefError::CannotDeleteHead)
    ));
    // HEAD file untouched.
    assert!(repo.git_dir().join("HEAD").is_file());
}

// @spec REFS-DELETE-004
#[test]
fn delete_ref_not_found_for_unknown() {
    let (_d, repo) = make_repo();
    assert!(matches!(
        repo.delete_ref("refs/heads/never-existed"),
        Err(RefError::RefNotFound(_))
    ));
}

// ---------------------------------------------------------------------
// REFS-LIST
// ---------------------------------------------------------------------

// @spec REFS-LIST-001, REFS-LIST-004
#[test]
fn list_refs_returns_sorted_by_name() {
    let (_d, repo) = make_repo();
    repo.write_ref("refs/heads/zeta", &fake_id(0x1)).unwrap();
    repo.write_ref("refs/heads/alpha", &fake_id(0x2)).unwrap();
    repo.write_ref("refs/heads/middle", &fake_id(0x3)).unwrap();
    let refs = repo.list_refs("refs/heads/").unwrap();
    let names: Vec<&str> = refs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["refs/heads/alpha", "refs/heads/middle", "refs/heads/zeta"]
    );
}

// @spec REFS-LIST-002
#[test]
fn list_refs_loose_shadows_packed() {
    let (dir, _initial) = make_repo();
    fs::write(
        dir.path().join(".git/packed-refs"),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/shadowed\n",
    )
    .unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let loose_id = fake_id(0x99);
    repo.write_ref("refs/heads/shadowed", &loose_id).unwrap();
    let refs = repo.list_refs("refs/heads/").unwrap();
    let found = refs
        .iter()
        .find(|(n, _)| n == "refs/heads/shadowed")
        .unwrap();
    assert_eq!(found.1, loose_id);
}

// @spec REFS-LIST-003
#[test]
fn list_refs_with_empty_prefix_lists_every_ref() {
    let (_d, repo) = make_repo();
    repo.write_ref("refs/heads/x", &fake_id(0x1)).unwrap();
    repo.write_ref("refs/tags/v1", &fake_id(0x2)).unwrap();
    let refs = repo.list_refs("").unwrap();
    let names: Vec<&str> = refs.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"refs/heads/x"));
    assert!(names.contains(&"refs/tags/v1"));
}
