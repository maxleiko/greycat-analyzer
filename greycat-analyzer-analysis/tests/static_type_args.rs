//! Generic type arguments on a static access (`Foo<int>::bar()`) are a
//! hard error. GreyCat has no bounded generics, so the type parameter is
//! inert in any static context — a static carries no instance to bind it
//! from. The runtime rejects the construct outright; we mirror it with a
//! precise diagnostic whose span is the removable `<...>` slice, so the
//! auto-fix can strip it back to `Foo::bar()`.

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::ide::quickfix::{QuickfixCx, edit_for_diagnostic};
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

const TYPE_DECL: &str =
    "type Foo<T> {\n    val: T?;\n    static fn bar(): String { return \"a\"; }\n}\n";

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn error_codes(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    pa.module(uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.code.to_string())
        .collect()
}

#[test]
fn type_args_on_static_call_errors() {
    let mut mgr = SourceManager::new();
    let src = format!("{TYPE_DECL}fn main() {{\n    Foo<int>::bar();\n}}\n");
    let uri = add(&mut mgr, "/proj/main.gcl", &src);
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = error_codes(&pa, &uri);
    assert!(
        codes.iter().any(|c| c == "static-type-args"),
        "expected `static-type-args` on `Foo<int>::bar()`; got: {codes:?}",
    );
}

#[test]
fn bare_static_call_is_clean() {
    let mut mgr = SourceManager::new();
    let src = format!("{TYPE_DECL}fn main() {{\n    Foo::bar();\n}}\n");
    let uri = add(&mut mgr, "/proj/main.gcl", &src);
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = error_codes(&pa, &uri);
    assert!(
        !codes.iter().any(|c| c == "static-type-args"),
        "`Foo::bar()` must not emit `static-type-args`; got: {codes:?}",
    );
}

#[test]
fn type_args_on_static_value_ref_errors() {
    // Static methods are first-class values; the `<int>` is still inert.
    let mut mgr = SourceManager::new();
    let src = format!("{TYPE_DECL}fn main() {{\n    var f = Foo<int>::bar;\n}}\n");
    let uri = add(&mut mgr, "/proj/main.gcl", &src);
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = error_codes(&pa, &uri);
    assert!(
        codes.iter().any(|c| c == "static-type-args"),
        "expected `static-type-args` on the value-ref `Foo<int>::bar`; got: {codes:?}",
    );
}

#[test]
fn diagnostic_span_is_the_angle_bracket_slice() {
    // The span must cover exactly `<int>` so the auto-fix strips it and
    // leaves `Foo::bar()`.
    let mut mgr = SourceManager::new();
    let src = format!("{TYPE_DECL}fn main() {{\n    Foo<int>::bar();\n}}\n");
    let uri = add(&mut mgr, "/proj/main.gcl", &src);
    let pa = ProjectAnalysis::analyze(&mgr);
    let diag = pa
        .module(&uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .find(|d| d.code == "static-type-args")
        .expect("static-type-args diagnostic");
    assert_eq!(
        &src[diag.byte_range.clone()],
        "<int>",
        "span should cover the `<int>` slice only",
    );
}

#[test]
fn quickfix_strips_the_type_args() {
    let src = format!("{TYPE_DECL}fn main() {{\n    Foo<int>::bar();\n}}\n");
    let mut mgr = SourceManager::new();
    let uri = add(&mut mgr, "/proj/main.gcl", &src);
    let pa = ProjectAnalysis::analyze(&mgr);
    let module = pa.module(&uri).expect("module");
    let diag = module
        .analysis
        .diagnostics
        .iter()
        .find(|d| d.code == "static-type-args")
        .expect("static-type-args diagnostic");

    let tree = greycat_analyzer_syntax::parse(&src);
    let cx = QuickfixCx::from_cst(tree.root_node(), &src);
    let edits = edit_for_diagnostic(&cx, diag.code, &diag.byte_range, &diag.message);
    assert_eq!(edits.len(), 1, "expected one strip edit; got: {edits:?}");
    let edit = &edits[0];
    let mut fixed = src.clone();
    fixed.replace_range(edit.byte_range.clone(), &edit.new_text);
    assert!(
        fixed.contains("Foo::bar();") && !fixed.contains("Foo<int>::bar();"),
        "quickfix should rewrite `Foo<int>::bar()` to `Foo::bar()`; got:\n{fixed}",
    );
}
