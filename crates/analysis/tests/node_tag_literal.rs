//! node-tag literals (`42_node`, `42nodeGeo`, ...) type as the erased
//! node builtin (`node<any?>`, `nodeIndex<any?, any?>`) and assign into
//! a matching `node*` slot. They carry no "unknown suffix" anomaly.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

const STD_CORE: &str = "native type any {}\n\
     native type null {}\n\
     native type bool {}\n\
     native type int {}\n\
     native type float {}\n\
     native type String {}\n\
     native type Array<T> {}\n\
     native type node<T> {}\n\
     native type nodeTime<T> {}\n\
     native type nodeIndex<K, V> {}\n\
     native type nodeList<T> {}\n\
     native type nodeGeo<T> {}\n";

fn analyze(user_src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let std_uri = Uri::from_str("file:///std/core.gcl").unwrap();
    mgr.add_simple(std_uri, STD_CORE, "std", false);
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    mgr.add_simple(user_uri.clone(), user_src, "project", false);
    (user_uri, ProjectAnalysis::analyze(&mgr))
}

fn diag_codes(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .map(|d| d.code.to_owned())
        .collect()
}

#[test]
fn node_literals_assign_to_node_slots_without_diagnostics() {
    let src = "\
fn expect_node(_: node) {}
fn expect_nodeTime(_: nodeTime) {}
fn expect_nodeIndex(_: nodeIndex) {}
fn expect_nodeList(_: nodeList) {}
fn expect_nodeGeo(_: nodeGeo) {}
fn main() {
    expect_node(42_node);
    expect_nodeTime(42_nodeTime);
    expect_nodeIndex(42_nodeIndex);
    expect_nodeList(42nodeList);
    expect_nodeGeo(42_nodeGeo);
}
";
    let (uri, pa) = analyze(src);
    let codes = diag_codes(&pa, &uri);
    assert!(codes.is_empty(), "expected no diagnostics, got: {codes:?}");
}

#[test]
fn wrong_node_slot_is_rejected() {
    // A `nodeTime` literal does not satisfy a `node` parameter.
    let src = "\
fn expect_node(_: node) {}
fn main() { expect_node(42_nodeTime); }
";
    let (uri, pa) = analyze(src);
    let codes = diag_codes(&pa, &uri);
    assert!(
        codes.iter().any(|c| c == "argument-type-mismatch"),
        "got: {codes:?}",
    );
}
