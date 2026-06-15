//! Generic type references must be instantiated with the
//! declared number of type arguments.
//!
//! Runtime: `nodeTime<int, float>` -> "nodeTime defines 1 generic params
//! while 2 detected". A bare reference (`Map`, `node`) is the all-`any?`
//! default and stays valid; only a non-empty, wrong-count argument list
//! is flagged. Applies to every generic head (stdlib natives + user
//! types), not just the node-tag family.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

/// Synthetic stdlib so `Array` / `Map` / `node*` resolve with their real
/// arities without needing `greycat install`.
const STD_CORE: &str = "native type any {}\n\
     native type null {}\n\
     native type bool {}\n\
     native type int {}\n\
     native type float {}\n\
     native type String {}\n\
     native type Array<T> {}\n\
     native type Map<K, V> {}\n\
     native type node<T> {}\n\
     native type nodeTime<T> {}\n\
     native type nodeIndex<K, V> {}\n";

fn analyze(user_src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let std_uri = Uri::from_str("file:///std/core.gcl").unwrap();
    mgr.add_simple(std_uri, STD_CORE, "std", false);
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    mgr.add_simple(user_uri.clone(), user_src, "project", false);
    (user_uri, ProjectAnalysis::analyze(&mgr))
}

fn arity_diags(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.code == "generic-arity-mismatch")
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn too_many_args_on_arity_one_native_flagged() {
    let (uri, pa) = analyze("fn main() { var _ = nodeTime<int, float> {}; }\n");
    let diags = arity_diags(&pa, &uri);
    assert_eq!(diags.len(), 1, "got: {diags:?}");
    assert!(diags[0].contains("expects 1 generic argument, but got 2"));
}

#[test]
fn too_few_args_on_arity_two_native_flagged() {
    let (uri, pa) = analyze("fn main() { var _ = nodeIndex<int> {}; }\n");
    let diags = arity_diags(&pa, &uri);
    assert_eq!(diags.len(), 1, "got: {diags:?}");
    assert!(diags[0].contains("expects 2 generic arguments, but got 1"));
}

#[test]
fn wrong_arity_on_user_generic_flagged() {
    let src = "\
type Box<T> { v: T; }
type Pair<K, V> { k: K; v: V; }
fn main() {
    var _a = Box<int, float> { v: 1 };
    var _b = Pair<int> { k: 1, v: 2 };
}
";
    let (uri, pa) = analyze(src);
    let diags = arity_diags(&pa, &uri);
    assert_eq!(diags.len(), 2, "got: {diags:?}");
}

#[test]
fn correct_arity_and_bare_refs_are_clean() {
    // Exact-count args, bare (defaulted) refs, and nested generics must
    // not be flagged.
    let src = "\
type Box<T> { v: T; }
fn main() {
    var _a: Array<int> = Array {};
    var _b: Map<int, String> = Map {};
    var _c = Box<int> { v: 1 };
    var _d: Array<Map<int, String>> = Array {};
    var _e: Array = Array {};
    var _f = node<int> { 1 };
}
";
    let (uri, pa) = analyze(src);
    let diags = arity_diags(&pa, &uri);
    assert!(diags.is_empty(), "expected no arity diags, got: {diags:?}");
}

#[test]
fn generic_param_in_scope_is_not_flagged() {
    // `T` inside `Box<T>` is a generic param, not a generic decl; using
    // it bare must not trip the arity check.
    let src = "\
type Box<T> { v: T; }
fn main() {
    var _b = Box<int> { v: 1 };
}
";
    let (uri, pa) = analyze(src);
    let diags = arity_diags(&pa, &uri);
    assert!(diags.is_empty(), "got: {diags:?}");
}
