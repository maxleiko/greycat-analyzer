// P37.2 — `breakpoint;` lowers to `Stmt::Breakpoint`, a unit variant
// shaped like `Stmt::Break` / `Stmt::Continue`. The grammar rule is
// `breakpoint_stmt` and the lowering arm sits next to break/continue in
// `lower_stmt`.

use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::{Decl, Stmt};
use greycat_analyzer_syntax::parse;

#[test]
fn breakpoint_lowers_to_breakpoint_variant() {
    let src = "fn f() {\n    breakpoint;\n}\n";
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
    assert_eq!(stmts.len(), 1, "expected exactly one inner stmt");
    match &hir.stmts[stmts[0]] {
        Stmt::Breakpoint(_) => {}
        other => panic!("expected Stmt::Breakpoint, got {other:?}"),
    }
}
