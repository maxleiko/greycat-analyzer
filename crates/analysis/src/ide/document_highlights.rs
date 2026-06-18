//! Document highlights — identifier occurrences in a single file. The
//! current pass only emits `Text`-kind highlights (same-spelling
//! matches); editor read/write differentiation lives in a future
//! analysis stage.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_core::lsp_types::Position;
use greycat_analyzer_syntax::cst::{node_at_offset, walk_named};
use greycat_analyzer_syntax::tree_sitter;

use crate::conv::position_to_byte;
use crate::ide::types::Range;

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentHighlightKind {
    Text,
    Read,
    Write,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy)]
pub struct DocumentHighlight {
    pub range: Range,
    pub kind: DocumentHighlightKind,
}

pub fn document_highlights(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    encoding: SourceEncoding,
) -> Vec<DocumentHighlight> {
    let byte = position_to_byte(text, pos, encoding);
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
                range: Range::from_byte_range(text, &n.byte_range(), encoding),
                kind: DocumentHighlightKind::Text,
            });
        }
        true
    });
    out
}
