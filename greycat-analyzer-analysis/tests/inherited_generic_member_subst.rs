//! Member access on an attribute inherited from a generic
//! supertype must substitute the type parameter.
//!
//! `type IntBox extends Box<int>` where `Box<T> { v: T; }` — accessing
//! `intBox.v` must type as `int` (substituting `T := int` from the
//! `extends Box<int>` hop), even though `IntBox` is itself non-generic.
//! Previously member-access typing only substituted from the receiver's
//! own generic args and ignored the supertype chain, so `intBox.v` typed
//! as the raw `T` and produced a spurious "not assignable" diagnostic.
//! Works transitively through abstract intermediate types and through
//! node-wrapped attrs (`node: node<T>` -> `node<int>`).

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

const STD_CORE: &str = "native type any {}\n\
     native type null {}\n\
     native type bool {}\n\
     native type int {}\n\
     native type String {}\n\
     native type node<T> {}\n";

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
        .filter(|d| d.message.contains("not assignable to parameter"))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn inherited_plain_t_member_substitutes() {
    let src = "\
type Box<T> { v: T; }
type IntBox extends Box<int> {}
fn want_int(_: int) {}
fn main() {
    var b = IntBox { v: 1 };
    want_int(b.v);
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
fn inherited_member_wrong_target_still_flagged() {
    // After substitution `b.v: int`, passing it where `String` is
    // expected must still error — the fix substitutes, it doesn't widen.
    let src = "\
type Box<T> { v: T; }
type IntBox extends Box<int> {}
fn want_str(_: String) {}
fn main() {
    var b = IntBox { v: 1 };
    want_str(b.v);
}
";
    let (uri, pa) = analyze(src);
    assert_eq!(
        mismatches(&pa, &uri).len(),
        1,
        "got: {:?}",
        mismatches(&pa, &uri)
    );
}

#[test]
fn transitive_abstract_chain_with_node_wrapped_attr() {
    // Concrete -> SomeType -> Parent<int>, attr `node: node<T>` must
    // resolve to `node<int>` across two non-generic intermediate hops.
    let src = "\
abstract type Parent<T> { node: node<T>; }
abstract type SomeType extends Parent<int> {}
type Concrete extends SomeType {}
fn want_node_int(_: node<int>) {}
fn main() {
    var c = Concrete { node: node<int> { 42 } };
    want_node_int(c.node);
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
fn direct_generic_member_still_substitutes() {
    // Guard the pre-existing path: a direct `Box<int>` receiver still
    // substitutes from its own args.
    let src = "\
type Box<T> { v: T; }
fn want_int(_: int) {}
fn main() {
    var b = Box<int> { v: 1 };
    want_int(b.v);
}
";
    let (uri, pa) = analyze(src);
    assert!(
        mismatches(&pa, &uri).is_empty(),
        "got: {:?}",
        mismatches(&pa, &uri)
    );
}
