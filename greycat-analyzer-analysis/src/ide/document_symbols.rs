//! Document outline — hierarchy of top-level decls (with type
//! members as children). IDE-shape ADT decoupled from `lsp_types`;
//! the LSP server's `capabilities/document_symbols.rs` converts to
//! `lsp_types::DocumentSymbol` at the wire boundary.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use greycat_analyzer_core::{SourceEncoding, SymbolTable};
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::hir::Decl;
use greycat_analyzer_syntax::tree_sitter;

use crate::ide::types::Range;

/// Mirror of `lsp_types::SymbolKind` for the subset the analyzer
/// produces. Same idea as [`crate::ide::completion::CompletionItemKind`]:
/// trimmed to what we actually emit so the bindgen surface stays clean.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Class,
    Enum,
    Variable,
    Key,
    Field,
    Method,
}

/// IDE-shape document symbol. Recursive via `children` — each
/// `DocumentSymbol` is its own wasm-bindgen object so `Vec<Self>`
/// crosses the boundary as a JS array of opaque handles.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct DocumentSymbol {
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub name: String,
    pub kind: SymbolKind,
    /// Full source span — decl signature + body for fns / types, the
    /// `attr:` line for type attrs, etc.
    pub range: Range,
    /// Span of the name identifier — what editors highlight when the
    /// user picks the symbol from the outline.
    pub selection_range: Range,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub children: Vec<DocumentSymbol>,
}

pub fn document_symbols(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    encoding: SourceEncoding,
) -> Vec<DocumentSymbol> {
    let symbols = SymbolTable::new();
    let hir = lower_module(text, &symbols, "module", lib, root);
    let module = match hir.module.as_ref() {
        Some(m) => m,
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    for decl_id in &module.decls {
        let decl = &hir.decls[*decl_id];
        let Some(name_id) = decl.name() else {
            continue;
        };
        let ident = &hir.idents[name_id];
        let ident_text = &symbols[ident.symbol];
        // The LSP `DocumentSymbol.name` field is required non-empty —
        // VSCode's client throws "name must not be falsy" if it sees
        // a `""`. Tree-sitter's error recovery synthesizes empty-range
        // name nodes when the user is mid-typing (`type Foo { static<CURSOR> }`
        // recovers as a partial method decl with an empty name ident),
        // and lowering's `alloc_ident` faithfully interns that empty
        // text. Skip rather than push it; an out-of-LSP-spec symbol
        // poisons the whole batch on the client side.
        if ident_text.is_empty() {
            continue;
        }
        let kind = match decl {
            Decl::Fn(_) => SymbolKind::Function,
            Decl::Type(_) => SymbolKind::Class,
            Decl::Enum(_) => SymbolKind::Enum,
            Decl::Var(_) => SymbolKind::Variable,
            Decl::Pragma(_) => SymbolKind::Key,
        };
        let full_range = Range::from_byte_range(text, decl.byte_range(), encoding);
        let selection_range = Range::from_byte_range(text, &ident.byte_range, encoding);
        let mut children: Vec<DocumentSymbol> = Vec::new();
        if let Decl::Type(td) = decl {
            for attr_id in &td.attrs {
                let a = &hir.type_attrs[*attr_id];
                let aname = &hir.idents[a.name];
                let aname_text = &symbols[aname.symbol];
                if aname_text.is_empty() {
                    continue;
                }
                children.push(DocumentSymbol {
                    name: aname_text.to_string(),
                    kind: SymbolKind::Field,
                    range: Range::from_byte_range(text, &a.byte_range, encoding),
                    selection_range: Range::from_byte_range(text, &aname.byte_range, encoding),
                    children: Vec::new(),
                });
            }
            for method_id in &td.methods {
                if let Decl::Fn(fnd) = &hir.decls[*method_id] {
                    let mname = &hir.idents[fnd.name];
                    let mname_text = &symbols[mname.symbol];
                    if mname_text.is_empty() {
                        continue;
                    }
                    children.push(DocumentSymbol {
                        name: mname_text.to_string(),
                        kind: SymbolKind::Method,
                        range: Range::from_byte_range(text, &fnd.byte_range, encoding),
                        selection_range: Range::from_byte_range(text, &mname.byte_range, encoding),
                        children: Vec::new(),
                    });
                }
            }
        }
        out.push(DocumentSymbol {
            name: ident_text.to_string(),
            kind,
            range: full_range,
            selection_range,
            children,
        });
    }
    out
}
