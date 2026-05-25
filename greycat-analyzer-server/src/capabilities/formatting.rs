//! Document and range formatting handlers.

use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{Position, TextEdit};

use crate::conv::{byte_to_position, position_to_byte};

/// Whole-document formatting. Returns a single `TextEdit` that replaces
/// the entire document range when the formatter's output differs from
/// the input. Returns `None` (no edits) when the document is already
/// formatted.
pub fn formatting(
    text: &str,
    root: tree_sitter::Node<'_>,
    encoding: SourceEncoding,
) -> Option<Vec<TextEdit>> {
    let formatted = greycat_analyzer_fmt::format_tree(text, root);
    if formatted == text {
        return Some(Vec::new());
    }
    let last_byte = text.len();
    let end_pos = byte_to_position(text, last_byte, encoding);
    Some(vec![TextEdit {
        range: lsp_types::Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: end_pos,
        },
        new_text: formatted,
    }])
}

///  range formatting — format only the text inside `range`. The
/// foundational formatter operates on whole-tree input, so the
/// implementation snapshots the slice, formats it, and returns a single
/// replacement edit covering the requested range. Falls back to no
/// edits when the slice doesn't change.
pub fn range_formatting(
    text: &str,
    root: tree_sitter::Node<'_>,
    range: lsp_types::Range,
    encoding: SourceEncoding,
) -> Option<Vec<TextEdit>> {
    let _ = root;
    let start = position_to_byte(text, range.start, encoding);
    let end = position_to_byte(text, range.end, encoding);
    if end <= start || end > text.len() {
        return Some(Vec::new());
    }
    let slice = &text[start..end];
    let sub_tree = greycat_analyzer_syntax::parse(slice);
    let formatted = greycat_analyzer_fmt::format_tree(slice, sub_tree.root_node());
    if formatted == slice {
        return Some(Vec::new());
    }
    Some(vec![TextEdit {
        range,
        new_text: formatted,
    }])
}
