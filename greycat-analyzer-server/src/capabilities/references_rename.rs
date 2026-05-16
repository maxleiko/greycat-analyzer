//! Find references + rename — both the single-file shim variants and
//! the project-wide variants the LSP server actually dispatches to.
//! Shares `RenameTarget` + `cursor_target` + `visit_target_sites` so the
//! two project-wide capabilities walk the same target-site tree.

use std::ops::Range;

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_analysis::resolver::{Definition, Resolutions, resolve};
use greycat_analyzer_core::{SourceManager, SymbolTable};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::Decl;
use greycat_analyzer_syntax::cst::{node_at_offset, walk_named};
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{Location, Position, PrepareRenameResponse, TextEdit, Uri, WorkspaceEdit};

use super::cursor_ident_idx;
use crate::conv::{byte_range_to_lsp, position_to_byte};

pub fn references(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    uri: &Uri,
    pos: Position,
) -> Vec<Location> {
    let byte = position_to_byte(text, pos);
    let Some(node) = node_at_offset(root, byte) else {
        return Vec::new();
    };
    if node.kind() != "ident" {
        return Vec::new();
    }

    // P8.1 scope-aware filter: resolve the cursor ident's binding via
    // `Resolutions`, then collect every use whose `Definition` points
    // back at the same binding. Falls back to text equality for
    // `Definition::Project` (cross-module — P8.2 lifts it through the
    // project pipeline).
    let symbols = SymbolTable::new();
    let hir = lower_module(text, &symbols, "module", lib, root);
    let res = resolve(&hir, &symbols);
    let Some(cursor_idx) = idx_for_node(&hir, node) else {
        return Vec::new();
    };
    let Some(target) = target_binding(&hir, &res, cursor_idx) else {
        // Cross-module / unresolved: fall back to text equality so the
        // capability doesn't go silent on stdlib symbols.
        return references_by_text(text, root, node, uri);
    };

    let mut out = Vec::new();
    // Include the binding site itself.
    out.push(Location {
        uri: uri.clone(),
        range: byte_range_to_lsp(text, &hir.idents[target].byte_range),
    });
    for (use_idx, def) in &res.uses {
        let resolves_to = match def {
            Definition::Param(i) | Definition::Local(i) | Definition::Generic(i) => Some(*i),
            Definition::Decl(decl_id) => hir.decls[*decl_id].name(),
            // Cross-module `ProjectDecl` use sites point at a foreign
            // HIR — they don't share the local `target` ident. P11.4
            // walks every doc's `Resolutions` to filter these by URI.
            Definition::ProjectDecl { .. } | Definition::Project => None,
        };
        if resolves_to == Some(target) {
            out.push(Location {
                uri: uri.clone(),
                range: byte_range_to_lsp(text, &hir.idents[*use_idx].byte_range),
            });
        }
    }
    out
}

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

/// Resolve `cursor_idx` to the *binding* `Idx<Ident>` (the def site
/// the resolver would point at). Returns `None` for `Project` /
/// unresolved idents — caller decides the fallback.
fn target_binding(
    hir: &Hir,
    res: &Resolutions,
    cursor_idx: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>,
) -> Option<greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>> {
    if let Some(def) = res.uses.get(&cursor_idx) {
        return match def {
            Definition::Param(i) | Definition::Local(i) | Definition::Generic(i) => Some(*i),
            Definition::Decl(decl_id) => hir.decls[*decl_id].name(),
            Definition::ProjectDecl { .. } | Definition::Project => None,
        };
    }
    // Not a use site — cursor is on a binding. Treat the cursor as the
    // binding itself.
    Some(cursor_idx)
}

