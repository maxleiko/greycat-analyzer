//! Regression: source-side raw-form generic must NOT flow into a
//! concrete-arg target. The `Foo<any?, any?>` shape is the runtime's
//! *target-only* raw-form acceptance sink — using it on the source
//! side and expecting the args to widen into a concrete `Foo<int, T>`
//! is rejected by the runtime:
//!
//!   abstract type AbstractType {}
//!   fn main() { stats(Tuple<any?, any?> {}); }
//!   fn stats(result: Tuple<int, AbstractType>) {}
//!
//! produces `argument of type 'Tuple' is not assignable to parameter
//! of type 'Tuple<int, AbstractType>'` at runtime. Pre-fix, core's
//! `is_assignable_to` performed a bidirectional invariance check on
//! each arg pair, which let P20.1's any-as-bottom rule fire on the
//! source side and falsely passed `(any?, int)` as structurally
//! equal. Fix: strict TypeId equality for generic-arg invariance.
//!
//! Uses a user-defined `Pair<A, B>` to keep the fixture independent
//! of stdlib seeding (built-in `Tuple` requires the real stdlib).

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn assignability_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("not assignable"))
        .map(|d| d.message.clone())
        .collect()
}

const TYPES_SRC: &str = "\
abstract type AbstractType {}\n\
type Pair<A, B> {\n\
    a: A;\n\
    b: B;\n\
}\n\
";

#[test]
fn raw_form_pair_source_to_concrete_pair_rejected() {
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/types.gcl", TYPES_SRC);
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn stats(result: Pair<int, AbstractType>) {}\n\
         fn main() {\n    stats(Pair<any?, any?> {});\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        !diags.is_empty(),
        "Pair<any?, any?> source MUST NOT flow into Pair<int, AbstractType> target; \
         got no diagnostics"
    );
}

#[test]
fn concrete_pair_to_raw_form_pair_target_allowed() {
    // Mirror direction — target raw-form accepts any concrete
    // instantiation (the all-Any-target wildcard rule).
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/types.gcl", TYPES_SRC);
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn sink(p: Pair<any?, any?>) {}\n\
         fn main() {\n    sink(Pair<int, AbstractType> { a: 1, b: AbstractType {} });\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "Pair<int, AbstractType> source MUST flow into Pair<any?, any?> target \
         (target-side raw-form acceptance); got: {diags:?}"
    );
}
