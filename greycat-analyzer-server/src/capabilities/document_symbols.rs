//! Document symbol handler — hierarchy of top-level decls + type members.

use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::Decl;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{DocumentSymbol, SymbolKind};

use crate::conv::byte_range_to_lsp;

pub fn document_symbols(text: &str, lib: &str, root: tree_sitter::Node<'_>) -> Vec<DocumentSymbol> {
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
            Decl::Fn(_) => SymbolKind::FUNCTION,
            Decl::Type(_) => SymbolKind::CLASS,
            Decl::Enum(_) => SymbolKind::ENUM,
            Decl::Var(_) => SymbolKind::VARIABLE,
            Decl::Pragma(_) => SymbolKind::KEY,
        };
        let full_range = byte_range_to_lsp(text, decl.byte_range());
        let selection_range = byte_range_to_lsp(text, &ident.byte_range);
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
                    detail: None,
                    kind: SymbolKind::FIELD,
                    tags: None,
                    #[allow(deprecated)]
                    deprecated: None,
                    range: byte_range_to_lsp(text, &a.byte_range),
                    selection_range: byte_range_to_lsp(text, &aname.byte_range),
                    children: None,
                });
            }
            for method_id in &td.methods {
                if let Decl::Fn(fnd) = &hir.decls[*method_id] {
                    let mname = &hir.idents[fnd.name];
                    let mname_text = &symbols[mname.symbol];
                    if mname_text.is_empty() {
                        continue;
                    }
                    #[allow(deprecated)]
                    children.push(DocumentSymbol {
                        name: mname_text.to_string(),
                        detail: None,
                        kind: SymbolKind::METHOD,
                        tags: None,
                        deprecated: None,
                        range: byte_range_to_lsp(text, &fnd.byte_range),
                        selection_range: byte_range_to_lsp(text, &mname.byte_range),
                        children: None,
                    });
                }
            }
        }
        #[allow(deprecated)]
        out.push(DocumentSymbol {
            name: ident_text.to_string(),
            detail: None,
            kind,
            tags: None,
            deprecated: None,
            range: full_range,
            selection_range,
            children: if children.is_empty() {
                None
            } else {
                Some(children)
            },
        });
    }
    out
}
