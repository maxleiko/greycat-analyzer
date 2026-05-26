//! IDE-shape primitive types shared across the `analysis::ide::*`
//! capability ADTs. Decoupled from `lsp_types` so the same shapes
//! cross the wasm boundary unchanged.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_core::conv::byte_to_position;

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub fn from_byte_range(
        text: &str,
        range: &std::ops::Range<usize>,
        encoding: SourceEncoding,
    ) -> Self {
        let start = byte_to_position(text, range.start, encoding);
        let end = byte_to_position(text, range.end, encoding);
        Self {
            start: Position {
                line: start.line,
                character: start.character,
            },
            end: Position {
                line: end.line,
                character: end.character,
            },
        }
    }
}

/// Replace `range` (in `(line, character)` coords) with `new_text`.
/// IDE-shape mirror of `lsp_types::TextEdit`; the analysis crate's
/// own [`crate::ide::actions::TextEdit`] carries a `byte_range` for
/// internal quickfix consumers and stays unchanged.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct TextEdit {
    pub range: Range,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub new_text: String,
}
