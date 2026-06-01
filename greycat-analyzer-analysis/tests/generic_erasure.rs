//! `generic-erasure` — the analyzer flags uses of a generic fn's result
//! that the GreyCat runtime throws on. The runtime erases function-level
//! generic parameters to `any?` (verified against `greycat run`
//! 8.0.372-dev), so a fn that constructs & returns `Container<T>` yields
//! `Container<any?>` at runtime, which is not assignable to a more-
//! specifically-parameterized parameter / field / return. Each behavior
//! asserted here was checked against the real runtime.
//!
//! Firing cases use a user-defined `Box<T>` (hermetic — built-in `Array`
//! only resolves with the stdlib loaded). The node-family self-exclusion
//! uses the runtime-seeded `node` (a `u64` handle whose type arg the
//! runtime never checks — node-tag bivariance accepts the erased form).

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

/// Run the project pipeline on a fixed prelude + `extra`, returning the
/// `generic-erasure` diagnostic messages.
fn erasure_diags(extra: &str) -> Vec<String> {
    let mut src = String::from(
        "type Person {}\n\
         type Box<T> { item: T; }\n\
         fn wrap<T>(x: T): Box<T> { return Box<T> { item: x }; }\n",
    );
    src.push_str(extra);
    let uri = Uri::from_str("file:///m.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(uri.clone(), &src, "project", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    pa.module(&uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.code == "generic-erasure")
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn arg_position_fires() {
    let d = erasure_diags(
        "fn needs(b: Box<Person>): int { return 0; }\n\
         fn main() { needs(wrap(Person {})); }\n",
    );
    assert_eq!(d.len(), 1, "expected one generic-erasure: {d:?}");
}

#[test]
fn via_variable_fires() {
    // The common shape: result stored in a var, then used. Taint must
    // propagate through the `var` binding.
    let d = erasure_diags(
        "fn needs(b: Box<Person>): int { return 0; }\n\
         fn main() { var b = wrap(Person {}); needs(b); }\n",
    );
    assert_eq!(d.len(), 1, "expected one generic-erasure: {d:?}");
}

#[test]
fn field_init_fires() {
    let d = erasure_diags(
        "type Holder { it: Box<Person>; }\n\
         fn main() { var h = Holder { it: wrap(Person {}) }; }\n",
    );
    assert_eq!(d.len(), 1, "expected one generic-erasure: {d:?}");
}

#[test]
fn return_position_fires() {
    let d = erasure_diags(
        "fn get(): Box<Person> { return wrap(Person {}); }\n\
         fn main() { var b = get(); }\n",
    );
    assert_eq!(d.len(), 1, "expected one generic-erasure: {d:?}");
}

#[test]
fn var_annotation_does_not_fire() {
    // `var b: Box<Person> = wrap(..)` is accepted by the runtime (the
    // annotation is cosmetic; the tag stays erased). No use downstream →
    // nothing throws → no diagnostic.
    let d = erasure_diags("fn main() { var b: Box<Person> = wrap(Person {}); }\n");
    assert!(d.is_empty(), "var annotation must not fire: {d:?}");
}

#[test]
fn widening_to_any_does_not_fire() {
    // Erased `Box<any?>` flows into a `Box<any?>` slot — assignable, no
    // throw.
    let d = erasure_diags(
        "fn takes_any(b: Box<any?>): int { return 0; }\n\
         fn main() { takes_any(wrap(Person {})); }\n",
    );
    assert!(d.is_empty(), "widening to any? must not fire: {d:?}");
}

#[test]
fn passthrough_does_not_fire() {
    // `id` forwards its param unchanged — honored, no new erasure.
    let d = erasure_diags(
        "fn id<T>(x: Box<T>): Box<T> { return x; }\n\
         fn needs(b: Box<Person>): int { return 0; }\n\
         fn main() { var b = Box<Person> { item: Person {} }; needs(id(b)); }\n",
    );
    assert!(d.is_empty(), "pass-through must not fire: {d:?}");
}

#[test]
fn bare_generic_return_does_not_fire() {
    // `first` returns bare `T` — the value keeps its real class.
    let d = erasure_diags(
        "fn first<T>(b: Box<T>): T { return b.item; }\n\
         fn needs(p: Person): int { return 0; }\n\
         fn main() { var b = Box<Person> { item: Person {} }; needs(first(b)); }\n",
    );
    assert!(d.is_empty(), "bare-T return must not fire: {d:?}");
}

#[test]
fn node_family_self_excludes() {
    // node is a `u64` handle — the runtime never checks its type arg, so
    // the erased `node` flows into any `node<T>` slot. Classified as
    // erasing, but the assignability check (node-tag bivariance) accepts
    // it, so no diagnostic.
    let d = erasure_diags(
        "fn wrapnode<T>(x: T): node<T> { return node<T> { x }; }\n\
         fn needs(n: node<Person>): int { return 0; }\n\
         fn main() { needs(wrapnode(Person {})); }\n",
    );
    assert!(d.is_empty(), "node-family must self-exclude: {d:?}");
}
