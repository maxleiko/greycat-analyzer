//! Regression: when two modules each declare a `private fn` of the
//! same name with different signatures, body-typing of a call inside
//! one module must use that module's own private decl — not the other
//! module's signature picked up by the project-wide `fn_signatures`
//! name index.
//!
//! Symptom in a real project (kopr): two modules each declare a
//! `private fn process_index(...): Array<MyLocalView>` with a
//! different return type. Calls inside module A get typed as if they
//! returned module B's `Array<OtherView>`, producing "not assignable"
//! diagnostics that disappear if either function is renamed. The
//! resolver binds the call's callee Ident correctly to the local
//! `Decl`; the bug lives in the body walker's `bare_fn_return`, which
//! always preferred `index.fn_signature_for(name)` (first-decl-wins
//! across the whole project) over the resolver's binding.
//!
//! `private fn` names are NOT in the project's public namespace, so a
//! cross-module name lookup must never satisfy a same-name lookup
//! from a different module.

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

#[test]
fn same_named_private_fns_do_not_steal_each_others_signatures() {
    // Two modules each declare `private fn helper(): T` with different
    // T. Each module also has a top-level fn that consumes the
    // local helper's result through a typed-parameter call. If the
    // body walker substitutes the foreign signature, the consumer in
    // one of the two modules will see "Array<X> not assignable to
    // Array<Y>" — exactly the kopr symptom.
    let mut mgr = SourceManager::new();
    let a_uri = add(
        &mut mgr,
        "/proj/src/a.gcl",
        "type LocalA { x: int; }\n\
         private fn build(): Array<LocalA> { return Array<LocalA> {}; }\n\
         fn consume(arr: Array<LocalA>) {}\n\
         fn caller_a() {\n    consume(build());\n}\n",
    );
    let b_uri = add(
        &mut mgr,
        "/proj/src/b.gcl",
        "type LocalB { y: int; }\n\
         private fn build(): Array<LocalB> { return Array<LocalB> {}; }\n\
         fn consume(arr: Array<LocalB>) {}\n\
         fn caller_b() {\n    consume(build());\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let a_diags = assignability_diagnostics(&pa, &a_uri);
    let b_diags = assignability_diagnostics(&pa, &b_uri);
    assert!(
        a_diags.is_empty(),
        "module a should have no assignability errors, got: {:#?}",
        a_diags
    );
    assert!(
        b_diags.is_empty(),
        "module b should have no assignability errors, got: {:#?}",
        b_diags
    );
}

#[test]
fn same_named_private_fns_with_overload_consumer_param() {
    // Closer to the kopr shape: each module declares a `private fn`
    // returning a different concrete type. The caller passes the
    // result to a typed-parameter call. If the body walker substitutes
    // the foreign signature, the consumer in one of the two modules
    // will report "value of type `OtherView` is not assignable to
    // parameter ... `LocalView`" — exactly the kopr symptom.
    let mut mgr = SourceManager::new();
    let a_uri = add(
        &mut mgr,
        "/proj/src/maps.gcl",
        "type MapElementView { id: int; }\n\
         private fn make_view(): MapElementView { return MapElementView { id: 1 }; }\n\
         fn consume(v: MapElementView) {}\n\
         fn entry() {\n    consume(make_view());\n}\n",
    );
    let b_uri = add(
        &mut mgr,
        "/proj/src/sld.gcl",
        "type SldElementView { id: int; }\n\
         private fn make_view(): SldElementView { return SldElementView { id: 1 }; }\n\
         fn consume(v: SldElementView) {}\n\
         fn entry() {\n    consume(make_view());\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let a_diags = assignability_diagnostics(&pa, &a_uri);
    let b_diags = assignability_diagnostics(&pa, &b_uri);
    assert!(
        a_diags.is_empty(),
        "maps.gcl should accept its own private `make_view()`, got: {:#?}",
        a_diags
    );
    assert!(
        b_diags.is_empty(),
        "sld.gcl should accept its own private `make_view()`, got: {:#?}",
        b_diags
    );
}
