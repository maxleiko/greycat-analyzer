//! Rename / find-references target resolution + project-wide site walk.
//!
//! The LSP-layer `rename` / `find-references` capabilities were previously
//! interleaving "where does this binding bind?" (analysis) with "render a
//! `Location` / `TextEdit` from a byte range" (LSP shape conversion). This
//! module owns the analysis half — given a cursor's binding it returns
//! every (URI, byte-range) pair the rename / references operation should
//! visit. The LSP layer then maps each site to whichever LSP shape the
//! request needs.
//!
//! Byte ranges are offsets into the home module's *source text* (i.e.
//! the same coordinates `Hir::idents[*].byte_range` carries). Callers
//! that need LSP positions consult the SourceManager / Document on the
//! LSP side.

use std::ops::Range;

use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_core::lsp_types::{Position, Uri};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{Decl, Ident, TypeAttr};
use greycat_analyzer_syntax::cst::node_at_offset;
use greycat_analyzer_syntax::tree_sitter;

use crate::analyzer::MemberDef;
use crate::conv::position_to_byte;
use crate::project::ProjectAnalysis;
use crate::resolver::Definition;

/// Map a cursor position in `text` to its `Idx<Ident>` against `hir`'s
/// `idents` arena, by byte-range match. Returns `None` if the cursor
/// isn't over an ident or no matching idx was allocated (e.g. lowering
/// skipped this shape).
pub fn cursor_ident_idx(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    hir: &Hir,
    encoding: SourceEncoding,
) -> Option<Idx<Ident>> {
    let byte = position_to_byte(text, pos, encoding);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return None;
    }
    hir.idents
        .iter()
        .find(|(_, i)| i.byte_range == node.byte_range())
        .map(|(idx, _)| idx)
}

/// What the cursor is asking us to rename / find references for.
/// Returned by [`resolve_target`] and consumed by [`target_sites`].
#[derive(Debug, Clone)]
pub enum RenameTarget {
    /// Function parameter / local var / generic-param. Confined to its
    /// declaring module's scope — no cross-module fan-out.
    LocalIdent { uri: Uri, ident: Idx<Ident> },
    /// Top-level decl. May be referenced from any module via
    /// [`Definition::Decl`] (in the home module) or
    /// [`Definition::ProjectDecl`] (importers).
    ProjectDecl { uri: Uri, decl: Idx<Decl> },
    /// A type attribute (`type Foo { name: String; }`). Use sites
    /// bind via the analyzer's `member_uses` / `foreign_member_uses`
    /// maps, not via the resolver — so a separate target shape is
    /// needed.
    TypeAttr { uri: Uri, attr: Idx<TypeAttr> },
    /// A type method (`type Foo { fn m() ... }`). The method itself
    /// is a `Decl::Fn` (with its own `Idx<Decl>`), but use sites
    /// resolve through `member_uses` / `foreign_member_uses` rather
    /// than `Resolutions::uses`. Distinct from [`Self::ProjectDecl`]
    /// so [`target_sites`] can fan out over the member-use maps
    /// instead of decl-use maps.
    TypeMethod { uri: Uri, method: Idx<Decl> },
}

/// One occurrence of a rename target within a module's source text.
/// `byte_range` indexes into the document at `uri`.
#[derive(Debug, Clone)]
pub struct TargetSite {
    pub uri: Uri,
    pub byte_range: Range<usize>,
}

