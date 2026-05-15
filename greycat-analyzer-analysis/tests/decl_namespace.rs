//! Regression tests for the three-namespace split in `duplicate-decl`
//! / `ambiguous-symbol`.
//!
//! The GreyCat runtime (validated against `greycat build` 8.0.301-dev)
//! keeps THREE top-level name slots that may all share an identifier:
//!
//! - Type-namespace: `Decl::Type` + `Decl::Enum`
//! - Fn-namespace:   `Decl::Fn`
//! - Var-namespace:  `Decl::Var` (module-level — graph-root nodes)
//!
//! Runtime probe outcomes:
//!
//! - Cross-namespace pairs (type+fn, type+var, enum+var, fn+var, all
//!   three together) → build clean.
//! - In-namespace pairs (two fns, two vars, two types, two enums,
//!   type+enum) → `syntax error … already declared`.

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_analysis::resolver::Definition;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_hir::types::{Decl, Expr};
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn duplicate_decl_count(pa: &ProjectAnalysis, uri: &Uri) -> usize {
    pa.module(uri)
        .expect("module")
        .lints
        .iter()
        .filter(|l| l.rule == "duplicate-decl")
        .count()
}

fn ambiguous_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error && d.message.contains("ambiguous-symbol"))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn same_module_type_and_fn_share_name_no_duplicate() {
    let mut mgr = SourceManager::new();
    let uri = add(&mut mgr, "/proj/src/a.gcl", "type geo {}\nfn geo() {}\n");
    let pa = ProjectAnalysis::analyze(&mgr);
    assert_eq!(
        duplicate_decl_count(&pa, &uri),
        0,
        "type/fn collision is valid GCL — duplicate-decl must not fire; got lints: {:#?}",
        pa.module(&uri).unwrap().lints,
    );
}

#[test]
fn same_module_type_and_enum_share_name_flags() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/src/a.gcl",
        "type geo {}\nenum geo { A, B }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert_eq!(
        duplicate_decl_count(&pa, &uri),
        1,
        "type+enum both live in the type-namespace — must still flag",
    );
}

#[test]
fn same_module_fn_and_var_share_name_no_duplicate() {
    // Runtime probe (8.0.301-dev): `fn Foo()` + `var Foo: node<int?>;`
    // in the same module builds clean — fn-namespace and var-namespace
    // are independent.
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/src/a.gcl",
        "fn geo() {}\nvar geo: node<int?>;\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert_eq!(
        duplicate_decl_count(&pa, &uri),
        0,
        "fn-ns and var-ns are independent — must not flag; got lints: {:#?}",
        pa.module(&uri).unwrap().lints,
    );
}

#[test]
fn same_module_two_vars_share_name_flags() {
    // Negative control: two module-vars sharing a name remain a real
    // collision (runtime: "module variable name conflict").
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/src/a.gcl",
        "var geo: node<int?>;\nvar geo: node<int?>;\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert_eq!(
        duplicate_decl_count(&pa, &uri),
        1,
        "two vars sharing a name is a real var-ns collision",
    );
}

#[test]
fn cross_module_type_and_fn_no_ambiguous_and_resolve_correctly() {
    let mut mgr = SourceManager::new();
    let a_uri = add(&mut mgr, "/proj/src/a.gcl", "type geo {}\n");
    let b_uri = add(
        &mut mgr,
        "/proj/src/b.gcl",
        "fn geo(x: int): int { return x; }\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn caller() {
    var n = geo(1);
    var t: geo;
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert!(
        ambiguous_diagnostics(&pa, &main_uri).is_empty(),
        "type-ns + value-ns collision across modules must not be ambiguous: {:#?}",
        ambiguous_diagnostics(&pa, &main_uri),
    );

    let m = pa.module(&main_uri).expect("main module");

    // Value-position bare `geo` (the callee of `geo(1)`) must bind to
    // b.gcl's `fn geo`.
    let value_binding = m
        .hir
        .exprs
        .iter()
        .find_map(|(_, expr)| match expr {
            Expr::Call(c) => match &m.hir.exprs[c.callee] {
                Expr::Ident { name, .. } if pa.symbol(&m.hir.idents[*name].symbol) == "geo" => {
                    m.resolutions.lookup(*name)
                }
                _ => None,
            },
            _ => None,
        })
        .expect("expected resolver binding for value-position `geo`");
    match value_binding {
        Definition::ProjectDecl { uri, decl } => {
            assert_eq!(
                uri, b_uri,
                "value-position `geo` should bind to b.gcl's fn, got {uri:?}",
            );
            let foreign = pa.module(&uri).unwrap();
            assert!(
                matches!(foreign.hir.decls[decl], Decl::Fn(_)),
                "expected Decl::Fn binding, got {:?}",
                foreign.hir.decls[decl],
            );
        }
        other => panic!(
            "value-position `geo` should be ProjectDecl, got {:?}",
            other
        ),
    }

    // Type-position bare `geo` (the annotation on `var t: geo`) must
    // bind to a.gcl's `type geo`.
    let type_binding = m
        .hir
        .type_refs
        .iter()
        .find_map(|(_, ty)| {
            if ty.qualifier.is_empty() && pa.symbol(&m.hir.idents[ty.name].symbol) == "geo" {
                m.resolutions.lookup(ty.name)
            } else {
                None
            }
        })
        .expect("expected resolver binding for type-position `geo`");
    match type_binding {
        Definition::ProjectDecl { uri, decl } => {
            assert_eq!(
                uri, a_uri,
                "type-position `geo` should bind to a.gcl's type, got {uri:?}",
            );
            let foreign = pa.module(&uri).unwrap();
            assert!(
                matches!(foreign.hir.decls[decl], Decl::Type(_)),
                "expected Decl::Type binding, got {:?}",
                foreign.hir.decls[decl],
            );
        }
        other => panic!("type-position `geo` should be ProjectDecl, got {:?}", other),
    }
}

#[test]
fn cross_module_two_fns_still_ambiguous() {
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/a.gcl", "fn geo() {}\n");
    add(&mut mgr, "/proj/src/b.gcl", "fn geo() {}\n");
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn caller() {
    geo();
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = ambiguous_diagnostics(&pa, &main_uri);
    assert_eq!(
        diags.len(),
        1,
        "two value-namespace `fn geo` decls in two modules must still flag ambiguous-symbol: {:#?}",
        diags,
    );
}
