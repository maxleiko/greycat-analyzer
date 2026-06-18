//! Regression test for the inheritance-depth diagnostic. The
//! GreyCat runtime rejects any `extends` chain reaching 5 levels
//! ("too depth inheritance: <name>"). The analyzer surfaces the
//! same constraint at declaration time so users hit it before
//! `greycat build`.

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

fn depth_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("inheritance chain too deep"))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn four_level_chain_is_accepted() {
    // A <- B <- C <- D (4 types, 3 extends edges) — runtime accepts.
    let src = "\
abstract type A {}
abstract type B extends A {}
abstract type C extends B {}
type D extends C {}
fn main() {}
";
    let (uri, pa) = analyze(src);
    let diags = depth_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no depth diagnostic, got: {diags:?}"
    );
}

#[test]
fn five_level_chain_is_rejected() {
    // A <- B <- C <- D <- E (5 types) — runtime errors with
    // "too depth inheritance: E"; analyzer must flag E.
    let src = "\
abstract type A {}
abstract type B extends A {}
abstract type C extends B {}
abstract type D extends C {}
type E extends D {}
fn main() {}
";
    let (uri, pa) = analyze(src);
    let diags = depth_diagnostics(&pa, &uri);
    assert_eq!(
        diags.len(),
        1,
        "expected exactly one depth diagnostic, got: {diags:?}"
    );
    assert!(
        diags[0].contains("`E`"),
        "diagnostic should name the offending type, got: {}",
        diags[0]
    );
    assert!(
        diags[0].contains("5 levels deep"),
        "diagnostic should state actual depth, got: {}",
        diags[0]
    );
    assert!(
        diags[0].contains("at most 4"),
        "diagnostic should state the limit, got: {}",
        diags[0]
    );
}

#[test]
fn single_type_no_extends_accepted() {
    // Sanity: a standalone type with no `extends` has chain length 1,
    // well within the limit.
    let src = "\
type Solo {}
fn main() {}
";
    let (uri, pa) = analyze(src);
    let diags = depth_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no depth diagnostic, got: {diags:?}"
    );
}
