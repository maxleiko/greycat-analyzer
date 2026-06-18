//! Folding ranges — region folds for block-shaped CST nodes. IDE-shape
//! ADT lives here so wasm consumers receive it unchanged; the LSP
//! server's [`capabilities::folding_ranges`](../../greycat-analyzer-server/src/capabilities/folding_ranges.rs)
//! becomes a thin converter to `lsp_types::FoldingRange`.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_core::conv::byte_to_position;
use greycat_analyzer_syntax::cst::walk_named;
use greycat_analyzer_syntax::tree_sitter;

/// What kind of source span this fold represents. The LSP spec has a
/// fixed enumeration (`comment` / `imports` / `region`); we currently
/// only emit `Region` — the variant is exposed so editors can render
/// region folds distinctly even if future passes add the other two.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldingRangeKind {
    Comment,
    Imports,
    Region,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct FoldingRange {
    pub start_line: u32,
    pub end_line: u32,
    pub kind: FoldingRangeKind,
}

pub fn folding_ranges(
    text: &str,
    root: tree_sitter::Node<'_>,
    encoding: SourceEncoding,
) -> Vec<FoldingRange> {
    let mut out = Vec::new();
    walk_named(root, |n| {
        if matches!(
            n.kind(),
            "block" | "type_body" | "enum_body" | "object_initializers"
        ) {
            let r = n.byte_range();
            let start = byte_to_position(text, r.start, encoding);
            let end = byte_to_position(text, r.end, encoding);
            if end.line > start.line {
                out.push(FoldingRange {
                    start_line: start.line,
                    end_line: end.line,
                    kind: FoldingRangeKind::Region,
                });
            }
        }
        true
    });
    out
}
