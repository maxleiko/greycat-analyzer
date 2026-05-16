//! Folding range handler — region folds for block-shaped CST nodes.

use greycat_analyzer_syntax::cst::walk_named;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{FoldingRange, FoldingRangeKind};

use crate::conv::byte_to_position;

pub fn folding_ranges(text: &str, root: tree_sitter::Node<'_>) -> Vec<FoldingRange> {
    let mut out = Vec::new();
    walk_named(root, |n| {
        if matches!(
            n.kind(),
            "block" | "type_body" | "enum_body" | "object_initializers"
        ) {
            let r = n.byte_range();
            let start = byte_to_position(text, r.start);
            let end = byte_to_position(text, r.end);
            if end.line > start.line {
                out.push(FoldingRange {
                    start_line: start.line,
                    start_character: None,
                    end_line: end.line,
                    end_character: None,
                    kind: Some(FoldingRangeKind::Region),
                    collapsed_text: None,
                });
            }
        }
        true
    });
    out
}
