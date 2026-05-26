//! Signature help — when the cursor is inside a `call_expr`, surface
//! the callee fn's parameter list with the active parameter
//! highlighted. IDE-shape ADTs flow through the wasm bridge unchanged;
//! the LSP server's `capabilities/signature_help.rs` converts to
//! `lsp_types::SignatureHelp` at the wire boundary.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use greycat_analyzer_core::lsp_types::Position;
use greycat_analyzer_core::{SourceEncoding, SymbolTable};
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::Decl;
use greycat_analyzer_syntax::cst::node_at_offset;
use greycat_analyzer_syntax::tree_sitter;

use crate::conv::position_to_byte;
use crate::ide::render::render_type_ref;

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct ParameterInformation {
    /// Byte offsets into the parent [`SignatureInformation`]'s `label`
    /// pointing at this parameter's slice — matches LSP's
    /// `ParameterLabel::LabelOffsets` shape so editors that highlight
    /// the active parameter can do so without re-parsing the label.
    pub label_start: u32,
    pub label_end: u32,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub documentation: Option<String>,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct SignatureInformation {
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub label: String,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub documentation: Option<String>,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub parameters: Vec<ParameterInformation>,
    pub active_parameter: u32,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct SignatureHelp {
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub signatures: Vec<SignatureInformation>,
    pub active_signature: u32,
    pub active_parameter: u32,
}

pub fn signature_help(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    encoding: SourceEncoding,
) -> Option<SignatureHelp> {
    let byte = position_to_byte(text, pos, encoding);
    let mut node = node_at_offset(root, byte)?;
    // Walk up looking for a `call_expr`.
    while node.kind() != "call_expr" {
        node = node.parent()?;
    }
    let callee = node.child_by_field_name("fn")?;
    let callee_text = text.get(callee.byte_range())?;

    let symbols = SymbolTable::new();
    let hir = lower_module(text, &symbols, "module", lib, root);
    // Find a fn_decl with matching name.
    let module = hir.module.as_ref()?;
    let fnd = module.decls.iter().find_map(|d| match &hir.decls[*d] {
        Decl::Fn(f) if &symbols[hir.idents[f.name].symbol] == callee_text => Some(f.clone()),
        _ => None,
    })?;

    let mut params = Vec::new();
    let mut label = format!("fn {}(", &symbols[hir.idents[fnd.name].symbol]);
    for (i, p_id) in fnd.params.iter().enumerate() {
        if i > 0 {
            label.push_str(", ");
        }
        let p = &hir.fn_params[*p_id];
        let pname = symbols[hir.idents[p.name].symbol].to_string();
        let label_start = label.len();
        let mut piece = pname.clone();
        if let Some(ty_id) = p.ty {
            piece.push_str(": ");
            piece.push_str(&render_type_ref(&hir, &symbols, ty_id));
        }
        label.push_str(&piece);
        params.push(ParameterInformation {
            label_start: label_start as u32,
            label_end: (label_start + piece.len()) as u32,
            documentation: None,
        });
    }
    label.push(')');
    if let Some(rt) = fnd.return_type {
        label.push_str(": ");
        label.push_str(&render_type_ref(&hir, &symbols, rt));
    }

    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation: fnd.doc,
            parameters: params,
            active_parameter: 0,
        }],
        active_signature: 0,
        active_parameter: 0,
    })
}
