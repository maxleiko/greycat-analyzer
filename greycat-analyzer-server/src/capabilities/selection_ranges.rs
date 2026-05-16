//! Selection range handler — nested ranges from leaf up to root.

use greycat_analyzer_syntax::cst::{ancestors, node_at_offset};
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{Position, SelectionRange};

use crate::conv::{byte_range_to_lsp, position_to_byte};

pub fn selection_ranges(
    text: &str,
    root: tree_sitter::Node<'_>,
    positions: &[Position],
) -> Vec<SelectionRange> {
    positions
        .iter()
        .filter_map(|pos| {
            let byte = position_to_byte(text, *pos);
            let leaf = node_at_offset(root, byte)?;
            let mut head: Option<SelectionRange> = None;
            let chain: Vec<lsp_types::Range> = ancestors(leaf)
                .map(|n| byte_range_to_lsp(text, &n.byte_range()))
                .collect();
            for r in chain.into_iter().rev() {
                head = Some(SelectionRange {
                    range: r,
                    parent: head.map(Box::new),
                });
            }
            head
        })
        .collect()
}