/// Pre- text-equality fallback. Used when the cursor doesn't
/// resolve through `Resolutions` (e.g., cross-module names) so the
/// capability still returns useful results.
fn references_by_text(
    text: &str,
    root: tree_sitter::Node<'_>,
    cursor_node: tree_sitter::Node<'_>,
    uri: &Uri,
) -> Vec<Location> {
    let target_text = text.get(cursor_node.byte_range()).unwrap_or("").to_string();
    if target_text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    walk_named(root, |n| {
        if n.kind() == "ident" && text.get(n.byte_range()).unwrap_or("") == target_text {
            out.push(Location {
                uri: uri.clone(),
                range: byte_range_to_lsp(text, &n.byte_range()),
            });
        }
        true
    });
    out
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

pub fn rename(
    text: &str,
    root: tree_sitter::Node<'_>,
    uri: &Uri,
    pos: Position,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return None;
    }

    // P8.1: same scope-aware filter as `references`. Falls back to
    // text equality when the cursor name doesn't resolve through
    // `Resolutions` (cross-module — P8.2 picks that up).
    let symbols = SymbolTable::new();
    let hir = lower_module(text, &symbols, "module", "project", root);
    let res = resolve(&hir, &symbols);
    let mut edits = Vec::new();
    if let Some(cursor_idx) = idx_for_node(&hir, node)
        && let Some(target) = target_binding(&hir, &res, cursor_idx)
    {
        edits.push(TextEdit {
            range: byte_range_to_lsp(text, &hir.idents[target].byte_range),
            new_text: new_name.to_string(),
        });
        for (use_idx, def) in &res.uses {
            let resolves_to = match def {
                Definition::Param(i) | Definition::Local(i) | Definition::Generic(i) => Some(*i),
                Definition::Decl(decl_id) => hir.decls[*decl_id].name(),
                Definition::ProjectDecl { .. } | Definition::Project => None,
            };
            if resolves_to == Some(target) {
                edits.push(TextEdit {
                    range: byte_range_to_lsp(text, &hir.idents[*use_idx].byte_range),
                    new_text: new_name.to_string(),
                });
            }
        }
    } else {
        // Fallback: text equality for unresolvable / cross-module names.
        let target_text = text.get(node.byte_range())?.to_string();
        walk_named(root, |n| {
            if n.kind() == "ident" && text.get(n.byte_range()).unwrap_or("") == target_text {
                edits.push(TextEdit {
                    range: byte_range_to_lsp(text, &n.byte_range()),
                    new_text: new_name.to_string(),
                });
            }
            true
        });
    }
    #[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice
    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), edits);
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

// =============================================================================
// P11.4 — project-wide references + rename
// =============================================================================

/// What the cursor is asking us to rename / find references for.
/// Returned by [`resolve_rename_target`] and consumed by
/// [`references_across_project`] / [`rename_across_project`].
#[derive(Debug, Clone)]
pub enum RenameTarget {
    /// Function parameter / local var / generic-param. Confined to its
    /// declaring module's scope — no cross-module fan-out.
    LocalIdent {
        uri: Uri,
        ident: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>,
    },
    /// Top-level decl. May be referenced from any module via
    /// [`Definition::Decl`] (in the home module) or
    /// [`Definition::ProjectDecl`] (importers).
    ProjectDecl {
        uri: Uri,
        decl: greycat_analyzer_hir::arena::Idx<Decl>,
    },
    /// A type attribute (`type Foo { name: String; }`). Use sites
    /// bind via the analyzer's `member_uses` / `foreign_member_uses`
    /// maps, not via the resolver — so a separate target shape is
    /// needed.
    TypeAttr {
        uri: Uri,
        attr: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::TypeAttr>,
    },
    /// A type method (`type Foo { fn m() ... }`). The method itself
    /// is a `Decl::Fn` (with its own `Idx<Decl>`), but use sites
    /// resolve through `member_uses` / `foreign_member_uses` rather
    /// than `Resolutions::uses`. Distinct from
    /// [`Self::ProjectDecl`] so the visitor can fan out over the
    /// member-use maps instead of decl-use maps.
    TypeMethod {
        uri: Uri,
        method: greycat_analyzer_hir::arena::Idx<Decl>,
    },
}

