//! Hard-error contract for non-literal pragma arguments.
//!
//! Pragma args must be constant primitive literals (string / int /
//! float / bool / char / duration / time). Anything else — ident
//! reference, call, arithmetic expression — yields a
//! `Severity::Error` `SemanticDiagnostic` with code
//! `invalid-pragma-arg` that the package gate refuses on.

use std::str::FromStr;

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;

fn analyze(uri: &str, src: &str) -> ProjectAnalysis {
    let mut mgr = SourceManager::new();
    mgr.add_simple(Uri::from_str(uri).unwrap(), src, "project", false);
    let mut pa = ProjectAnalysis::new();
    pa.analyze_staged(&mgr);
    pa
}

fn diag_codes(pa: &ProjectAnalysis, uri: &str) -> Vec<&'static str> {
    let u = Uri::from_str(uri).unwrap();
    let m = pa.module(&u).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.code)
        .collect()
}

#[test]
fn literal_string_arg_accepted() {
    let pa = analyze("file:///proj/a.gcl", "@expose(\"public\")\nfn alpha() {}\n");
    let codes = diag_codes(&pa, "file:///proj/a.gcl");
    assert!(
        !codes.contains(&"invalid-pragma-arg"),
        "string literal arg should be valid; got {codes:?}"
    );
}

#[test]
fn literal_int_arg_accepted() {
    let pa = analyze("file:///proj/a.gcl", "@max_count(100)\nfn alpha() {}\n");
    let codes = diag_codes(&pa, "file:///proj/a.gcl");
    assert!(!codes.contains(&"invalid-pragma-arg"));
}

#[test]
fn literal_duration_arg_accepted() {
    let pa = analyze("file:///proj/a.gcl", "@timeout(5s)\nfn alpha() {}\n");
    let codes = diag_codes(&pa, "file:///proj/a.gcl");
    assert!(!codes.contains(&"invalid-pragma-arg"));
}

#[test]
fn ident_reference_arg_is_hard_error() {
    let pa = analyze("file:///proj/a.gcl", "@permission(admin)\nfn alpha() {}\n");
    let codes = diag_codes(&pa, "file:///proj/a.gcl");
    assert!(
        codes.contains(&"invalid-pragma-arg"),
        "bare ident `admin` should fail the validator; got {codes:?}"
    );
}

#[test]
fn arithmetic_expr_arg_is_hard_error() {
    // GreyCat doesn't constant-fold pragma args. `1 + 2` is a
    // runtime expression even though its value is statically known.
    let pa = analyze("file:///proj/a.gcl", "@max_count(1 + 2)\nfn alpha() {}\n");
    let codes = diag_codes(&pa, "file:///proj/a.gcl");
    assert!(
        codes.contains(&"invalid-pragma-arg"),
        "arithmetic expr arg should fail the validator; got {codes:?}"
    );
}

#[test]
fn pragma_on_type_decl_is_validated_too() {
    let pa = analyze(
        "file:///proj/t.gcl",
        "@tag(bogus_ident)\ntype Foo { x: int; }\n",
    );
    let codes = diag_codes(&pa, "file:///proj/t.gcl");
    assert!(
        codes.contains(&"invalid-pragma-arg"),
        "type-decl annotation must be checked too; got {codes:?}"
    );
}

#[test]
fn pragma_on_method_is_validated_too() {
    let pa = analyze(
        "file:///proj/t.gcl",
        "type Foo {\n  @expose(some_ref)\n  fn m() {}\n}\n",
    );
    let codes = diag_codes(&pa, "file:///proj/t.gcl");
    assert!(
        codes.contains(&"invalid-pragma-arg"),
        "method annotation must be checked too; got {codes:?}"
    );
}

#[test]
fn bare_type_ref_is_accepted_as_path() {
    // `@for_type(Foo)` where `Foo` is a defined type in the same
    // module — Path with chain = [Foo] resolves via `type_names`.
    let pa = analyze(
        "file:///proj/p.gcl",
        "type Foo { x: int; }\n@for_type(Foo)\nfn bar() {}\n",
    );
    let codes = diag_codes(&pa, "file:///proj/p.gcl");
    assert!(
        !codes.contains(&"invalid-pragma-arg"),
        "bare type ref should be valid; got {codes:?}"
    );
}

#[test]
fn enum_variant_ref_is_accepted_as_path() {
    // `@format(Color::red)` where `Color` is a same-module enum.
    let pa = analyze(
        "file:///proj/p.gcl",
        "enum Color { red; green; }\n@format(Color::red)\nfn bar() {}\n",
    );
    let codes = diag_codes(&pa, "file:///proj/p.gcl");
    assert!(
        !codes.contains(&"invalid-pragma-arg"),
        "enum variant ref should be valid; got {codes:?}"
    );
}

#[test]
fn null_literal_is_accepted() {
    let pa = analyze("file:///proj/p.gcl", "@default(null)\nfn bar() {}\n");
    let codes = diag_codes(&pa, "file:///proj/p.gcl");
    assert!(
        !codes.contains(&"invalid-pragma-arg"),
        "null literal should be valid; got {codes:?}"
    );
}

#[test]
fn array_literal_arg_is_hard_error() {
    let pa = analyze("file:///proj/p.gcl", "@tags([1, 2, 3])\nfn bar() {}\n");
    let codes = diag_codes(&pa, "file:///proj/p.gcl");
    assert!(
        codes.contains(&"invalid-pragma-arg"),
        "array literal can't be stored on a pragma at compile time; got {codes:?}"
    );
}

#[test]
fn call_arg_is_hard_error() {
    let pa = analyze(
        "file:///proj/p.gcl",
        "fn helper(): int { return 0; }\n@max(helper())\nfn bar() {}\n",
    );
    let codes = diag_codes(&pa, "file:///proj/p.gcl");
    assert!(
        codes.contains(&"invalid-pragma-arg"),
        "call expression can't be stored on a pragma at compile time; got {codes:?}"
    );
}
