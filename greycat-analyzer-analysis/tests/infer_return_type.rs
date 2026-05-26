//! Regression tests for the `infer-return-type` lint.
//!
//! Two distinct gaps the lint used to have:
//!
//! 1. It only walked the fn body's outer block and emitted on the
//!    *last* `Stmt::Return` found there. Returns nested inside an
//!    `if` / loop / `try` were invisible — so a fn with mixed-branch
//!    returns reported the type of whichever branch landed at the
//!    outer level. The user's reproduction was a `bool` hint for a
//!    function whose two paths returned `float?` and `bool`.
//!
//! 2. When branches genuinely disagree, the honest join is a union.
//!    GCL has no `T | U` syntax (the user could only annotate `: any`,
//!    which is uninformative and already filtered). The fix: when more
//!    than one distinct return type shows up, stay silent rather than
//!    emit a partial / misleading hint.
//!
//! Plus a gate against non-expressible types (`Lambda`, `Never`,
//! `Unresolved`) that could leak through even when branches agree.

use greycat_analyzer_analysis::lint::LintSeverity;
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

fn infer_return_hints<'a>(pa: &'a ProjectAnalysis, uri: &Uri) -> Vec<&'a str> {
    let m = pa.module(uri).expect("module");
    m.lints
        .iter()
        .filter(|l| l.rule == "infer-return-type")
        .map(|l| l.message.as_str())
        .collect()
}

#[test]
fn single_branch_return_emits_hint() {
    // Baseline: one return path, one settled type → emit.
    let src = "fn f() {\n    return 42;\n}\n";
    let (uri, pa) = analyze(src);
    let hints = infer_return_hints(&pa, &uri);
    assert_eq!(hints.len(), 1, "got: {hints:?}");
    assert!(
        hints[0].contains("`int`"),
        "expected `int` hint, got: {hints:?}"
    );
}

#[test]
fn nested_return_inside_if_is_seen() {
    // The fix's core repro: the only return path is inside an `if`
    // body. The old walker only inspected the outer block and emitted
    // nothing here; the new walker descends into branches.
    let src = "fn f(x: int) {\n    if (x == 0) {\n        return 42;\n    }\n}\n";
    let (uri, pa) = analyze(src);
    let hints = infer_return_hints(&pa, &uri);
    assert_eq!(hints.len(), 1, "got: {hints:?}");
    assert!(
        hints[0].contains("`int`"),
        "expected `int` hint from inside the if-body, got: {hints:?}"
    );
}

#[test]
fn null_and_nullable_branch_join_to_nullable() {
    // `return x.b` (type `float?`) on one branch, `return null` on the
    // other. The honest join is `float? | null = float?`, which is
    // expressible. The lint must propose `float?`, not skip.
    let src = "\
type Foo {
    a: int;
    b: float?;
}
fn foo(x: Foo) {
    if (x.a == 0) {
        return x.b;
    }
    return null;
}
";
    let (uri, pa) = analyze(src);
    let hints = infer_return_hints(&pa, &uri);
    assert_eq!(hints.len(), 1, "got: {hints:?}");
    assert!(
        hints[0].contains("`float?`"),
        "expected `float?` hint, got: {hints:?}"
    );
}

#[test]
fn nullable_and_nonnull_branches_join_when_wrap_exists() {
    // One branch returns `x.b: float?`, the other returns a non-null
    // `float`. The join is `float?`. The nullable wrap is already in
    // the arena (from `x.b`'s declared type), so the read-only intern
    // lookup finds it and the lint emits the hint.
    let src = "\
type Foo {
    a: int;
    b: float?;
}
fn foo(x: Foo) {
    if (x.a == 0) {
        return x.b;
    }
    return 3.14;
}
";
    let (uri, pa) = analyze(src);
    let hints = infer_return_hints(&pa, &uri);
    assert_eq!(hints.len(), 1, "got: {hints:?}");
    assert!(
        hints[0].contains("`float?`"),
        "expected `float?` hint, got: {hints:?}"
    );
}

#[test]
fn mixed_branch_returns_skip_when_join_is_not_expressible() {
    // True disagreement: one branch returns `float?`, the other
    // returns `bool`. The honest unified type is `float? | bool`
    // which has no GCL syntax. The fix detects the disagreement and
    // skips.
    let src = "\
type Foo {
    a: int;
    b: float?;
}
fn foo(x: Foo) {
    if (x.a == 0) {
        return x.b;
    }
    return false;
}
";
    let (uri, pa) = analyze(src);
    let hints = infer_return_hints(&pa, &uri);
    assert!(
        hints.is_empty(),
        "mixed-branch returns should not emit a hint, got: {hints:?}"
    );
}

#[test]
fn matching_branch_returns_emit_hint() {
    // When branches agree, the hint is still safe to emit. Both paths
    // return `int` so the unification succeeds.
    let src = "\
fn f(x: int) {
    if (x == 0) {
        return 1;
    }
    return 2;
}
";
    let (uri, pa) = analyze(src);
    let hints = infer_return_hints(&pa, &uri);
    assert_eq!(hints.len(), 1, "got: {hints:?}");
    assert!(
        hints[0].contains("`int`"),
        "expected `int` hint from matching branches, got: {hints:?}"
    );
}

