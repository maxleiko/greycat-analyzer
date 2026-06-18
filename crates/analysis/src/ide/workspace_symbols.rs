//! Workspace-wide symbol query — aggregates every document's
//! [`crate::ide::document_symbols`] output, flattens it, and filters by
//! case-insensitive substring match.
//!
//! IDE-shape ADT decoupled from `lsp_types`; the LSP server's
//! `capabilities/workspace_symbols.rs` lifts to `lsp_types::WorkspaceSymbol`
//! at the wire boundary, and the wasm bridge consumes the same shape
//! unchanged.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_core::lsp_types::Uri;

use crate::ide::document_symbols::{DocumentSymbol, SymbolKind, document_symbols};
use crate::ide::types::Location;

/// IDE-shape workspace-wide symbol entry. The `uri` field is wasm-
/// `skip`'d and reachable via the inherited `location.uri()` getter
/// (same pattern as [`Location`]).
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct WorkspaceSymbol {
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub name: String,
    pub kind: SymbolKind,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub location: Location,
}

/// Aggregate every document's [`document_symbols`] output, flatten it,
/// and filter by case-insensitive substring match. The input iterator
/// yields `(uri, lib, text)` triples — the same shape the server's
/// `Backend::all_documents` produces.
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
        flatten(&uri, &symbols, &needle, &mut out);
    }
    out
}

fn flatten(uri: &Uri, symbols: &[DocumentSymbol], needle: &str, out: &mut Vec<WorkspaceSymbol>) {
    for sym in symbols {
        if needle.is_empty() || sym.name.to_lowercase().contains(needle) {
            out.push(WorkspaceSymbol {
                name: sym.name.clone(),
                kind: sym.kind,
                location: Location {
                    uri: uri.clone(),
                    range: sym.selection_range,
                },
            });
        }
        if !sym.children.is_empty() {
            flatten(uri, &sym.children, needle, out);
        }
    }
}
