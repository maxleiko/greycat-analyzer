//! P13.3 — typed-suffix numeric literals lower to dedicated
//! [`LiteralKind`] variants instead of bare `Number`.

use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::{Decl, Expr, LiteralExpr, LiteralKind, Stmt};
use greycat_analyzer_syntax::parse;

fn first_var_init_kind(src: &str, idx: usize) -> LiteralKind {
    let tree = parse(src);
    let hir = lower_module(src, "module", "lib", tree.root_node());
    let module = hir.module.as_ref().expect("module lowered");
    let fn_decl = module
        .decls
        .iter()
        .find_map(|d| match &hir.decls[*d] {
            Decl::Fn(f) => Some(f),
            _ => None,
        })
        .expect("fn lowered");
    let body = hir.stmts[fn_decl.body.expect("body")].clone();
    let stmts = match body {
        Stmt::Block(s) => s,
        _ => panic!("expected block body"),
    };
    let stmt = &hir.stmts[stmts[idx]];
    let init = match stmt {
        Stmt::Var(v) => v.init.expect("init"),
        _ => panic!("expected var stmt"),
    };
    match &hir.exprs[init] {
        Expr::Literal(LiteralExpr { kind, .. }) => *kind,
        other => panic!("expected literal init, got {other:?}"),
    }
}

#[test]
fn time_suffix_lowers_to_time_kind() {
    let src = "fn f() {\n    var t = 100_time;\n}\n";
    assert_eq!(first_var_init_kind(src, 0), LiteralKind::Time);
}

#[test]
fn duration_unit_suffix_lowers_to_duration_kind() {
    let src = "fn f() {\n    var d = 5h_30m;\n}\n";
    assert_eq!(first_var_init_kind(src, 0), LiteralKind::Duration);
}

#[test]
fn float_suffix_stays_number_for_text_inspection() {
    // P13.3 keeps `_f` floats as `LiteralKind::Number`; the analyzer's
    // `numeric_literal_kind` text-inspector decides int-vs-float.
    let src = "fn f() {\n    var x = 1.5_f;\n}\n";
    assert_eq!(first_var_init_kind(src, 0), LiteralKind::Number);
}

#[test]
fn plain_number_stays_number() {
    let src = "fn f() {\n    var i = 42;\n}\n";
    assert_eq!(first_var_init_kind(src, 0), LiteralKind::Number);
}
