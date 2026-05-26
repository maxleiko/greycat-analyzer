//! Selection range — nested CST spans from leaf up to root. Flattened
//! to a `Vec<Range>` (leaf-to-root) per cursor position; the LSP server's
//! `capabilities/selection_ranges.rs` re-builds the nested
//! `SelectionRange { parent: ... }` linked list at the wire boundary.

use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_core::lsp_types::Position;
use greycat_analyzer_syntax::cst::{ancestors, node_at_offset};
use greycat_analyzer_syntax::tree_sitter;

use crate::conv::position_to_byte;
use crate::ide::types::Range;

/// Selection ranges at a single cursor position — leaf-to-root order.
/// Returns an empty vec when the cursor doesn't land on any node.
pub fn selection_ranges(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    encoding: SourceEncoding,
) -> Vec<Range> {
    let byte = position_to_byte(text, pos, encoding);
    let Some(leaf) = node_at_offset(root, byte) else {
        return Vec::new();
    };
    ancestors(leaf)
        .map(|n| Range::from_byte_range(text, &n.byte_range(), encoding))
        .collect()
}