/// Inspect the cursor's binding through cached project analysis and
/// classify the rename / reference target. Returns `None` for cursors
/// not on an ident, runtime-only names ([`Definition::Project`]
/// `Array`, `Map`, native fns, primitives), and unrecognized binding
/// shapes (e.g. method names — that's  /  territory).
pub fn resolve_rename_target(
    project: &ProjectAnalysis,
    cursor_uri: &Uri,
    cursor_idx: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>,
) -> Option<RenameTarget> {
    use greycat_analyzer_analysis::analyzer::MemberDef;
    let module = project.module(cursor_uri)?;
    if let Some(def) = module.resolutions.lookup(cursor_idx) {
        return match def {
            Definition::Param(i) | Definition::Local(i) | Definition::Generic(i) => {
                Some(RenameTarget::LocalIdent {
                    uri: cursor_uri.clone(),
                    ident: i,
                })
            }
            Definition::Decl(decl) => Some(RenameTarget::ProjectDecl {
                uri: cursor_uri.clone(),
                decl,
            }),
            Definition::ProjectDecl { uri, decl } => Some(RenameTarget::ProjectDecl { uri, decl }),
            // Runtime-only names (Array / Map / node tags / native fns
            // / primitives) have no declaration to rename.
            Definition::Project => None,
        };
    }
    // Member-position cursor: the resolver doesn't bind `.x` /
    // `t.method()` (member resolution lives in the analyzer). Consult
    // the analyzer's member-use maps to find which attr / method this
    // use site refers to, then return the corresponding rename target
    // anchored at the *home* module of the type that owns the member.
    if let Some(member) = module.analysis.member_lookup(cursor_idx) {
        return Some(match member {
            MemberDef::Attr(attr) => RenameTarget::TypeAttr {
                uri: cursor_uri.clone(),
                attr,
            },
            MemberDef::Method(method) => RenameTarget::TypeMethod {
                uri: cursor_uri.clone(),
                method,
            },
        });
    }
    if let Some(foreign) = module.analysis.foreign_member_lookup(cursor_idx) {
        return Some(match foreign.member {
            MemberDef::Attr(attr) => RenameTarget::TypeAttr {
                uri: foreign.uri.clone(),
                attr,
            },
            MemberDef::Method(method) => RenameTarget::TypeMethod {
                uri: foreign.uri.clone(),
                method,
            },
        });
    }
    // Cursor isn't a use site — it's on a binding. Three flavors of
    // binding live at module top level:
    //   1. Top-level decls (`fn` / `type` / `enum` / `var`) — their
    //      name idents appear in `module.decls`.
    //   2. Type members (attrs + methods) — nested under a `Decl::Type`.
    //   3. Param / local / generic — the LocalIdent fallback.
    let module_root = module.hir.module.as_ref()?;
    for &decl_id in &module_root.decls {
        if module.hir.decls[decl_id].name() == Some(cursor_idx) {
            return Some(RenameTarget::ProjectDecl {
                uri: cursor_uri.clone(),
                decl: decl_id,
            });
        }
        if let Decl::Type(td) = &module.hir.decls[decl_id] {
            for &attr_id in &td.attrs {
                if module.hir.type_attrs[attr_id].name == cursor_idx {
                    return Some(RenameTarget::TypeAttr {
                        uri: cursor_uri.clone(),
                        attr: attr_id,
                    });
                }
            }
            for &method_id in &td.methods {
                if module.hir.decls[method_id].name() == Some(cursor_idx) {
                    return Some(RenameTarget::TypeMethod {
                        uri: cursor_uri.clone(),
                        method: method_id,
                    });
                }
            }
        }
    }
    Some(RenameTarget::LocalIdent {
        uri: cursor_uri.clone(),
        ident: cursor_idx,
    })
}

// P11.4
/// Find every reference to the cursor's binding across the
/// whole project. Replaces the previous text-equality fallback.
pub fn references_across_project(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
) -> Vec<Location> {
    let Some(target) = cursor_target(project, manager, cursor_uri, cursor_pos) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    visit_target_sites(project, manager, &target, |uri, text, range| {
        out.push(Location {
            uri: uri.clone(),
            range: byte_range_to_lsp(text, &range),
        });
    });
    out
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
    visit_target_sites(project, manager, &target, |uri, text, range| {
        changes.entry(uri.clone()).or_default().push(TextEdit {
            range: byte_range_to_lsp(text, &range),
            new_text: new_name.to_string(),
        });
    });
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
    resolve_rename_target(project, cursor_uri, cursor_idx)
}

