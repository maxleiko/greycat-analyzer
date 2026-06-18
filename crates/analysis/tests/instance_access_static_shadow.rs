//! Regression tests for `fix(analysis): attrs win over methods;
//! instance access skips static`.
//!
//! Before the fix, `resolve_member` returned on the first match in the
//! local type's methods. A `type Sub extends Super { static fn name(...)
//! {} }` then shadowed the inherited `name: time` attr — `this.name`
//! resolved to the static method (typed as `function`), surfacing
//! "value of `function` not assignable to `time`".
//!
//! The fix walks attrs across the chain first (local then inherited),
//! then methods; and filters `static` candidates out of instance
//! access so an inherited non-static method isn't shadowed by a local
//! static of the same name.

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

fn assignability_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("not assignable"))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn inherited_attr_wins_over_local_static_method() {
    // `Sub` has a `static fn from(): int` AND inherits `from: time`
    // from `Base`. Instance access `this.from` must bind to the attr.
    let src = "\
abstract type Base { from: time; }
type Sub extends Base {
    fn use_self(): time { return this.from; }
    static fn from(): int { return 99; }
}
fn main() {}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics, got: {diags:?}"
    );
}

#[test]
fn local_attr_still_wins_over_local_instance_method() {
    // Belt-and-suspenders for the local case: a local non-static
    // method shouldn't shadow a same-named local attr.
    let src = "\
type Box {
    value: int;
    fn use_self(): int { return this.value; }
    fn value(): String { return \"\"; }
}
fn main() {}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics, got: {diags:?}"
    );
}

#[test]
fn static_access_when_no_attr_conflict_resolves_to_method() {
    // When the static method's name does NOT collide with an
    // inherited attr, `Type::method` resolves as a function ref
    // (typed `function`). This pins down that the static-access
    // path still walks methods.
    let src = "\
type Holder {
    static fn make(): int { return 7; }
}
fn takesFn(f: function) {}
fn main() {
    takesFn(Holder::make);
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics (Holder::make is a fn ref), got: {diags:?}"
    );
}

#[test]
fn inherited_non_static_method_resolves_when_local_has_only_attr() {
    // Sanity check: when the local type has neither attr nor method
    // named X, `resolve_member` should walk the chain for methods
    // after attrs. An inherited non-static method must resolve.
    let src = "\
abstract type Base { fn greet(): String { return \"hi\"; } }
type Sub extends Base {}
fn takesString(s: String) {}
fn main() {
    var s = Sub {};
    takesString(s.greet());
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics, got: {diags:?}"
    );
}
