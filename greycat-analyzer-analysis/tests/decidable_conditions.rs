//! Regression tests for trivially-decidable if-conditions and
//! disjunctive `is`-narrowing.
//!
//! Covered:
//! - `(x is A || x is B)` narrows `x` to `A | B` in the then-branch.
//! - `(x is A && x is B)` with disjoint A, B narrows `x` to `never`
//!   and emits "condition is always false".
//! - `x is T` on a binding whose declared type is already `T` emits
//!   "condition is always true".
//! - `x is T` on a binding declared `U` (disjoint from `T`) emits
//!   "condition is always false".
//! - `x != null` / `x == null` on a non-nullable binding emit
//!   always-true / always-false.
//! - Boolean literal conditions (`if (true)` / `if (false)`).

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

fn diag_messages(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect()
}

fn assignability_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    diag_messages(pa, uri)
        .into_iter()
        .filter(|m| m.contains("not assignable"))
        .collect()
}

#[test]
fn disjunctive_is_narrows_to_union() {
    // `x is int || x is float` should narrow `x` to `int | float` in
    // the then-branch. Pre-fix `x` stayed `any` and the call below
    // would have silently passed; post-fix the narrow exists and
    // surfaces an assignability error against a single-int taker.
    let src = "\
fn taker_int(v: int) {}
fn main(x: any) {
    if (x is int || x is float) {
        taker_int(x);
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert_eq!(
        diags.len(),
        1,
        "expected one assignability diag from union-narrow x going into int param, got: {diags:?}"
    );
    let msg = &diags[0];
    assert!(
        msg.contains("int | float") || msg.contains("float | int"),
        "expected union shape in message, got: {msg}"
    );
}

#[test]
fn disjunctive_is_then_typed_keeps_specific_narrow_callable() {
    // Specific-type call inside a single `is`-then still works
    // (sanity: the union path didn't break the single case).
    let src = "\
fn taker_int(v: int) {}
fn main(x: any) {
    if (x is int) {
        taker_int(x);
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no diags for narrowed-then-call, got: {diags:?}"
    );
}

#[test]
fn conjunctive_is_disjoint_types_always_false() {
    // `x is int && x is float` — int and float have no common
    // subtype, so the condition is always false and `x` narrows to
    // `never` in the then-branch.
    let src = "\
fn main(x: any) {
    if (x is int && x is float) {
        var _y = x;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = diag_messages(&pa, &uri);
    let always_false: Vec<&String> = diags
        .iter()
        .filter(|m| m.contains("always false") && m.contains('x'))
        .collect();
    assert_eq!(
        always_false.len(),
        1,
        "expected one always-false diag, got: {diags:?}"
    );
}

#[test]
fn conjunctive_is_subtype_keeps_most_specific() {
    // `x is Animal && x is Cat` where Cat <: Animal — no contradiction,
    // narrow to the most specific (Cat). No always-false diagnostic.
    let src = "\
type Animal { name: String; }
type Cat extends Animal { whiskers: int; }
fn main(x: any) {
    if (x is Animal && x is Cat) {
        var _y = x;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = diag_messages(&pa, &uri);
    let always_false: Vec<&String> = diags
        .iter()
        .filter(|m| m.contains("always false"))
        .collect();
    assert!(
        always_false.is_empty(),
        "expected no always-false diag for compatible types, got: {diags:?}"
    );
}

#[test]
fn is_check_redundant_on_already_typed_binding() {
    // `x: int; if (x is int)` — always true (x is already int).
    let src = "\
fn main(x: int) {
    if (x is int) {
        var _y = x;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = diag_messages(&pa, &uri);
    let always_true: Vec<&String> = diags.iter().filter(|m| m.contains("always true")).collect();
    assert_eq!(
        always_true.len(),
        1,
        "expected one always-true diag, got: {diags:?}"
    );
}

#[test]
fn is_check_disjoint_from_declared_type_always_false() {
    // `x: int; if (x is float)` — int and float are disjoint, always false.
    let src = "\
fn main(x: int) {
    if (x is float) {
        var _y = x;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = diag_messages(&pa, &uri);
    let always_false: Vec<&String> = diags
        .iter()
        .filter(|m| m.contains("always false"))
        .collect();
    assert_eq!(
        always_false.len(),
        1,
        "expected one always-false diag, got: {diags:?}"
    );
}

#[test]
fn is_check_on_any_does_not_warn() {
    // `x: any; if (x is int)` — any is the top type, every value
    // *could* be int, but the check is a meaningful runtime
    // discriminator. No always-(true|false) diagnostic.
    let src = "\
fn main(x: any) {
    if (x is int) {
        var _y = x;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = diag_messages(&pa, &uri);
    let trivial: Vec<&String> = diags
        .iter()
        .filter(|m| m.contains("always true") || m.contains("always false"))
        .collect();
    assert!(
        trivial.is_empty(),
        "expected no trivial-condition diag for any-typed receiver, got: {diags:?}"
    );
}

#[test]
fn null_check_on_non_nullable_binding_always_true() {
    // `x: int; if (x != null)` — int is non-nullable, always true.
    let src = "\
fn main(x: int) {
    if (x != null) {
        var _y = x;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = diag_messages(&pa, &uri);
    let always_true: Vec<&String> = diags.iter().filter(|m| m.contains("always true")).collect();
    assert_eq!(
        always_true.len(),
        1,
        "expected one always-true diag, got: {diags:?}"
    );
}

#[test]
fn null_eq_on_non_nullable_binding_always_false() {
    // `x: int; if (x == null)` — always false.
    let src = "\
fn main(x: int) {
    if (x == null) {
        var _y = x;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = diag_messages(&pa, &uri);
    let always_false: Vec<&String> = diags
        .iter()
        .filter(|m| m.contains("always false"))
        .collect();
    assert_eq!(
        always_false.len(),
        1,
        "expected one always-false diag, got: {diags:?}"
    );
}

#[test]
fn null_check_on_nullable_binding_does_not_warn() {
    // `x: int?` — null check is meaningful, no diag.
    let src = "\
fn main(x: int?) {
    if (x != null) {
        var _y = x;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = diag_messages(&pa, &uri);
    let trivial: Vec<&String> = diags
        .iter()
        .filter(|m| m.contains("always true") || m.contains("always false"))
        .collect();
    assert!(
        trivial.is_empty(),
        "expected no trivial-condition diag for nullable receiver, got: {diags:?}"
    );
}

#[test]
fn literal_true_condition_always_true() {
    let src = "\
fn main() {
    if (true) {
        var _y = 1;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = diag_messages(&pa, &uri);
    let always_true: Vec<&String> = diags.iter().filter(|m| m.contains("always true")).collect();
    assert_eq!(
        always_true.len(),
        1,
        "expected one always-true diag, got: {diags:?}"
    );
}

#[test]
fn literal_false_condition_always_false() {
    let src = "\
fn main() {
    if (false) {
        var _y = 1;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = diag_messages(&pa, &uri);
    let always_false: Vec<&String> = diags
        .iter()
        .filter(|m| m.contains("always false"))
        .collect();
    assert_eq!(
        always_false.len(),
        1,
        "expected one always-false diag, got: {diags:?}"
    );
}

#[test]
fn composition_not_inverts_decidable() {
    // `!(true)` is always false; `!(false)` is always true.
    let src = "\
fn main() {
    if (!(true)) {
        var _a = 1;
    }
    if (!(false)) {
        var _b = 1;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = diag_messages(&pa, &uri);
    let always_false: Vec<&String> = diags
        .iter()
        .filter(|m| m.contains("always false"))
        .collect();
    let always_true: Vec<&String> = diags.iter().filter(|m| m.contains("always true")).collect();
    assert_eq!(
        always_false.len(),
        1,
        "expected one always-false from !(true), got: {diags:?}"
    );
    assert_eq!(
        always_true.len(),
        1,
        "expected one always-true from !(false), got: {diags:?}"
    );
}

#[test]
fn composition_and_or_decidable() {
    // `true && false` → false; `true || false` → true.
    let src = "\
fn main() {
    if (true && false) {
        var _a = 1;
    }
    if (true || false) {
        var _b = 1;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = diag_messages(&pa, &uri);
    let always_false: Vec<&String> = diags
        .iter()
        .filter(|m| m.contains("always false"))
        .collect();
    let always_true: Vec<&String> = diags.iter().filter(|m| m.contains("always true")).collect();
    assert_eq!(
        always_false.len(),
        1,
        "expected one always-false, got: {diags:?}"
    );
    assert_eq!(
        always_true.len(),
        1,
        "expected one always-true, got: {diags:?}"
    );
}
