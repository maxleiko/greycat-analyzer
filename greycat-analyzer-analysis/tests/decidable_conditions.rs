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
//! - Boolean literal conditions (`if (true)` / `if (false)` / `!(true)`)
//!   are provably decidable but written by the author, so they are
//!   *suppressed* — only type-derived decidability warns.

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

/// Collect every user-visible message from both semantic diagnostics
/// (`analysis.diagnostics`) and lints (`module.lints`). The "condition
/// is always true / false" findings used to live in the diagnostics
/// stream but were migrated to the suppressible `decidable-condition`
/// lint rule, so the tests below match against the union of both. The
/// LSP / CLI surface them the same way (publishDiagnostics merges them
/// downstream), so the union is the right unit.
fn diag_messages(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    let mut out: Vec<String> = m
        .analysis
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect();
    out.extend(m.lints.iter().map(|l| l.message.clone()));
    out
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
fn literal_true_condition_does_not_warn() {
    // A written `true` is provably decidable but reveals nothing the
    // author didn't type — the warning is suppressed.
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
    assert!(
        always_true.is_empty(),
        "literal `true` must not warn, got: {diags:?}"
    );
}

#[test]
fn literal_false_condition_does_not_warn() {
    // `if (false)` is suppressed too; the dead body is still covered by
    // the separate `unreachable` lint via `decidable_conditions`.
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
    assert!(
        always_false.is_empty(),
        "literal `false` must not warn, got: {diags:?}"
    );
}

#[test]
fn negated_literal_does_not_warn() {
    // `!(true)` / `!(false)` reduce to a written constant through `!` and
    // `( )`, so they are suppressed as well.
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
    let decidable: Vec<&String> = diags
        .iter()
        .filter(|m| m.contains("always false") || m.contains("always true"))
        .collect();
    assert!(
        decidable.is_empty(),
        "negated literal must not warn, got: {diags:?}"
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

#[test]
fn union_of_subtypes_assigns_to_common_supertype() {
    // `s is Rect || s is Circle` narrows `s` to `Rect | Circle`.
    // Both variants extend `Shape`, so the union must be assignable
    // to a `Shape` parameter. The bug: the index-aware assignability
    // wrapper had no Union arm, so the per-alt recursion dropped
    // back to the core (inheritance-blind) relation and rejected
    // every alt, surfacing a false-positive "not assignable" diag.
    let src = "\
abstract type Shape {}
type Rect extends Shape {}
type Circle extends Shape {}
fn expect_shape(_: Shape) {}
fn main(s: Shape) {
    if (s is Rect || s is Circle) {
        expect_shape(s);
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "Union<Rect, Circle> must be assignable to Shape (common supertype), got: {diags:?}"
    );
}

#[test]
fn subtype_assigns_to_union_with_supertype_alt() {
    // `Cat -> Animal | int` must succeed via the supertype alt.
    // The bug: target-Union arm in core does `any(alt -> from)`
    // using the inheritance-blind core relation, so `Cat -> Animal`
    // failed; the index-aware wrapper had no Union arm to retry.
    let src = "\
type Animal {}
type Cat extends Animal {}
fn taker(_: Animal | int) {}
fn main() {
    var c = Cat{};
    taker(c);
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "Cat must be assignable to (Animal | int) via supertype alt, got: {diags:?}"
    );
}

#[test]
fn union_of_subtypes_casts_to_common_supertype() {
    // Symmetric bug in `is_castable_with_index` — same layering, same
    // missed Union arms. `(Rect | Circle) as Shape` must succeed.
    let src = "\
abstract type Shape {}
type Rect extends Shape {}
type Circle extends Shape {}
fn main(s: Shape) {
    if (s is Rect || s is Circle) {
        var _x = s as Shape;
    }
}
";
    let (uri, pa) = analyze(src);
    let cast_diags: Vec<String> = diag_messages(&pa, &uri)
        .into_iter()
        .filter(|m| m.contains("cannot cast") || m.contains("not castable"))
        .collect();
    assert!(
        cast_diags.is_empty(),
        "Union<Rect, Circle> must be castable to Shape (common supertype), got: {cast_diags:?}"
    );
}

// =============================================================================
// `is` on a runtime-erased generic result (function-generic erasure)
// =============================================================================

#[test]
fn is_check_on_erased_generic_result_reasons_about_runtime_type() {
    // `wrap` constructs & returns `Pair<T, int>`, which the GreyCat
    // runtime erases to `Pair<any?, int>`. The analyzer once read the
    // result as `Pair<int, int>` and would call `r is Pair<int, int>`
    // "always TRUE"; at runtime the erased value can never be the
    // specific type, so it's always FALSE (verified via greycat run on
    // the `Tuple<Table<T>, int>` analog). The `is`-decidability must
    // reason about the runtime-erased type, and say *why*.
    //
    // `Pair<A, B>` (two args, second concrete) mirrors the user's
    // `Tuple<Table<T>, int>`: the erased form isn't all-`any?`, so it
    // doesn't trip the all-Any wildcard in `is_assignable_to`.
    let src = "\
type Pair<A, B> { a: A; b: B; }
fn wrap<T>(x: T): Pair<T, int> { return Pair<T, int> { a: x, b: 0 }; }
fn main() {
    var r = wrap(1);
    if (r is Pair<int, int>) {
    }
}
";
    let (uri, pa) = analyze(src);
    let msgs = diag_messages(&pa, &uri);
    assert!(
        msgs.iter()
            .any(|m| m.contains("always false") && m.contains("erased")),
        "expected always-false-due-to-erasure, got: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("always true")),
        "must not claim always true on an erased value, got: {msgs:?}"
    );
}

#[test]
fn is_check_on_non_erased_generic_has_no_erasure_note() {
    // A genuinely-known `Pair<int, int>` (not from an erasing call) must
    // keep the plain decidable-condition wording — no erasure note.
    let src = "\
type Pair<A, B> { a: A; b: B; }
fn main() {
    var r = Pair<int, int> { a: 1, b: 2 };
    if (r is Pair<int, int>) {
    }
}
";
    let (uri, pa) = analyze(src);
    let msgs = diag_messages(&pa, &uri);
    assert!(
        msgs.iter().any(|m| m.contains("always true")),
        "non-erased exact-type check is always true: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("erased")),
        "non-erased check must not carry the erasure note: {msgs:?}"
    );
}
