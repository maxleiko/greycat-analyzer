//! Thin converter from `analysis::ide::document_highlights` ADT to
//! `lsp_types::DocumentHighlight`.

use greycat_analyzer_analysis::ide::document_highlights::{
    DocumentHighlight as IdeDocumentHighlight, DocumentHighlightKind as IdeDocumentHighlightKind,
    document_highlights as document_highlights_inner,
};
use greycat_analyzer_analysis::ide::types::{Position as IdePosition, Range as IdeRange};
use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{DocumentHighlight, DocumentHighlightKind, Position, Range};

pub fn document_highlights(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    encoding: SourceEncoding,
) -> Vec<DocumentHighlight> {
    document_highlights_inner(text, root, pos, encoding)
        .into_iter()
        .map(to_lsp)
        .collect()
}

fn to_lsp(h: IdeDocumentHighlight) -> DocumentHighlight {
    DocumentHighlight {
        range: range_to_lsp(h.range),
        kind: Some(match h.kind {
            IdeDocumentHighlightKind::Text => DocumentHighlightKind::TEXT,
            IdeDocumentHighlightKind::Read => DocumentHighlightKind::READ,
            IdeDocumentHighlightKind::Write => DocumentHighlightKind::WRITE,
        }),
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