/// Walk every site the rename target is referenced from. Calls `emit`
/// with `(home_uri, home_text, byte_range)` for each hit — emit may
/// shape it into a `Location`, `TextEdit`, etc.
fn visit_target_sites(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    target: &RenameTarget,
    mut emit: impl FnMut(&Uri, &str, Range<usize>),
) {
    match target {
        RenameTarget::LocalIdent { uri, ident } => {
            let Some(cell) = manager.get(uri) else {
                return;
            };
            let doc = cell.borrow();
            let Some(module) = project.module(uri) else {
                return;
            };
            // Binding site.
            emit(uri, &doc.text, module.hir.idents[*ident].byte_range.clone());
            for (use_idx, def) in &module.resolutions.uses {
                let hits = matches!(
                    def,
                    Definition::Param(i) | Definition::Local(i) | Definition::Generic(i)
                        if i == ident
                );
                if hits {
                    emit(
                        uri,
                        &doc.text,
                        module.hir.idents[*use_idx].byte_range.clone(),
                    );
                }
            }
        }
        RenameTarget::ProjectDecl {
            uri: target_uri,
            decl: target_decl,
        } => {
            // Home module: binding site + same-module Decl uses.
            if let Some(home_cell) = manager.get(target_uri)
                && let Some(home_module) = project.module(target_uri)
            {
                let home_doc = home_cell.borrow();
                if let Some(name_idx) = home_module.hir.decls[*target_decl].name() {
                    emit(
                        target_uri,
                        &home_doc.text,
                        home_module.hir.idents[name_idx].byte_range.clone(),
                    );
                }
                for (use_idx, def) in &home_module.resolutions.uses {
                    if matches!(def, Definition::Decl(d) if d == target_decl) {
                        emit(
                            target_uri,
                            &home_doc.text,
                            home_module.hir.idents[*use_idx].byte_range.clone(),
                        );
                    }
                }
            }
            // Importers: every other module's ProjectDecl uses with
            // matching (uri, decl).
            for (other_uri, other_module) in project.iter() {
                if other_uri == target_uri {
                    continue;
                }
                let Some(other_cell) = manager.get(other_uri) else {
                    continue;
                };
                let other_doc = other_cell.borrow();
                for (use_idx, def) in &other_module.resolutions.uses {
                    if let Definition::ProjectDecl { uri, decl } = def
                        && uri == target_uri
                        && decl == target_decl
                    {
                        emit(
                            other_uri,
                            &other_doc.text,
                            other_module.hir.idents[*use_idx].byte_range.clone(),
                        );
                    }
                }
            }
        }
        RenameTarget::TypeAttr {
            uri: target_uri,
            attr: target_attr,
        } => {
            use greycat_analyzer_analysis::analyzer::MemberDef;
            // Home module: binding site + same-module member_uses.
            if let Some(home_cell) = manager.get(target_uri)
                && let Some(home_module) = project.module(target_uri)
            {
                let home_doc = home_cell.borrow();
                let name_idx = home_module.hir.type_attrs[*target_attr].name;
                emit(
                    target_uri,
                    &home_doc.text,
                    home_module.hir.idents[name_idx].byte_range.clone(),
                );
                for (use_idx, member) in &home_module.analysis.member_uses {
                    if matches!(member, MemberDef::Attr(a) if a == target_attr) {
                        emit(
                            target_uri,
                            &home_doc.text,
                            home_module.hir.idents[*use_idx].byte_range.clone(),
                        );
                    }
                }
            }
            // Importers: foreign_member_uses entries pointing at this
            // attr in the home module.
            for (other_uri, other_module) in project.iter() {
                if other_uri == target_uri {
                    continue;
                }
                let Some(other_cell) = manager.get(other_uri) else {
                    continue;
                };
                let other_doc = other_cell.borrow();
                for (use_idx, foreign) in &other_module.analysis.foreign_member_uses {
                    if foreign.uri == *target_uri
                        && matches!(foreign.member, MemberDef::Attr(a) if a == *target_attr)
                    {
                        emit(
                            other_uri,
                            &other_doc.text,
                            other_module.hir.idents[*use_idx].byte_range.clone(),
                        );
                    }
                }
            }
        }
        RenameTarget::TypeMethod {
            uri: target_uri,
            method: target_method,
        } => {
            use greycat_analyzer_analysis::analyzer::MemberDef;
            // Home module: method's own name + every same-module
            // member_uses pointing at it.
            if let Some(home_cell) = manager.get(target_uri)
                && let Some(home_module) = project.module(target_uri)
            {
                let home_doc = home_cell.borrow();
                if let Some(name_idx) = home_module.hir.decls[*target_method].name() {
                    emit(
                        target_uri,
                        &home_doc.text,
                        home_module.hir.idents[name_idx].byte_range.clone(),
                    );
                }
                for (use_idx, member) in &home_module.analysis.member_uses {
                    if matches!(member, MemberDef::Method(m) if m == target_method) {
                        emit(
                            target_uri,
                            &home_doc.text,
                            home_module.hir.idents[*use_idx].byte_range.clone(),
                        );
                    }
                }
            }
            // Importers: foreign_member_uses pointing at this method.
            for (other_uri, other_module) in project.iter() {
                if other_uri == target_uri {
                    continue;
                }
                let Some(other_cell) = manager.get(other_uri) else {
                    continue;
                };
                let other_doc = other_cell.borrow();
                for (use_idx, foreign) in &other_module.analysis.foreign_member_uses {
                    if foreign.uri == *target_uri
                        && matches!(foreign.member, MemberDef::Method(m) if m == *target_method)
                    {
                        emit(
                            other_uri,
                            &other_doc.text,
                            other_module.hir.idents[*use_idx].byte_range.clone(),
                        );
                    }
                }
            }
        }
    }
}
