// Grammar accepts a freestanding `expr_stmt` at module scope so doc
// snippets pretty-print under the same syntax highlighter as real
// modules. The HIR lowering must silently drop these — no `Decl` is
// emitted. Catches a regression where someone wires `expr_stmt` to a
// `Decl::Unsupported` (or similar) and accidentally leaks the snippet
// into downstream analysis.

use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_syntax::parse;

#[test]
fn top_level_expr_does_not_produce_a_decl() {
    let src = "foo();\n";
    let tree = parse(src);
    let symbols = SymbolTable::default();
    let hir = lower_module(src, &symbols, "module", "lib", tree.root_node());
    let module = hir.module.as_ref().expect("module lowered");
    assert!(
        module.decls.is_empty(),
        "expected no decls for top-level `foo();`, got {} decls",
        module.decls.len()
    );
}

#[test]
fn top_level_expr_mixed_with_real_decl_only_emits_the_decl() {
    let src = "foo();\nfn next() {}\n";
    let tree = parse(src);
    let symbols = SymbolTable::default();
    let hir = lower_module(src, &symbols, "module", "lib", tree.root_node());
    let module = hir.module.as_ref().expect("module lowered");
    assert_eq!(
        module.decls.len(),
        1,
        "expected only the `fn next()` decl, got {} decls",
        module.decls.len()
    );
}
