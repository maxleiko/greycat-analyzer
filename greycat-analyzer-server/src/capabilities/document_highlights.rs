//! Document highlight handler — identifier occurrences in the same file.

use greycat_analyzer_syntax::cst::{node_at_offset, walk_named};
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{DocumentHighlight, DocumentHighlightKind, Position};

use crate::conv::{byte_range_to_lsp, position_to_byte};

pub fn document_highlights(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
) -> Vec<DocumentHighlight> {
    let byte = position_to_byte(text, pos);
    let Some(node) = node_at_offset(root, byte) else {
        return Vec::new();
    };
    if node.kind() != "ident" {
        return Vec::new();
    }
    let target_text = text.get(node.byte_range()).unwrap_or("").to_string();
    if target_text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    walk_named(root, |n| {
        if n.kind() == "ident" && text.get(n.byte_range()).unwrap_or("") == target_text {
            out.push(DocumentHighlight {
                range: byte_range_to_lsp(text, &n.byte_range()),
                kind: Some(DocumentHighlightKind::TEXT),
            });
        }
        true
    });
    out
}
