//! Thin converter from the IDE-shape `analysis::ide::document_symbols`
//! ADT to `lsp_types::DocumentSymbol`. Production logic lives in the
//! analysis crate so the same tree is reachable from non-LSP consumers
//! (workspace symbols, wasm bridge).

use greycat_analyzer_analysis::ide::document_symbols::{
    DocumentSymbol as IdeDocumentSymbol, SymbolKind as IdeSymbolKind,
    document_symbols as document_symbols_inner,
};
use greycat_analyzer_analysis::ide::types::{Position as IdePosition, Range as IdeRange};
use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{DocumentSymbol, Position, Range, SymbolKind};

pub fn document_symbols(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    encoding: SourceEncoding,
) -> Vec<DocumentSymbol> {
    document_symbols_inner(text, lib, root, encoding)
        .into_iter()
        .map(to_lsp)
        .collect()
}

pub(super) fn to_lsp(sym: IdeDocumentSymbol) -> DocumentSymbol {
    #[allow(deprecated)]
    DocumentSymbol {
        name: sym.name,
        detail: None,
        kind: kind_to_lsp(sym.kind),
        tags: None,
        deprecated: None,
        range: range_to_lsp(sym.range),
        selection_range: range_to_lsp(sym.selection_range),
        children: if sym.children.is_empty() {
            None
        } else {
            Some(sym.children.into_iter().map(to_lsp).collect())
        },
    }
}

fn kind_to_lsp(kind: IdeSymbolKind) -> SymbolKind {
    match kind {
        IdeSymbolKind::Function => SymbolKind::FUNCTION,
        IdeSymbolKind::Class => SymbolKind::CLASS,
        IdeSymbolKind::Enum => SymbolKind::ENUM,
        IdeSymbolKind::Variable => SymbolKind::VARIABLE,
        IdeSymbolKind::Key => SymbolKind::KEY,
        IdeSymbolKind::Field => SymbolKind::FIELD,
        IdeSymbolKind::Method => SymbolKind::METHOD,
    }
}

fn range_to_lsp(r: IdeRange) -> Range {
    Range {
        start: pos_to_lsp(r.start),
        end: pos_to_lsp(r.end),
    }
}

fn pos_to_lsp(p: IdePosition) -> Position {
    Position {
        line: p.line,
        character: p.character,
    }
}
