//! Regression tests for enum-value-set narrowing and its effect on
//! the `non-exhaustive` lint.
//!
//! Setup: GCL doesn't have a `match`; users write enum-dispatch as
//! `if (c == E::A) ... else if (c == E::B) ...`. The
//! `non-exhaustive` lint detects chains like that and warns when not
//! every variant is covered. Before this fix the lint compared the
//! chain's covered set against the enum's *declared* variant list, so
//! an outer guard like `if (c == E::A || c == E::B) { ... }` couldn't
//! make a contained chain "exhaustive" — the lint still wanted
//! coverage of every other variant of E.
//!
//! With the fix, `derive_cond_narrows` populates a per-binding
//! enum-value set from `x == E::V` (singleton) and `||`-chains of
//! those (union); the `Stmt::If` then/else-entry writes the set to a
//! new narrow stack; and `check_enum_exhaustiveness` consults the
//! stack to scope the "expected" variant set.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze(src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///mod.gcl").unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    (uri, ProjectAnalysis::analyze(&mgr))
}

fn non_exhaustive_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.lints
        .iter()
        .filter(|d| d.rule == "non-exhaustive")
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn or_chain_narrow_makes_inner_chain_exhaustive() {
    // Outer `c == Red || c == Green` narrows c to {Red, Green}.
    // The inner chain covers exactly that subset → exhaustive.
    let src = "\
enum Color { Red; Green; Blue; Yellow; }
fn describeWarm(c: Color): String {
    if (c == Color::Red || c == Color::Green) {
        if (c == Color::Red) {
            return \"warm\";
        } else if (c == Color::Green) {
            return \"cool\";
        }
    }
    return \"other\";
}
";
    let (uri, pa) = analyze(src);
    let diags = non_exhaustive_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected the inner chain to be exhaustive over the narrowed {{Red, Green}} subset, got: {diags:?}",
    );
}

#[test]
fn no_outer_guard_still_reports_missing_variants() {
    // Sanity check that without an outer narrow, the chain is still
    // flagged for missing variants.
    let src = "\
enum Color { Red; Green; Blue; Yellow; }
fn pick(c: Color): String {
    if (c == Color::Red) {
        return \"r\";
    } else if (c == Color::Green) {
        return \"g\";
    }
    return \"other\";
}
";
    let (uri, pa) = analyze(src);
    let diags = non_exhaustive_diagnostics(&pa, &uri);
    assert!(
        diags
            .iter()
            .any(|d| d.contains("Blue") && d.contains("Yellow")),
        "expected a non-exhaustive diagnostic listing Blue and Yellow, got: {diags:?}",
    );
}

#[test]
fn or_chain_narrow_partial_inner_chain_still_reports() {
    // Outer narrow is {Red, Green, Blue}; inner only covers Red and
    // Green → Blue should still be reported missing.
    let src = "\
enum Color { Red; Green; Blue; Yellow; }
fn pick(c: Color): String {
    if (c == Color::Red || c == Color::Green || c == Color::Blue) {
        if (c == Color::Red) {
            return \"r\";
        } else if (c == Color::Green) {
            return \"g\";
        }
    }
    return \"other\";
}
";
    let (uri, pa) = analyze(src);
    let diags = non_exhaustive_diagnostics(&pa, &uri);
    assert!(
        diags
            .iter()
            .any(|d| d.contains("Blue") && !d.contains("Yellow")),
        "expected a non-exhaustive diagnostic listing Blue but not Yellow, got: {diags:?}",
    );
}

#[test]
fn single_variant_outer_eq_narrows_inner_chain() {
    // `c == Red` alone narrows c to {Red}; an inner chain matching
    // just Red is exhaustive over that subset.
    let src = "\
enum Color { Red; Green; Blue; Yellow; }
fn pick(c: Color): String {
    if (c == Color::Red) {
        if (c == Color::Red) {
            return \"r\";
        } else if (c == Color::Green) {
            return \"unreachable\";
        }
    }
    return \"other\";
}
";
    let (uri, pa) = analyze(src);
    let diags = non_exhaustive_diagnostics(&pa, &uri);
    // With the narrow, expected = {Red} ∩ declared = {Red}; chain
    // covers {Red, Green}; missing = ∅ → exhaustive.
    assert!(
        diags.is_empty(),
        "expected the inner chain to be exhaustive over {{Red}}, got: {diags:?}",
    );
}

#[test]
fn neq_else_branch_narrows() {
    // `if (c != Red) { ... } else { ... }` — inside the else, c is
    // narrowed to {Red}. (This exercises the else_enum_values path.)
    let src = "\
enum Color { Red; Green; Blue; Yellow; }
fn pick(c: Color): String {
    if (c != Color::Red) {
        return \"other\";
    } else {
        if (c == Color::Red) {
            return \"r\";
        }
    }
    return \"unreachable\";
}
";
    let (uri, pa) = analyze(src);
    let diags = non_exhaustive_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no non-exhaustive diagnostic — `c != Red` else-branch narrows c to {{Red}}, got: {diags:?}",
    );
}
