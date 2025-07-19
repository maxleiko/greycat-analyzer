#![allow(unused)] // TODO REMOVE THIS ONCE STABLE

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
    filename: &str,
    source: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), Box<dyn std::error::Error>> {
    todo!()
}
