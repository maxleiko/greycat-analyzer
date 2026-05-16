//! Project-wide find-references + rename. The analysis half
//! ("which sites does this binding cover?") lives in
//! [`greycat_analyzer_analysis::rename`]; this module is the thin LSP
//! shape-converter that fetches each site's source text via the
//! `SourceManager` and emits `Location` / `TextEdit` values.
//!
//! `prepare_rename` stays here as a pure single-file handler — it only
//! validates the cursor sits on an ident, no analysis state needed.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_analysis::rename;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_hir::Hir;
use greycat_analyzer_syntax::cst::node_at_offset;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{Location, Position, PrepareRenameResponse, TextEdit, Uri, WorkspaceEdit};

pub use greycat_analyzer_analysis::rename::RenameTarget;

use super::goto::cursor_ident_idx;
use crate::conv::{byte_range_to_lsp, position_to_byte};

/// Map a tree-sitter ident node back to its `Idx<Ident>` in the HIR
/// arena by byte-range match. Returns `None` if no matching ident was
/// allocated (e.g., the lowering skipped this shape).
pub(super) fn idx_for_node(
    hir: &Hir,
    node: tree_sitter::Node<'_>,
) -> Option<greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>> {
    hir.idents
        .iter()
        .find(|(_, i)| i.byte_range == node.byte_range())
        .map(|(idx, _)| idx)
}

pub fn prepare_rename(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
) -> Option<PrepareRenameResponse> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return None;
    }
    let placeholder = text.get(node.byte_range())?.to_string();
    Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: byte_range_to_lsp(text, &node.byte_range()),
        placeholder,
    })
}

/// Thin re-export of the analysis-crate classifier so existing callers
/// of `capabilities::resolve_rename_target` keep working.
pub fn resolve_rename_target(
    project: &ProjectAnalysis,
    cursor_uri: &Uri,
    cursor_idx: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>,
) -> Option<RenameTarget> {
    rename::resolve_target(project, cursor_uri, cursor_idx)
}

// P11.4
/// Find every reference to the cursor's binding across the
/// whole project.
pub fn references_across_project(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
) -> Vec<Location> {
    let Some(target) = cursor_target(project, manager, cursor_uri, cursor_pos) else {
        return Vec::new();
    };
    rename::target_sites(project, &target)
        .into_iter()
        .filter_map(|site| {
            let cell = manager.get(&site.uri)?;
            let doc = cell.borrow();
            Some(Location {
                range: byte_range_to_lsp(&doc.text, &site.byte_range),
                uri: site.uri,
            })
        })
        .collect()
}

// P11.4
/// Produce a `WorkspaceEdit` renaming every site the cursor's
/// binding is referenced from, across the whole project.
pub fn rename_across_project(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let target = cursor_target(project, manager, cursor_uri, cursor_pos)?;
    #[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
    let mut changes: std::collections::HashMap<Uri, Vec<TextEdit>> =
        std::collections::HashMap::new();
    for site in rename::target_sites(project, &target) {
        let Some(cell) = manager.get(&site.uri) else {
            continue;
        };
        let doc = cell.borrow();
        let range = byte_range_to_lsp(&doc.text, &site.byte_range);
        changes.entry(site.uri).or_default().push(TextEdit {
            range,
            new_text: new_name.to_string(),
        });
    }
    if changes.is_empty() {
        return None;
    }
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

fn cursor_target(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
) -> Option<RenameTarget> {
    let cell = manager.get(cursor_uri)?;
    let doc = cell.borrow();
    let module = project.module(cursor_uri)?;
    let cursor_idx = cursor_ident_idx(&doc.text, doc.root_node(), cursor_pos, &module.hir)?;
    drop(doc);
    rename::resolve_target(project, cursor_uri, cursor_idx)
}
