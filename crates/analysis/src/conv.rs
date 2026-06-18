//! LSP ↔ byte-offset conversions shared by capability handlers and
//! ide services. The encoding-aware position / range math lives in
//! [`greycat_analyzer_core::conv`]; this module re-exports it together
//! with the analysis-side `stmt_byte_range` helper.

use std::ops::Range;

pub use greycat_analyzer_core::conv::{
    byte_range_to_lsp, byte_to_position, position_to_byte, ranges_overlap,
};

use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::hir::Stmt;

pub fn stmt_byte_range(hir: &Hir, stmt_id: Idx<Stmt>) -> Range<usize> {
    use Stmt as HS;
    match &hir.stmts[stmt_id] {
        HS::Block(b) => b.byte_range.clone(),
        HS::Var(s) => s.byte_range.clone(),
        HS::Assign(s) => s.byte_range.clone(),
        HS::If(s) => s.byte_range.clone(),
        HS::While(s) => s.byte_range.clone(),
        HS::DoWhile(s) => s.byte_range.clone(),
        HS::For(s) => s.byte_range.clone(),
        HS::ForIn(s) => s.byte_range.clone(),
        HS::Try(s) => s.byte_range.clone(),
        HS::At(s) => s.byte_range.clone(),
        HS::Expr(e) => hir.exprs[*e].byte_range(),
        HS::Return(r) => r.byte_range.clone(),
        HS::Break(b) => b.byte_range.clone(),
        HS::Continue(c) => c.byte_range.clone(),
        HS::Breakpoint(b) => b.byte_range.clone(),
        HS::Throw(t) => t.byte_range.clone(),
    }
}
