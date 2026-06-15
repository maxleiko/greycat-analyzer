//! Regression test for Bug 3 — strict return-type contract enforcement.
//!
//! The runtime never validates the return type and implicitly returns
//! `null` when control falls off the end of a body. The analyzer is
//! strict: a function / method whose body can reach its end implicitly
//! returns `null`, so it must be flagged when `null` doesn't satisfy the
//! declared return type. Nullable return types accept the implicit null
//! and are fine even with a fall-through path.

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

fn code_count(pa: &ProjectAnalysis, uri: &Uri, code: &str) -> usize {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.code == code)
        .count()
}

fn missing_return_count(pa: &ProjectAnalysis, uri: &Uri) -> usize {
    code_count(pa, uri, "missing-return")
}

#[test]
fn non_nullable_return_with_reachable_end_is_flagged() {
    let (uri, pa) = analyze("fn bar(): String {}\n");
    assert_eq!(
        missing_return_count(&pa, &uri),
        1,
        "a non-nullable return type with an empty body must be flagged",
    );
}

#[test]
fn nullable_return_with_reachable_end_is_ok() {
    // Empty body and a partial-coverage body both fall through to an
    // implicit `null`, which satisfies a nullable return type.
    let src = "\
fn foo(): int? {}
fn baz(x: bool): float? {
    if (x) {
        return 3.14;
    }
}
";
    let (uri, pa) = analyze(src);
    assert_eq!(
        missing_return_count(&pa, &uri),
        0,
        "nullable return types accept the implicit null on fall-through",
    );
}

#[test]
fn void_function_is_never_flagged() {
    let (uri, pa) = analyze("fn noop() {\n    var x = 1;\n}\n");
    assert_eq!(
        missing_return_count(&pa, &uri),
        0,
        "a function with no declared return type has no contract to violate",
    );
}

#[test]
fn all_paths_return_is_ok() {
    let src = "\
fn pick(x: bool): int {
    if (x) {
        return 1;
    } else {
        return 2;
    }
}
";
    let (uri, pa) = analyze(src);
    assert_eq!(
        missing_return_count(&pa, &uri),
        0,
        "every path returns, so the end of the body is unreachable",
    );
}

#[test]
fn body_ending_in_throw_is_ok() {
    let src = "\
fn boom(): int {
    throw \"nope\";
}
";
    let (uri, pa) = analyze(src);
    assert_eq!(
        missing_return_count(&pa, &uri),
        0,
        "a throw diverges, so the end of the body is unreachable",
    );
}

#[test]
fn valueless_return_in_non_nullable_fn_is_flagged() {
    // `return;` returns null; the runtime throws "wrong return type ...
    // null found while none nullable expected" when the path runs.
    let src = "\
fn make(x: bool): int {
    if (x) {
        return 7;
    }
    return;
}
";
    let (uri, pa) = analyze(src);
    // A bare `return;` is `return null;` — same `type-mismatch` the
    // valued path emits, not `missing-return`.
    assert_eq!(
        code_count(&pa, &uri, "type-mismatch"),
        1,
        "a valueless `return;` in a non-nullable fn must be flagged",
    );
    assert_eq!(missing_return_count(&pa, &uri), 0);
}

#[test]
fn valueless_return_in_nullable_fn_is_ok() {
    let src = "\
fn make(x: bool): int? {
    if (x) {
        return 7;
    }
    return;
}
";
    let (uri, pa) = analyze(src);
    assert_eq!(
        code_count(&pa, &uri, "type-mismatch"),
        0,
        "a valueless `return;` returns null, which satisfies a nullable return type",
    );
}

#[test]
fn infinite_while_true_with_inner_return_is_ok() {
    // The end of the body is genuinely unreachable: the only exit is the
    // inner `return`. The narrow infinite-loop rule must see this.
    let src = "\
fn ready(): bool { return true; }
fn poll(): int {
    while (true) {
        if (ready()) {
            return 1;
        }
    }
}
";
    let (uri, pa) = analyze(src);
    assert_eq!(
        missing_return_count(&pa, &uri),
        0,
        "`while (true)` with no break never falls through",
    );
}

#[test]
fn literal_true_for_with_inner_return_is_ok() {
    let src = "\
fn forever(): int {
    for (var i = 0; true; i = i + 1) {
        return 2;
    }
}
";
    let (uri, pa) = analyze(src);
    assert_eq!(
        missing_return_count(&pa, &uri),
        0,
        "a `for` with a literal-true condition and no break never falls through",
    );
}

#[test]
fn while_true_with_break_is_flagged() {
    // A `break` makes the loop exit reachable, so control can fall off
    // the end — stays conservative, stays flagged.
    let src = "\
fn ready(): bool { return true; }
fn breaks(): int {
    while (true) {
        if (ready()) {
            break;
        }
    }
}
";
    let (uri, pa) = analyze(src);
    assert_eq!(
        missing_return_count(&pa, &uri),
        1,
        "a break targeting the loop makes the end reachable",
    );
}

#[test]
fn while_with_non_literal_condition_is_flagged() {
    // We don't const-fold non-literal conditions, so the loop may not
    // run at all and the end is reachable.
    let src = "\
fn cond(x: bool): int {
    while (x) {
        return 3;
    }
}
";
    let (uri, pa) = analyze(src);
    assert_eq!(
        missing_return_count(&pa, &uri),
        1,
        "a non-literal loop condition stays conservative (reachable end)",
    );
}

#[test]
fn non_nullable_method_with_reachable_end_is_flagged() {
    let src = "\
type Box {
    fn get(): int {
        var x = 1;
    }
}
";
    let (uri, pa) = analyze(src);
    assert_eq!(
        missing_return_count(&pa, &uri),
        1,
        "methods are covered by the same contract check as top-level fns",
    );
}
