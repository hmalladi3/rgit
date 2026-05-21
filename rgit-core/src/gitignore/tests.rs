//! Spec-anchored tests for the gitignore module.

use super::*;

fn ignored(rules: &str, path: &str, is_dir: bool) -> bool {
    let mut set = GitignoreSet::new();
    set.add_rules(rules);
    set.is_ignored(path.as_bytes(), is_dir)
}

#[test]
fn literal_pattern_matches_anywhere() {
    assert!(ignored("CLAUDE.md", "CLAUDE.md", false));
    assert!(ignored("CLAUDE.md", "src/CLAUDE.md", false));
}

#[test]
fn comment_and_blank_lines_skipped() {
    assert!(!ignored("# a comment\n\nfoo", "bar", false));
    assert!(ignored("# comment\nfoo", "foo", false));
}

#[test]
fn star_matches_no_slash() {
    assert!(ignored("*.log", "app.log", false));
    assert!(ignored("*.log", "deep/app.log", false));
    assert!(!ignored("*.log", "app.log.txt", false));
}

#[test]
fn double_star_matches_any_path() {
    assert!(ignored("**/secret", "secret", false));
    assert!(ignored("**/secret", "a/b/secret", false));
}

#[test]
fn trailing_slash_means_directory_only() {
    assert!(ignored("target/", "target", true));
    assert!(!ignored("target/", "target", false));
}

#[test]
fn leading_slash_anchors_to_root() {
    assert!(ignored("/target", "target", false));
    assert!(!ignored("/target", "subdir/target", false));
}

#[test]
fn negation_unignores() {
    let rules = "*.log\n!important.log";
    assert!(ignored(rules, "debug.log", false));
    assert!(!ignored(rules, "important.log", false));
}

#[test]
fn anchored_pattern_with_slash_in_middle() {
    let rules = "src/secret";
    assert!(ignored(rules, "src/secret", false));
    assert!(!ignored(rules, "other/src/secret", false));
}

#[test]
fn unanchored_pattern_matches_at_any_depth() {
    let rules = "tmp";
    assert!(ignored(rules, "tmp", false));
    assert!(ignored(rules, "build/tmp", false));
    assert!(ignored(rules, "a/b/c/tmp", false));
}

#[test]
fn typical_rust_ignores() {
    let rules = "\
# Build artifacts
/target/

# Editor
.vscode/
*.swp

# OS
.DS_Store
";
    assert!(ignored(rules, "target", true));
    assert!(ignored(rules, ".vscode", true));
    assert!(ignored(rules, "src/main.rs.swp", false));
    assert!(ignored(rules, ".DS_Store", false));
    assert!(ignored(rules, "deep/dir/.DS_Store", false));
    assert!(!ignored(rules, "src/main.rs", false));
    assert!(!ignored(rules, "Cargo.toml", false));
}
