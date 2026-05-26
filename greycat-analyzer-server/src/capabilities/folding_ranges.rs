//! Thin converter from the IDE-shape `analysis::ide::folding_ranges`
//! ADT to `lsp_types::FoldingRange`.

use greycat_analyzer_analysis::ide::folding_ranges::{
    FoldingRange as IdeFoldingRange, FoldingRangeKind as IdeFoldingRangeKind,
    folding_ranges as folding_ranges_inner,
};
use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{FoldingRange, FoldingRangeKind};

pub fn folding_ranges(
    text: &str,
    root: tree_sitter::Node<'_>,
    encoding: SourceEncoding,
) -> Vec<FoldingRange> {
    folding_ranges_inner(text, root, encoding)
        .into_iter()
        .map(to_lsp)
        .collect()
}

fn to_lsp(r: IdeFoldingRange) -> FoldingRange {
    FoldingRange {
        start_line: r.start_line,
        start_character: None,
        end_line: r.end_line,
        end_character: None,
        kind: Some(match r.kind {
            IdeFoldingRangeKind::Comment => FoldingRangeKind::Comment,
            IdeFoldingRangeKind::Imports => FoldingRangeKind::Imports,
            IdeFoldingRangeKind::Region => FoldingRangeKind::Region,
        }),
        collapsed_text: None,
    }
}
