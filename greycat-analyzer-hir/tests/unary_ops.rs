//! `++` / `--` / unary `+` lower to dedicated `UnaryOp` variants
//! (not the `Not` wildcard fallback). Regression test for
//! `fix(hir): lower ++ / -- / + as Inc/Dec/Pos, not Not`.

use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::{Decl, Expr, Stmt, UnaryExpr, UnaryOp};
use greycat_analyzer_syntax::parse;

fn first_stmt_unary_op(src: &str) -> UnaryOp {
    let tree = parse(src);
    let symbols = SymbolTable::default();
    let hir = lower_module(src, &symbols, "module", "lib", tree.root_node());
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
        Stmt::Block(b) => b.stmts,
        _ => panic!("expected block body"),
    };
    // The first stmt is `var x = 0;` — the unary expr stmt is at idx 1.
    let stmt = &hir.stmts[stmts[1]];
    let expr_id = match stmt {
        Stmt::Expr(idx) => *idx,
        other => panic!("expected expr stmt, got {other:?}"),
    };
    match &hir.exprs[expr_id] {
        Expr::Unary(UnaryExpr { op, .. }) => *op,
        other => panic!("expected unary expr, got {other:?}"),
    }
}

#[test]
fn prefix_increment_lowers_to_inc() {
    let src = "fn f() {\n    var x = 0;\n    ++x;\n}\n";
    assert_eq!(first_stmt_unary_op(src), UnaryOp::Inc);
}

#[test]
fn postfix_increment_lowers_to_inc() {
    let src = "fn f() {\n    var x = 0;\n    x++;\n}\n";
    assert_eq!(first_stmt_unary_op(src), UnaryOp::Inc);
}

#[test]
fn prefix_decrement_lowers_to_dec() {
    let src = "fn f() {\n    var x = 0;\n    --x;\n}\n";
    assert_eq!(first_stmt_unary_op(src), UnaryOp::Dec);
}

#[test]
fn postfix_decrement_lowers_to_dec() {
    let src = "fn f() {\n    var x = 0;\n    x--;\n}\n";
    assert_eq!(first_stmt_unary_op(src), UnaryOp::Dec);
}

#[test]
fn unary_plus_lowers_to_pos() {
    let src = "fn f() {\n    var x = 0;\n    +x;\n}\n";
    assert_eq!(first_stmt_unary_op(src), UnaryOp::Pos);
}

#[test]
fn unary_minus_lowers_to_neg() {
    let src = "fn f() {\n    var x = 0;\n    -x;\n}\n";
    assert_eq!(first_stmt_unary_op(src), UnaryOp::Neg);
}

#[test]
fn unary_not_lowers_to_not() {
    let src = "fn f() {\n    var x = true;\n    !x;\n}\n";
    assert_eq!(first_stmt_unary_op(src), UnaryOp::Not);
}
