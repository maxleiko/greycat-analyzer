//! Workspace symbol handler — aggregates every document's
//! [`document_symbols`](super::document_symbols) output, flattens it,
//! and filters by case-insensitive substring match (matching the TS
//! reference).

use greycat_analyzer_core::SourceEncoding;
use lsp_types::{DocumentSymbol, Location, OneOf, Uri, WorkspaceSymbol};

use super::document_symbols::document_symbols;

pub fn workspace_symbols(
    docs: impl IntoIterator<Item = (Uri, String, String)>,
    query: &str,
    encoding: SourceEncoding,
) -> Vec<WorkspaceSymbol> {
    let needle = query.to_lowercase();
    let mut out = Vec::new();
    for (uri, lib, text) in docs {
        let tree = greycat_analyzer_syntax::parse(&text);
        let symbols = document_symbols(&text, &lib, tree.root_node(), encoding);
        flatten_workspace(&uri, &symbols, &needle, &mut out);
    }
    out
}

fn flatten_workspace(
    uri: &Uri,
    symbols: &[DocumentSymbol],
    needle: &str,
    out: &mut Vec<WorkspaceSymbol>,
) {
    for sym in symbols {
        if needle.is_empty() || sym.name.to_lowercase().contains(needle) {
            out.push(WorkspaceSymbol {
                name: sym.name.clone(),
                kind: sym.kind,
                tags: sym.tags.clone(),
                container_name: None,
                location: OneOf::Left(Location {
                    uri: uri.clone(),
                    range: sym.selection_range,
                }),
                data: None,
            });
        }
        if let Some(children) = &sym.children {
            flatten_workspace(uri, children, needle, out);
        }
    }
}
