//! Regression: `(A | B) as A` is a valid downcast — the runtime
//! checks at use time and panics if the value happens to be `B`, but
//! statically the analyzer must accept the assertion. The previous
//! `is_castable` Union-source arm required EVERY alt to be castable
//! to the target (`alts.iter().all`), which rejected the common
//! "narrow a `??`-built union back to one of its members" pattern.
//!
//! Symptom in kopr: `var x = lhs.get(id) ?? rhs.get(id);` types
//! `x` as `node<L> | node<R> | null`. After the user has guarded
//! `x != null && !(x is R)`, writing `x as node<L>` got rejected
//! by the analyzer even though the runtime accepts the cast.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn cast_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("cannot cast"))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn cast_union_to_member_type_is_accepted() {
    // `Type1 | Type2` cast to `Type1` is the canonical narrow-back
    // pattern after a `??` build-up plus an `is` guard. There is no
    // explicit `A | B` syntax in GreyCat — unions surface from
    // `??` over two map lookups whose value types differ. Mirror
    // exactly that shape so the body walker actually materialises
    // a `Union` source TypeId.
    let mut mgr = SourceManager::new();
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "type A { x: int; }\n\
         type B { y: int; }\n\
         fn pick_a(): A? { return A { x: 1 }; }\n\
         fn pick_b(): B? { return null; }\n\
         fn take_a(a: A) {}\n\
         fn caller() {\n\
             var v = pick_a() ?? pick_b();\n\
             if (v != null) {\n\
                 take_a(v as A);\n\
             }\n\
         }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = cast_diagnostics(&pa, &main_uri);
    let all: Vec<String> = pa
        .module(&main_uri)
        .unwrap()
        .analysis
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect();
    assert!(
        diags.is_empty(),
        "(A | B) as A should be a valid cast, got cast diags: {:#?}\nall diags: {:#?}",
        diags,
        all,
    );
}

#[test]
fn cast_union_to_unrelated_type_still_rejected() {
    // Symmetry check: if no alt could possibly be the target, the
    // cast must still fail. `(A | B) as C` where C is unrelated to
    // both A and B is a real bug — the runtime would always panic.
    let mut mgr = SourceManager::new();
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "type A { x: int; }\n\
         type B { y: int; }\n\
         type C { z: int; }\n\
         fn pick_a(): A? { return A { x: 1 }; }\n\
         fn pick_b(): B? { return null; }\n\
         fn take_c(c: C) {}\n\
         fn caller() {\n\
             var v = pick_a() ?? pick_b();\n\
             if (v != null) {\n\
                 take_c(v as C);\n\
             }\n\
         }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = cast_diagnostics(&pa, &main_uri);
    assert!(
        !diags.is_empty(),
        "(A | B) as C must be rejected; analyzer let it through silently",
    );
}
