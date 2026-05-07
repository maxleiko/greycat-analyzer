use wasm_bindgen::prelude::*;

/// Parse `source` and return the s-expression of the resulting tree-sitter
/// CST. Cheap, lossless surface for the playground while richer wasm bindings
/// (HIR, diagnostics, formatter) are still being built.
#[wasm_bindgen]
pub fn parse_sexp(source: &str) -> String {
    greycat_analyzer_syntax::parse(source).root_node().to_sexp()
}
