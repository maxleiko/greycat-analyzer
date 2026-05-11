//! End-to-end regression tests for the `unreachable` lint's
//! trivially-decidable-condition extension. Each shape covers:
//! - the lint emits a `Hint`-severity `unreachable` diagnostic
//!   at the expected range,
//! - the quickfix produces a single edit that re-parses cleanly
//!   and removes / unwraps to the live branch.

use greycat_analyzer_analysis::lint::LintSeverity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_analysis::quickfix;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze(src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///mod.gcl").unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    (uri, ProjectAnalysis::analyze(&mgr))
}

fn unreachable_diags(
    pa: &ProjectAnalysis,
    uri: &Uri,
) -> Vec<greycat_analyzer_analysis::lint::LintDiagnostic> {
    pa.module(uri)
        .expect("module")
        .lints
        .iter()
        .filter(|l| l.rule == "unreachable")
        .cloned()
        .collect()
}

/// Apply every `unreachable` quickfix to `src`, descending-byte-range,
/// matching the CLI's order.
fn apply_unreachable_fixes(src: &str, pa: &ProjectAnalysis, uri: &Uri) -> String {
    let mut diags = unreachable_diags(pa, uri);
    diags.sort_by_key(|d| std::cmp::Reverse(d.byte_range.start));
    let mut text = src.to_string();
    for d in &diags {
        let edits = quickfix::edit_for_diagnostic(&text, d.rule, &d.byte_range, &d.message);
        for e in edits {
            text.replace_range(e.byte_range.clone(), &e.new_text);
        }
    }
    text
}

fn assert_reparses(text: &str) {
    let tree = greycat_analyzer_syntax::parse(text);
    assert!(
        !tree.root_node().has_error(),
        "fixed source must re-parse cleanly:\n{text}"
    );
}

#[test]
fn always_false_if_no_else_deletes_whole_stmt() {
    let src = "\
fn main(x: int) {
    if (x is float) {
        var a = 1;
    }
    var keep = 99;
}
";
    let (uri, pa) = analyze(src);
    let diags = unreachable_diags(&pa, &uri);
    assert_eq!(diags.len(), 1, "expected one unreachable, got {diags:?}");
    assert_eq!(diags[0].severity, LintSeverity::Hint);
    // Range should span the whole if-stmt.
    let r = &diags[0].byte_range;
    assert!(
        src[r.clone()].starts_with("if "),
        "range start: {:?}",
        &src[r.clone()]
    );

    let fixed = apply_unreachable_fixes(src, &pa, &uri);
    assert_reparses(&fixed);
    assert!(
        !fixed.contains("if (x is float)"),
        "if-stmt should be gone:\n{fixed}"
    );
    assert!(
        fixed.contains("var keep = 99;"),
        "live tail should survive:\n{fixed}"
    );
}

#[test]
fn always_false_if_with_else_unwraps_to_else() {
    let src = "\
fn main(x: int) {
    if (x is float) {
        var a = 1;
    } else {
        var b = 2;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = unreachable_diags(&pa, &uri);
    assert_eq!(diags.len(), 1, "expected one unreachable, got {diags:?}");

    let fixed = apply_unreachable_fixes(src, &pa, &uri);
    assert_reparses(&fixed);
    assert!(
        !fixed.contains("if (x is float)"),
        "if scaffold should be gone:\n{fixed}"
    );
    assert!(
        !fixed.contains("} else {"),
        "else keyword should be gone:\n{fixed}"
    );
    assert!(
        fixed.contains("var b = 2;"),
        "live else body must survive:\n{fixed}"
    );
}

#[test]
fn always_true_if_with_else_drops_else_branch() {
    let src = "\
fn main(x: int) {
    if (x is int) {
        var a = 1;
    } else {
        var b = 2;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = unreachable_diags(&pa, &uri);
    assert_eq!(diags.len(), 1, "expected one unreachable, got {diags:?}");

    let fixed = apply_unreachable_fixes(src, &pa, &uri);
    assert_reparses(&fixed);
    assert!(
        fixed.contains("if (x is int)"),
        "if-stmt should survive:\n{fixed}"
    );
    assert!(
        fixed.contains("var a = 1;"),
        "live then body must survive:\n{fixed}"
    );
    assert!(
        !fixed.contains("} else {"),
        "else keyword should be gone:\n{fixed}"
    );
    assert!(
        !fixed.contains("var b = 2;"),
        "dead else body must be gone:\n{fixed}"
    );
}

#[test]
fn always_false_while_deletes_whole_stmt() {
    let src = "\
fn main() {
    while (false) {
        var a = 1;
    }
    var keep = 99;
}
";
    let (uri, pa) = analyze(src);
    let diags = unreachable_diags(&pa, &uri);
    assert_eq!(diags.len(), 1, "expected one unreachable, got {diags:?}");

    let fixed = apply_unreachable_fixes(src, &pa, &uri);
    assert_reparses(&fixed);
    assert!(
        !fixed.contains("while"),
        "while-stmt should be gone:\n{fixed}"
    );
    assert!(
        fixed.contains("var keep = 99;"),
        "live tail should survive:\n{fixed}"
    );
}

#[test]
fn always_false_for_deletes_whole_stmt() {
    let src = "\
fn main() {
    for (var i = 0; false; i = i + 1) {
        var a = 1;
    }
    var keep = 99;
}
";
    let (uri, pa) = analyze(src);
    let diags = unreachable_diags(&pa, &uri);
    assert_eq!(diags.len(), 1, "expected one unreachable, got {diags:?}");

    let fixed = apply_unreachable_fixes(src, &pa, &uri);
    assert_reparses(&fixed);
    assert!(
        !fixed.contains("for ("),
        "for-stmt should be gone:\n{fixed}"
    );
    assert!(
        fixed.contains("var keep = 99;"),
        "live tail should survive:\n{fixed}"
    );
}

#[test]
fn always_true_if_no_else_does_not_lint() {
    // No dead code — the then-branch is live, no else to drop.
    // Should not surface as `unreachable`.
    let src = "\
fn main(x: int) {
    if (x is int) {
        var a = 1;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = unreachable_diags(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no unreachable lints, got {diags:?}"
    );
}

#[test]
fn do_while_decidable_does_not_emit_unreachable() {
    // Body runs once regardless of the do-while condition.
    let src = "\
fn main() {
    do {
        var a = 1;
    } while (false);
}
";
    let (uri, pa) = analyze(src);
    let diags = unreachable_diags(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no unreachable lints, got {diags:?}"
    );
}

#[test]
fn nested_dead_if_inside_live_else_is_still_flagged() {
    // Outer if always-false → unwrap to else. The else block itself
    // contains another dead-if. After applying both fixes, the live
    // body remains.
    let src = "\
fn main(x: int) {
    if (x is float) {
        var a = 1;
    } else {
        if (x is float) {
            var b = 2;
        }
        var live = 7;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = unreachable_diags(&pa, &uri);
    assert_eq!(diags.len(), 2, "expected outer + inner dead, got {diags:?}");

    let fixed = apply_unreachable_fixes(src, &pa, &uri);
    assert_reparses(&fixed);
    assert!(
        !fixed.contains("if (x is float)"),
        "both dead ifs should be gone:\n{fixed}"
    );
    assert!(
        fixed.contains("var live = 7;"),
        "live body must survive:\n{fixed}"
    );
}
