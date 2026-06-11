//! Cross-callee-shape generic inference, with the `typeof T` extension.
//!
//! The user-visible bug that motivated this file: a stdlib call like
//! `type::enum_by_name(DurationUnit, "milliseconds")` reported the
//! local var binding as `T?` (raw, unsubstituted), so inlay hints and
//! `dump-types` rendered the unrefined generic name instead of the
//! concrete `DurationUnit?`. The fix wires generic inference through
//! the Static / Member / Arrow / QualifiedStatic callee shapes (they
//! used to skip witness collection entirely) and teaches witness
//! collection about `typeof T` parameters paired with type-literal
//! arguments.
//!
//! These tests pin the inferred binding type for the call's enclosing
//! `var` so future refactors can't silently regress the substitution.

use greycat_analyzer_analysis::display::display_type;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_hir::types::{Decl, Stmt};
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

/// Look up the inferred type of the first `var <name>` declaration in
/// the module's first fn body.
fn var_type(pa: &ProjectAnalysis, uri: &Uri, var_name: &str) -> String {
    let m = pa.module(uri).expect("module");
    let arena = pa.arena();
    let decl_registry = pa.decl_registry();
    let symbols = pa.symbols();
    for (_decl_id, decl) in m.hir.decls.iter() {
        let Decl::Fn(fnd) = decl else { continue };
        let Some(body) = fnd.body else { continue };
        let Stmt::Block(block) = &m.hir.stmts[body] else {
            continue;
        };
        for stmt_id in block.stmts.iter() {
            let Stmt::Var(lv) = &m.hir.stmts[*stmt_id] else {
                continue;
            };
            if symbols[m.hir.idents[lv.name].symbol] != *var_name {
                continue;
            }
            let ty = m
                .analysis
                .def_types
                .get(&lv.name)
                .copied()
                .expect("def_type for the var");
            return display_type(arena, decl_registry, symbols, ty).to_string();
        }
    }
    panic!("var `{var_name}` not found in any fn body");
}

/// Minimal in-project stdlib stub. The tests use locally-declared
/// generic functions / types so the project closure stays single-
/// module (`SourceManager::add_simple` doesn't load stdlib). Each
/// `native` declaration mirrors the shape of the stdlib function it
/// stands in for.
const STUB: &str = r#"
native fn min_g<T>(a: T, b: T): T;
native fn max_g<T>(a: T, b: T): T;
native fn abs_g<T>(a: T): T;
type type_stub {
    native static fn enum_by_name_stub<T>(et: typeof T, name: String): T?;
}
"#;

#[test]
fn typeof_t_call_binds_t_from_type_literal_arg() {
    // The user-reported scenario, distilled. A static generic method
    // `<T>(et: typeof T, name: String): T?` paired with a bare type-
    // ident argument should bind `T` to that type and return the
    // substituted shape.
    let mut mgr = SourceManager::new();
    let src = format!(
        "{STUB}enum Color {{ red; green; blue; }}
fn check() {{
    var x = type_stub::enum_by_name_stub(Color, \"red\");
}}
"
    );
    let uri = add(&mut mgr, "/proj/main.gcl", &src);
    let pa = ProjectAnalysis::analyze(&mgr);
    let ty = var_type(&pa, &uri, "x");
    assert_eq!(
        ty, "Color?",
        "expected `Color?` from typeof witness binding, got `{ty}`"
    );
}

#[test]
fn freestanding_generic_binds_t_from_value_arg() {
    // Regression guard for the unchanged bare-Ident generic-inference
    // path. `<T>(a: T, b: T): T` should still bind `T := int` from
    // the `42` argument.
    let mut mgr = SourceManager::new();
    let src = format!(
        "{STUB}fn other(): int {{ return 7; }}
fn check() {{
    var n = min_g(42, other());
}}
"
    );
    let uri = add(&mut mgr, "/proj/main.gcl", &src);
    let pa = ProjectAnalysis::analyze(&mgr);
    let ty = var_type(&pa, &uri, "n");
    assert_eq!(ty, "int", "min_g(42, int) should bind T := int, got `{ty}`");
}

#[test]
fn freestanding_generic_binds_t_from_float_args() {
    let mut mgr = SourceManager::new();
    let src = format!(
        "{STUB}fn check() {{
    var m = max_g(3.14, 2.71);
}}
"
    );
    let uri = add(&mut mgr, "/proj/main.gcl", &src);
    let pa = ProjectAnalysis::analyze(&mgr);
    let ty = var_type(&pa, &uri, "m");
    assert_eq!(
        ty, "float",
        "max_g(float, float) should bind T := float, got `{ty}`"
    );
}

#[test]
fn typeof_t_call_binds_t_from_fqn_type_literal_arg() {
    // FQN form of [`typeof_t_call_binds_t_from_type_literal_arg`].
    // `module::EnumName` lowers to `Expr::Static` (the parser's
    // 2-segment `module::name` shape), which falls through to
    // `qualified_static_value_type`. That helper had to learn the
    // typeof refinement too, or the bare-ident fix above would
    // silently regress for users who reach for the FQN form.
    let mut mgr = SourceManager::new();
    // Two modules: an enum lives in the `other` lib, the user calls
    // the stub static via `other::Color` from `main.gcl`.
    let _other_uri = add(
        &mut mgr,
        "/proj/other.gcl",
        "enum Color { red; green; blue; }\n",
    );
    let main_src = format!(
        "{STUB}fn check() {{
    var x = type_stub::enum_by_name_stub(other::Color, \"red\");
}}
"
    );
    let main_uri = add(&mut mgr, "/proj/main.gcl", &main_src);
    let pa = ProjectAnalysis::analyze(&mgr);
    let ty = var_type(&pa, &main_uri, "x");
    assert_eq!(
        ty, "Color?",
        "FQN type-literal arg should also bind T, got `{ty}`"
    );
}

#[test]
fn freestanding_generic_binds_t_from_int_arg() {
    let mut mgr = SourceManager::new();
    let src = format!(
        "{STUB}fn check() {{
    var a = abs_g(-1);
}}
"
    );
    let uri = add(&mut mgr, "/proj/main.gcl", &src);
    let pa = ProjectAnalysis::analyze(&mgr);
    let ty = var_type(&pa, &uri, "a");
    assert_eq!(ty, "int", "abs_g(int) should bind T := int, got `{ty}`");
}