/// Inspect the cursor's binding through cached project analysis and
/// classify the rename / reference target. Returns `None` for cursors
/// not on an ident, runtime-only names ([`Definition::Project`]
/// `Array`, `Map`, native fns, primitives), and unrecognized binding
/// shapes.
pub fn resolve_target(
    project: &ProjectAnalysis,
    cursor_uri: &Uri,
    cursor_idx: Idx<Ident>,
) -> Option<RenameTarget> {
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

/// Collect every site referencing `target` across the project. Pure
/// analysis — no source text needed since byte ranges already live in
/// the cached HIR. Order isn't guaranteed; callers that care should
/// sort by `(uri, byte_range.start)`.
pub fn target_sites(project: &ProjectAnalysis, target: &RenameTarget) -> Vec<TargetSite> {
    let mut out = Vec::new();
    match target {
        RenameTarget::LocalIdent { uri, ident } => {
            let Some(module) = project.module(uri) else {
                return out;
            };
            // Binding site.
            out.push(TargetSite {
                uri: uri.clone(),
                byte_range: module.hir.idents[*ident].byte_range.clone(),
            });
            for (use_idx, def) in &module.resolutions.uses {
                let hits = matches!(
                    def,
                    Definition::Param(i) | Definition::Local(i) | Definition::Generic(i)
                        if i == ident
                );
                if hits {
                    out.push(TargetSite {
                        uri: uri.clone(),
                        byte_range: module.hir.idents[*use_idx].byte_range.clone(),
                    });
                }
            }
        }
        RenameTarget::ProjectDecl {
            uri: target_uri,
            decl: target_decl,
        } => {
            // Home module: binding site + same-module Decl uses.
            if let Some(home_module) = project.module(target_uri) {
                if let Some(name_idx) = home_module.hir.decls[*target_decl].name() {
                    out.push(TargetSite {
                        uri: target_uri.clone(),
                        byte_range: home_module.hir.idents[name_idx].byte_range.clone(),
                    });
                }
                for (use_idx, def) in &home_module.resolutions.uses {
                    if matches!(def, Definition::Decl(d) if d == target_decl) {
                        out.push(TargetSite {
                            uri: target_uri.clone(),
                            byte_range: home_module.hir.idents[*use_idx].byte_range.clone(),
                        });
                    }
                }
            }
            // Importers: every other module's ProjectDecl uses with
            // matching (uri, decl).
            for (other_uri, other_module) in project.iter() {
                if other_uri == target_uri {
                    continue;
                }
                for (use_idx, def) in &other_module.resolutions.uses {
                    if let Definition::ProjectDecl { uri, decl } = def
                        && uri == target_uri
                        && decl == target_decl
                    {
                        out.push(TargetSite {
                            uri: other_uri.clone(),
                            byte_range: other_module.hir.idents[*use_idx].byte_range.clone(),
                        });
                    }
                }
            }
        }
        RenameTarget::TypeAttr {
            uri: target_uri,
            attr: target_attr,
        } => {
            // Home module: binding site + same-module member_uses.
            if let Some(home_module) = project.module(target_uri) {
                let name_idx = home_module.hir.type_attrs[*target_attr].name;
                out.push(TargetSite {
                    uri: target_uri.clone(),
                    byte_range: home_module.hir.idents[name_idx].byte_range.clone(),
                });
                for (use_idx, member) in &home_module.analysis.member_uses {
                    if matches!(member, MemberDef::Attr(a) if a == target_attr) {
                        out.push(TargetSite {
                            uri: target_uri.clone(),
                            byte_range: home_module.hir.idents[*use_idx].byte_range.clone(),
                        });
                    }
                }
            }
            // Importers: foreign_member_uses entries pointing at this
            // attr in the home module.
            for (other_uri, other_module) in project.iter() {
                if other_uri == target_uri {
                    continue;
                }
                for (use_idx, foreign) in &other_module.analysis.foreign_member_uses {
                    if foreign.uri == *target_uri
                        && matches!(foreign.member, MemberDef::Attr(a) if a == *target_attr)
                    {
                        out.push(TargetSite {
                            uri: other_uri.clone(),
                            byte_range: other_module.hir.idents[*use_idx].byte_range.clone(),
                        });
                    }
                }
            }
        }
        RenameTarget::TypeMethod {
            uri: target_uri,
            method: target_method,
        } => {
            // Home module: method's own name + every same-module
            // member_uses pointing at it.
            if let Some(home_module) = project.module(target_uri) {
                if let Some(name_idx) = home_module.hir.decls[*target_method].name() {
                    out.push(TargetSite {
                        uri: target_uri.clone(),
                        byte_range: home_module.hir.idents[name_idx].byte_range.clone(),
                    });
                }
                for (use_idx, member) in &home_module.analysis.member_uses {
                    if matches!(member, MemberDef::Method(m) if m == target_method) {
                        out.push(TargetSite {
                            uri: target_uri.clone(),
                            byte_range: home_module.hir.idents[*use_idx].byte_range.clone(),
                        });
                    }
                }
            }
            // Importers: foreign_member_uses pointing at this method.
            for (other_uri, other_module) in project.iter() {
                if other_uri == target_uri {
                    continue;
                }
                for (use_idx, foreign) in &other_module.analysis.foreign_member_uses {
                    if foreign.uri == *target_uri
                        && matches!(foreign.member, MemberDef::Method(m) if m == *target_method)
                    {
                        out.push(TargetSite {
                            uri: other_uri.clone(),
                            byte_range: other_module.hir.idents[*use_idx].byte_range.clone(),
                        });
                    }
                }
            }
        }
    }
    out
}
