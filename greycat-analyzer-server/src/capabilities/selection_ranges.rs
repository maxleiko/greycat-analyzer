//! Thin converter that re-builds the LSP `SelectionRange { parent: ... }`
//! linked list from the IDE-shape flat `Vec<Range>` produced by
//! [`greycat_analyzer_analysis::ide::selection_ranges`].

use greycat_analyzer_analysis::ide::selection_ranges::selection_ranges as selection_ranges_inner;
use greycat_analyzer_analysis::ide::types::{Position as IdePosition, Range as IdeRange};
use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{Position, Range, SelectionRange};

pub fn selection_ranges(
    text: &str,
    root: tree_sitter::Node<'_>,
    positions: &[Position],
    encoding: SourceEncoding,
) -> Vec<SelectionRange> {
    positions
        .iter()
        .filter_map(|pos| {
            let chain = selection_ranges_inner(text, root, *pos, encoding);
            if chain.is_empty() {
                return None;
            }
            // Walk leaf-to-root and re-link in reverse so each entry's
            // `parent` points outward (LSP convention).
            let mut head: Option<SelectionRange> = None;
            for r in chain.into_iter().rev() {
                head = Some(SelectionRange {
                    range: range_to_lsp(r),
                    parent: head.map(Box::new),
                });
            }
            head
        })
        .collect()
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
