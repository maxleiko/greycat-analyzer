//! Thin converter from the IDE-shape `analysis::ide::hover::Hover`
//! ADT to `lsp_types::Hover`. All hover production logic lives in the
//! analysis crate (`crate::ide::hover`); this file maps the result onto
//! the LSP wire shape so the legacy `capabilities::hover_with_project`
//! callers (server.rs) keep their signature.

use greycat_analyzer_analysis::ide::hover::{Hover as IdeHover, hover_with_project as hover_inner};
use greycat_analyzer_analysis::ide::types::{Position as IdePosition, Range as IdeRange};
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::{SourceEncoding, SourceManager};
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Range, Uri};

#[allow(clippy::too_many_arguments)]
pub fn hover_with_project(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    uri: &Uri,
    project: &ProjectAnalysis,
    manager: &SourceManager,
    encoding: SourceEncoding,
) -> Option<Hover> {
    hover_inner(text, lib, root, pos, uri, project, manager, encoding).map(to_lsp)
}

fn to_lsp(h: IdeHover) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: h.markdown,
        }),
        range: Some(range_to_lsp(h.range)),
    }
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
