mod ast;
pub mod cst;
mod lexer;
pub mod span;
// #[cfg(feature = "wasm")]
// mod wasm;

pub use lexer::*;
use lsp_types::Diagnostic;

// TODO move this to HIR
#[allow(clippy::ptr_arg)]
pub fn parse(
    _filename: &str,
    _source: &str,
    _diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), Box<dyn std::error::Error>> {
    todo!()
}
