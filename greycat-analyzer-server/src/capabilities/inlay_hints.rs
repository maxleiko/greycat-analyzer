//! Thin converter from the IDE-shape `analysis::ide::inlay_hints::InlayHint`
//! ADT to `lsp_types::InlayHint`. Production logic lives in the
//! analysis crate.

use greycat_analyzer_analysis::ide::inlay_hints::{
    InlayHint as IdeInlayHint, InlayHintKind as IdeInlayHintKind,
    inlay_hints_with_project as inlay_hints_inner,
};
use greycat_analyzer_analysis::ide::types::{Position as IdePosition, Range as IdeRange};
use greycat_analyzer_analysis::project::{ModuleAnalysis, ProjectAnalysis};
use greycat_analyzer_core::SourceEncoding;
use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Position, Range};

pub fn inlay_hints_with_project(
    module: &ModuleAnalysis,
    project: &ProjectAnalysis,
    text: &str,
    range: &Range,
    encoding: SourceEncoding,
) -> Vec<InlayHint> {
    let ide_range = IdeRange {
        start: IdePosition {
            line: range.start.line,
            character: range.start.character,
        },
        end: IdePosition {
            line: range.end.line,
            character: range.end.character,
        },
    };
    inlay_hints_inner(module, project, text, &ide_range, encoding)
        .into_iter()
        .map(to_lsp)
        .collect()
}

fn to_lsp(h: IdeInlayHint) -> InlayHint {
    InlayHint {
        position: Position {
            line: h.position.line,
            character: h.position.character,
        },
        label: InlayHintLabel::String(h.label),
        kind: Some(match h.kind {
            IdeInlayHintKind::Type => InlayHintKind::TYPE,
            IdeInlayHintKind::Parameter => InlayHintKind::PARAMETER,
        }),
        text_edits: None,
        tooltip: None,
        padding_left: if h.padding_left { Some(true) } else { None },
        padding_right: if h.padding_right { Some(true) } else { None },
        data: None,
    }
}
