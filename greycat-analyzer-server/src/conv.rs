//! LSP <-> byte-offset conversions and small geometry helpers shared by
//! capability handlers. LSP positions are 0-indexed `(line, character)`;
//! the rest of the codebase treats `character` as a byte column (matching
//! tree-sitter's `Point.column`). All conversions go through this module
//! so the convention stays consistent.

use std::ops::Range;

use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::Stmt;
use lsp_types::Position;

pub(crate) fn position_to_byte(text: &str, pos: Position) -> usize {
    let mut line = 0u32;
    let mut byte = 0usize;
    for c in text.chars() {
        if line == pos.line {
            break;
        }
        byte += c.len_utf8();
        if c == '\n' {
            line += 1;
        }
    }
    // advance `character` byte columns, capping at next newline / EOF.
    let mut col = 0u32;
    let bytes = text.as_bytes();
    while col < pos.character && byte < bytes.len() {
        if bytes[byte] == b'\n' {
            break;
        }
        let c = text[byte..].chars().next().unwrap();
        byte += c.len_utf8();
        col += c.len_utf8() as u32;
    }
    byte
}

pub(crate) fn byte_to_position(text: &str, byte: usize) -> Position {
    let mut line = 0u32;
    let mut col = 0u32;
    let prefix = &text[..byte.min(text.len())];
    for c in prefix.chars() {
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += c.len_utf8() as u32;
        }
    }
    Position {
        line,
        character: col,
    }
}

pub(crate) fn byte_range_to_lsp(text: &str, range: &Range<usize>) -> lsp_types::Range {
    lsp_types::Range {
        start: byte_to_position(text, range.start),
        end: byte_to_position(text, range.end),
    }
}

pub(crate) fn ranges_overlap(a: &lsp_types::Range, b: &lsp_types::Range) -> bool {
    !(a.end.line < b.start.line
        || a.start.line > b.end.line
        || (a.end.line == b.start.line && a.end.character < b.start.character)
        || (a.start.line == b.end.line && a.start.character > b.end.character))
}

pub(crate) fn stmt_byte_range(hir: &Hir, stmt_id: Idx<Stmt>) -> Range<usize> {
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
        HS::Return(Some(e)) => hir.exprs[*e].byte_range(),
        HS::Throw(e) => hir.exprs[*e].byte_range(),
        HS::Return(None) | HS::Break | HS::Continue | HS::Breakpoint => 0..0,
    }
}
