//! Tests for the diff module.

use super::*;

#[test]
fn split_lines_preserves_trailing_lf() {
    let lines = split_lines(b"a\nb\nc\n");
    assert_eq!(lines, vec![b"a\n".as_slice(), b"b\n", b"c\n"]);
}

#[test]
fn split_lines_handles_final_line_without_lf() {
    let lines = split_lines(b"a\nb");
    assert_eq!(lines, vec![b"a\n".as_slice(), b"b"]);
}

#[test]
fn diff_lines_empty_vs_empty() {
    let a: &[&[u8]] = &[];
    let b: &[&[u8]] = &[];
    assert!(diff_lines(a, b).is_empty());
}

#[test]
fn diff_lines_pure_addition() {
    let a: Vec<&[u8]> = vec![];
    let b: Vec<&[u8]> = vec![b"x\n", b"y\n"];
    let script = diff_lines(&a, &b);
    assert_eq!(
        script,
        vec![LineChange::Added(b"x\n"), LineChange::Added(b"y\n"),]
    );
}

#[test]
fn diff_lines_pure_removal() {
    let a: Vec<&[u8]> = vec![b"x\n", b"y\n"];
    let b: Vec<&[u8]> = vec![];
    let script = diff_lines(&a, &b);
    assert_eq!(
        script,
        vec![LineChange::Removed(b"x\n"), LineChange::Removed(b"y\n"),]
    );
}

#[test]
fn diff_lines_in_middle_change() {
    let a: Vec<&[u8]> = vec![b"a\n", b"b\n", b"c\n"];
    let b: Vec<&[u8]> = vec![b"a\n", b"B\n", b"c\n"];
    let script = diff_lines(&a, &b);
    // Expected: Same(a), Removed(b), Added(B), Same(c). Order of
    // adjacent Remove/Add may vary; just check counts + same-lines.
    let same_count = script
        .iter()
        .filter(|c| matches!(c, LineChange::Same(_)))
        .count();
    let removed_count = script
        .iter()
        .filter(|c| matches!(c, LineChange::Removed(_)))
        .count();
    let added_count = script
        .iter()
        .filter(|c| matches!(c, LineChange::Added(_)))
        .count();
    assert_eq!(same_count, 2);
    assert_eq!(removed_count, 1);
    assert_eq!(added_count, 1);
}

#[test]
fn unified_diff_emits_label_lines_and_hunks() {
    let a = b"alpha\nbeta\ngamma\n";
    let b = b"alpha\nBETA\ngamma\n";
    let out = unified_diff(a, "a/file", b, "b/file", 3);
    assert!(out.starts_with("--- a/file\n+++ b/file\n"));
    assert!(out.contains("@@ "));
    assert!(out.contains("-beta\n"));
    assert!(out.contains("+BETA\n"));
}

#[test]
fn unified_diff_no_changes_emits_only_labels() {
    let a = b"alpha\nbeta\n";
    let b = b"alpha\nbeta\n";
    let out = unified_diff(a, "a/x", b, "b/x", 3);
    assert_eq!(out, "--- a/x\n+++ b/x\n");
}

#[test]
fn unified_diff_marks_missing_newline_at_eof() {
    let a = b"alpha\nbeta";
    let b = b"alpha\nBETA";
    let out = unified_diff(a, "a/x", b, "b/x", 3);
    assert!(out.contains("\\ No newline at end of file"));
}