#[test]
fn dead_return_after_divergent_sibling_does_not_pollute() {
    // The wrong-typed `return "dead"` sits after `return 1` in the
    // same block, so it's unreachable. The walker must stop at the
    // first divergent statement and not let the dead branch turn the
    // hint into a mixed-types skip. Mirrors what the `unreachable`
    // lint already flags as dead code.
    let src = "\
fn f(x: int) {
    if (x == 0) {
        return 1;
        return \"dead\";
    }
    return 2;
}
";
    let (uri, pa) = analyze(src);
    let hints = infer_return_hints(&pa, &uri);
    assert_eq!(
        hints.len(),
        1,
        "dead post-return statement must not break the hint: {hints:?}"
    );
    assert!(hints[0].contains("`int`"), "got: {hints:?}");
}

#[test]
fn return_after_throw_in_same_block_is_dead() {
    // `throw` is also divergent — the trailing `return "dead"` is
    // unreachable and must not contribute. The function's only live
    // return is the outer `return 42`.
    let src = "\
fn f(x: int) {
    if (x == 0) {
        throw \"bye\";
        return \"dead\";
    }
    return 42;
}
";
    let (uri, pa) = analyze(src);
    let hints = infer_return_hints(&pa, &uri);
    assert_eq!(hints.len(), 1, "got: {hints:?}");
    assert!(hints[0].contains("`int`"), "got: {hints:?}");
}

#[test]
fn lambda_return_type_is_not_expressible_skip() {
    // Closure type is `Lambda { params, ret }` in the type arena. GCL
    // has no literal for function types beyond the `function` primitive,
    // and the analyzer types closures with their full signature here,
    // not as `function`. So the hint would render as a non-syntactic
    // form like `(int) -> int` — skip.
    let src = "fn f() {\n    return |x: int| x + 1;\n}\n";
    let (uri, pa) = analyze(src);
    let hints = infer_return_hints(&pa, &uri);
    assert!(
        hints.is_empty(),
        "lambda-typed returns should not emit a hint, got: {hints:?}"
    );
}

#[test]
fn hint_severity_is_hint() {
    // Defensive: severity is `Hint` (not `Warning`). Editors render
    // hints with a softer affordance, which is what we want for a
    // suggested annotation.
    let src = "fn f() {\n    return 42;\n}\n";
    let (uri, pa) = analyze(src);
    let m = pa.module(&uri).unwrap();
    let hit = m
        .lints
        .iter()
        .find(|l| l.rule == "infer-return-type")
        .expect("hint should fire");
    assert_eq!(hit.severity, LintSeverity::Hint);
}

#[test]
fn ambiguous_decl_name_renders_with_module_qualifier() {
    // Two sibling modules both declare `PowerNetwork`. A consumer
    // module instantiates one of them and the lint suggests a return
    // type. Because the bare name is ambiguous project-wide, the hint
    // (and therefore the `--fix` it drives) must carry the
    // `<module>::Name` qualifier — pasting the bare name back into
    // source would immediately produce an `ambiguous-symbol` error.
    let mut mgr = SourceManager::new();
    let a_uri = Uri::from_str("file:///proj/src/a.gcl").unwrap();
    mgr.add_simple(a_uri, "type PowerNetwork {}\n", "project", false);
    let b_uri = Uri::from_str("file:///proj/src/b.gcl").unwrap();
    mgr.add_simple(b_uri, "type PowerNetwork {}\n", "project", false);
    let main_uri = Uri::from_str("file:///proj/src/main.gcl").unwrap();
    mgr.add_simple(
        main_uri.clone(),
        "fn small() {\n    return a::PowerNetwork {};\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let hints = infer_return_hints(&pa, &main_uri);
    assert_eq!(hints.len(), 1, "got: {hints:?}");
    assert!(
        hints[0].contains("`a::PowerNetwork`"),
        "expected qualified `a::PowerNetwork`, got: {hints:?}"
    );
}

#[test]
fn unambiguous_decl_name_stays_bare() {
    // Symmetric guard: when only one module exports the name, the hint
    // must NOT qualify (an unnecessary `mod::Name` would be churn in
    // the --fix output and confuse the user reading the diagnostic).
    let mut mgr = SourceManager::new();
    let a_uri = Uri::from_str("file:///proj/src/a.gcl").unwrap();
    mgr.add_simple(a_uri, "type PowerNetwork {}\n", "project", false);
    let main_uri = Uri::from_str("file:///proj/src/main.gcl").unwrap();
    mgr.add_simple(
        main_uri.clone(),
        "fn small() {\n    return PowerNetwork {};\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let hints = infer_return_hints(&pa, &main_uri);
    assert_eq!(hints.len(), 1, "got: {hints:?}");
    assert!(
        hints[0].contains("`PowerNetwork`") && !hints[0].contains("::"),
        "expected bare `PowerNetwork`, got: {hints:?}"
    );
}
