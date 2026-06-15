//! node-tag generics are covariant in their arguments, not
//! bivariant.
//!
//! The runtime never validates a `var` declaration's type annotation
//! against its initializer (the same laxness as return types), so the
//! analyzer is the only guard. Previously the node-tag family
//! (`nodeIndex`, `nodeTime`, `nodeList`, `nodeGeo`, `node`) was treated
//! bivariantly — any same-handle instantiation was mutually assignable,
//! so `nodeIndex<int, float>` flowed into a `nodeIndex<int, String>`
//! slot. Now the args are checked per-position via `is_assignable_to`:
//! genuine subtyping passes, incompatible args are rejected.

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
     native type Map<K, V> {}\n\
     native type node<T> {}\n\
     native type nodeIndex<K, V> {}\n\
     native type nodeList<T> {}\n";

fn analyze(user_src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let std_uri = Uri::from_str("file:///std/core.gcl").unwrap();
    mgr.add_simple(std_uri, STD_CORE, "std", false);
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    mgr.add_simple(user_uri.clone(), user_src, "project", false);
    (user_uri, ProjectAnalysis::analyze(&mgr))
}

fn mismatches(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.code == "type-mismatch")
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn incompatible_node_index_value_arg_rejected() {
    let (uri, pa) =
        analyze("fn main() { var _: nodeIndex<int, String> = nodeIndex<int, float> {}; }\n");
    let ms = mismatches(&pa, &uri);
    assert_eq!(ms.len(), 1, "got: {ms:?}");
    assert!(ms[0].contains("nodeIndex<int, float>") && ms[0].contains("nodeIndex<int, String>"));
}

#[test]
fn equal_args_assign_clean() {
    let (uri, pa) =
        analyze("fn main() { var _: nodeIndex<int, String> = nodeIndex<int, String> {}; }\n");
    assert!(mismatches(&pa, &uri).is_empty());
}

#[test]
fn covariant_subtype_arg_assigns_clean() {
    // node<Dog> -> node<Animal> and nodeIndex<int, Dog> ->
    // nodeIndex<int, Animal> are valid: the value arg is covariant.
    let src = "\
type Animal {}
type Dog extends Animal {}
fn main() {
    var _a: node<Animal> = node<Dog> { Dog {} };
    var _b: nodeIndex<int, Animal> = nodeIndex<int, Dog> {};
}
";
    let (uri, pa) = analyze(src);
    assert!(
        mismatches(&pa, &uri).is_empty(),
        "got: {:?}",
        mismatches(&pa, &uri)
    );
}

#[test]
fn covariance_rejects_super_to_sub() {
    // nodeList<Animal> is NOT assignable to nodeList<Dog> — covariance
    // only flows sub -> super.
    let src = "\
type Animal {}
type Dog extends Animal {}
fn main() {
    var _a: nodeList<Dog> = nodeList<Animal> {};
}
";
    let (uri, pa) = analyze(src);
    let ms = mismatches(&pa, &uri);
    assert_eq!(ms.len(), 1, "got: {ms:?}");
}
