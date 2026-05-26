//! Thin converter from the IDE-shape `analysis::ide::workspace_symbols`
//! ADT to `lsp_types::WorkspaceSymbol`. Production logic lives in the
//! analysis crate so the same flattening + substring filter is
//! reachable from the wasm bridge unchanged.

use greycat_analyzer_analysis::ide::document_symbols::SymbolKind as IdeSymbolKind;
use greycat_analyzer_analysis::ide::types::{
    Location as IdeLocation, Position as IdePosition, Range as IdeRange,
};
use greycat_analyzer_analysis::ide::workspace_symbols::{
    WorkspaceSymbol as IdeWorkspaceSymbol, workspace_symbols as workspace_symbols_inner,
};
use greycat_analyzer_core::SourceEncoding;
use lsp_types::{Location, OneOf, Position, Range, SymbolKind, Uri, WorkspaceSymbol};

pub fn workspace_symbols(
    docs: impl IntoIterator<Item = (Uri, String, String)>,
    query: &str,
    encoding: SourceEncoding,
) -> Vec<WorkspaceSymbol> {
    workspace_symbols_inner(docs, query, encoding)
        .into_iter()
        .map(to_lsp)
        .collect()
}

fn to_lsp(sym: IdeWorkspaceSymbol) -> WorkspaceSymbol {
    WorkspaceSymbol {
        name: sym.name,
        kind: kind_to_lsp(sym.kind),
        tags: None,
        container_name: None,
        location: OneOf::Left(location_to_lsp(sym.location)),
        data: None,
    }
}

fn location_to_lsp(loc: IdeLocation) -> Location {
    Location {
        uri: loc.uri,
        range: range_to_lsp(loc.range),
    }
}

fn kind_to_lsp(kind: IdeSymbolKind) -> SymbolKind {
    match kind {
        IdeSymbolKind::Function => SymbolKind::FUNCTION,
        IdeSymbolKind::Class => SymbolKind::CLASS,
        IdeSymbolKind::Enum => SymbolKind::ENUM,
        IdeSymbolKind::Variable => SymbolKind::VARIABLE,
        IdeSymbolKind::Key => SymbolKind::KEY,
        IdeSymbolKind::Field => SymbolKind::FIELD,
        IdeSymbolKind::Method => SymbolKind::METHOD,
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
