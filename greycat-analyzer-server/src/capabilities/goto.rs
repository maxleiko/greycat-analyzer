//! Goto definition / declaration / implementation handlers.
//! Both single-file and project-aware variants live here, alongside
//! the `cursor_ident_idx` helper that references_rename also reuses.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_analysis::resolver::{Definition, resolve};
use greycat_analyzer_core::{SourceManager, SymbolTable};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::Decl;
use greycat_analyzer_syntax::cst::node_at_offset;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{GotoDefinitionResponse, Location, Position, Uri};

use super::references_rename;
use crate::conv::{byte_range_to_lsp, position_to_byte};

pub fn goto_definition(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    uri: &Uri,
    pos: Position,
) -> Option<GotoDefinitionResponse> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return None;
    }

    let symbols = SymbolTable::new();
    let hir = lower_module(text, &symbols, "module", lib, root);
    let resolutions = resolve(&hir, &symbols);

    // Find which Idx<Ident> this CST node corresponds to.
    let ident_text = text.get(node.byte_range())?;
    let target = hir
        .idents
        .iter()
        .find(|(_, i)| i.byte_range == node.byte_range() && &symbols[i.symbol] == ident_text)?
        .0;

    if let Some(def) = resolutions.lookup(target) {
        let target_range = match def {
            Definition::Decl(decl_id) => {
                let name = hir.decls[decl_id].name()?;
                hir.idents[name].byte_range.clone()
            }
            Definition::Local(name) | Definition::Param(name) | Definition::Generic(name) => {
                hir.idents[name].byte_range.clone()
            }
            // P11.2 records the cross-module decl pointer here, but
            // resolving it to a `Location` requires reading the foreign
            // module's text — that's P11.3. For now fall through so
            // the member-access lookup below still runs.
            Definition::ProjectDecl { .. } | Definition::Project => return None,
        };
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: byte_range_to_lsp(text, &target_range),
        }));
    }

    // P6.3: the property side of `a.b` / `a->b` isn't in `Resolutions`
    // — bindings live in `AnalysisResult::member_uses`. Run the
    // analyzer to consult it before giving up.
    let (_arena, _decl_registry, analysis) =
        greycat_analyzer_analysis::analyzer::analyze(&hir, &resolutions, &symbols);
    let member = analysis.member_lookup(target)?;
    let target_range = match member {
        greycat_analyzer_analysis::analyzer::MemberDef::Attr(attr_id) => {
            let name = hir.type_attrs[attr_id].name;
            hir.idents[name].byte_range.clone()
        }
        greycat_analyzer_analysis::analyzer::MemberDef::Method(decl_id) => {
            let name = hir.decls[decl_id].name()?;
            hir.idents[name].byte_range.clone()
        }
    };
    Some(GotoDefinitionResponse::Scalar(Location {
        uri: uri.clone(),
        range: byte_range_to_lsp(text, &target_range),
    }))
}

// P15.9
/// Goto-def on a module-name segment of a `static_expr` chain.
/// In `runtime::Identity::create`, the leftmost ident `runtime` names
/// the module that owns `Identity`. This helper checks whether the
/// cursor sits on the leftmost segment of such a chain and, if so,
/// returns the URI of the matching `.gcl` file (jumping to its first
/// line). Returns `None` otherwise — caller falls through to the
/// regular goto-def flow.
pub fn goto_module_segment(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    manager: &SourceManager,
) -> Option<Location> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return None;
    }
    // The leftmost `type_ident` in a `static_expr` chain is the
    // module-name slot. Walk up to confirm the parent shape.
    let parent = node.parent()?;
    if parent.kind() != "type_ident" {
        return None;
    }
    let static_parent = parent.parent()?;
    if static_parent.kind() != "static_expr" {
        return None;
    }
    let cursor_text = text.get(node.byte_range())?.to_string();
    // Match against any cached doc whose `name()` matches the cursor
    // text. `Document::name()` is the filename without `.gcl`, which
    // is the convention GreyCat's `runtime::X` chains rely on.
    for (uri, cell) in manager.iter() {
        let doc = cell.borrow();
        if doc.name() == cursor_text {
            return Some(Location {
                uri: uri.clone(),
                range: lsp_types::Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 0,
                    },
                },
            });
        }
    }
    None
}

// P11.3
/// Turn a `Definition::ProjectDecl { uri, decl }` into the
/// concrete `Location` of the foreign module's decl-name range. Pure
/// helper: caller fetches the foreign HIR + text from the project-
/// analysis cache + source manager and passes them in.
pub fn cross_module_decl_location(
    foreign_uri: &Uri,
    foreign_text: &str,
    foreign_hir: &Hir,
    decl_id: greycat_analyzer_hir::arena::Idx<Decl>,
) -> Option<Location> {
    let name_id = foreign_hir.decls[decl_id].name()?;
    let range = byte_range_to_lsp(foreign_text, &foreign_hir.idents[name_id].byte_range);
    Some(Location {
        uri: foreign_uri.clone(),
        range,
    })
}

// P11.5
/// Turn a `ForeignMember` (cross-module attr / method
/// binding) into a `Location` pointing at the foreign attr / method's
/// name range. Mirrors [`cross_module_decl_location`] but indexes
/// `type_attrs` for `MemberDef::Attr` and `decls` for `Method`.
pub fn cross_module_member_location(
    foreign_uri: &Uri,
    foreign_text: &str,
    foreign_hir: &Hir,
    member: &greycat_analyzer_analysis::analyzer::MemberDef,
) -> Option<Location> {
    use greycat_analyzer_analysis::analyzer::MemberDef;
    let range = match *member {
        MemberDef::Attr(attr_id) => {
            let name_id = foreign_hir.type_attrs[attr_id].name;
            foreign_hir.idents[name_id].byte_range.clone()
        }
        MemberDef::Method(decl_id) => {
            let name_id = foreign_hir.decls[decl_id].name()?;
            foreign_hir.idents[name_id].byte_range.clone()
        }
    };
    Some(Location {
        uri: foreign_uri.clone(),
        range: byte_range_to_lsp(foreign_text, &range),
    })
}

///  helper — map a cursor position in `text` to its `Idx<Ident>`
/// against the cached `hir`'s `idents` arena, by byte-range match.
/// Returns `None` if the cursor isn't over an ident or no matching
/// idx was allocated (e.g. lowering skipped this shape).
pub fn cursor_ident_idx(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    hir: &Hir,
) -> Option<greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return None;
    }
    references_rename::idx_for_node(hir, node)
}

// P31.1
/// `textDocument/definition` with project context. Mirrors the
/// dispatcher chain the LSP handler runs:
///
/// 1. Module-name segment (`runtime::X`) — jump to that module's file.
/// 2. In-module [`goto_definition`] — resolver / local member_uses path.
/// 3. Cross-module fallback via `Definition::ProjectDecl` (foreign
///    top-level decl).
/// 4. Cross-module member chain segment (`foreign_decl_lookup`).
/// 5. Cross-module member access — `foreign_member_uses`, which the
///    analyzer's `resolve_member_with` populates for inherited members
///    (`type Sub extends Base` + `s.method` lands on `Base::method`).
///
/// Returns `None` if no rule fires.
pub fn goto_definition_across_project(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
) -> Option<GotoDefinitionResponse> {
    let cell = manager.get(cursor_uri)?;
    let doc = cell.borrow();
    if let Some(loc) = goto_module_segment(&doc.text, doc.root_node(), cursor_pos, manager) {
        return Some(GotoDefinitionResponse::Scalar(loc));
    }
    if let Some(loc) = goto_definition(&doc.text, &doc.lib, doc.root_node(), cursor_uri, cursor_pos)
    {
        return Some(loc);
    }
    let module = project.module(cursor_uri)?;
    let cursor_idx = cursor_ident_idx(&doc.text, doc.root_node(), cursor_pos, &module.hir)?;

    if let Some(Definition::ProjectDecl {
        uri: foreign_uri,
        decl,
    }) = module.resolutions.lookup(cursor_idx)
    {
        drop(doc);
        let foreign_module = project.module(&foreign_uri)?;
        let foreign_cell = manager.get(&foreign_uri)?;
        let foreign_doc = foreign_cell.borrow();
        return cross_module_decl_location(
            &foreign_uri,
            &foreign_doc.text,
            &foreign_module.hir,
            decl,
        )
        .map(GotoDefinitionResponse::Scalar);
    }
    if let Some(fdecl) = module.analysis.foreign_decl_lookup(cursor_idx) {
        let foreign_uri = fdecl.uri.clone();
        let decl = fdecl.decl;
        drop(doc);
        let foreign_module = project.module(&foreign_uri)?;
        let foreign_cell = manager.get(&foreign_uri)?;
        let foreign_doc = foreign_cell.borrow();
        return cross_module_decl_location(
            &foreign_uri,
            &foreign_doc.text,
            &foreign_module.hir,
            decl,
        )
        .map(GotoDefinitionResponse::Scalar);
    }
    let foreign = module.analysis.foreign_member_lookup(cursor_idx)?;
    let foreign_uri = foreign.uri.clone();
    let member = foreign.member;
    drop(doc);
    let foreign_module = project.module(&foreign_uri)?;
    let foreign_cell = manager.get(&foreign_uri)?;
    let foreign_doc = foreign_cell.borrow();
    cross_module_member_location(
        &foreign_uri,
        &foreign_doc.text,
        &foreign_module.hir,
        &member,
    )
    .map(GotoDefinitionResponse::Scalar)
}

// P8.6
/// `textDocument/implementation`. For a method-name ident,
/// returns every concrete (non-`abstract`, non-`native`) method with
/// that name across all type decls in the module. For other idents,
/// falls through to [`goto_definition`] so the editor still produces
/// a useful jump.
pub fn goto_implementation(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    uri: &Uri,
    pos: Position,
) -> Option<GotoDefinitionResponse> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return goto_definition(text, lib, root, uri, pos);
    }
    let cursor_text = text.get(node.byte_range())?.to_string();

    let symbols = SymbolTable::new();
    let hir = lower_module(text, &symbols, "module", lib, root);
    let mut locations = Vec::new();
    let Some(module) = hir.module.as_ref() else {
        return goto_definition(text, lib, root, uri, pos);
    };
    for decl_id in &module.decls {
        if let Decl::Type(td) = &hir.decls[*decl_id] {
            for method_id in &td.methods {
                if let Decl::Fn(fnd) = &hir.decls[*method_id] {
                    if fnd.modifiers.abstract_ || fnd.modifiers.native {
                        continue;
                    }
                    if symbols[hir.idents[fnd.name].symbol] == *cursor_text {
                        locations.push(Location {
                            uri: uri.clone(),
                            range: byte_range_to_lsp(text, &hir.idents[fnd.name].byte_range),
                        });
                    }
                }
            }
        }
    }
    if locations.is_empty() {
        return goto_definition(text, lib, root, uri, pos);
    }
    Some(GotoDefinitionResponse::Array(locations))
}

// P11.6 + P31.2
/// Project-wide `textDocument/implementation`. For a method-name
/// ident, returns every *concrete* (non-`abstract`, non-`native`)
/// method that:
///
/// - is named the same as the cursor's ident, AND
/// - belongs to a type that is a subtype of (or equal to) the
///   *declaring type* — the type that owns the method binding at
///   the cursor.
///
/// The subtype filter drops the pre-P31.2 false positives where
/// unrelated types coincidentally shared a method name. Falls
/// through to in-module [`goto_implementation`] (which itself falls
/// through to [`goto_definition`]) for non-method idents and when
/// the declaring type can't be determined.
pub fn goto_implementation_across_project(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
) -> Option<GotoDefinitionResponse> {
    let cell = manager.get(cursor_uri)?;
    let doc = cell.borrow();
    let byte = position_to_byte(&doc.text, cursor_pos);
    let node = node_at_offset(doc.root_node(), byte)?;
    if node.kind() != "ident" {
        return goto_implementation(&doc.text, &doc.lib, doc.root_node(), cursor_uri, cursor_pos);
    }
    let cursor_text = doc.text.get(node.byte_range())?.to_string();
    drop(doc);

    // Cursor on a type name (binding site or use site) → return every
    // concrete subtype across the project. Tried before the method
    // path because the type-name shape can't be a method ident, and
    // the method path won't match types.
    if let Some(target_type) = type_target_for_cursor(project, manager, cursor_uri, cursor_pos)
        && let Some(resp) = type_implementations(project, manager, &target_type)
    {
        return Some(resp);
    }

    let Some(declaring_type) =
        declaring_type_for_method_cursor(project, manager, cursor_uri, cursor_pos)
    else {
        // No declaring type → fall through to the naive in-module path.
        let cell = manager.get(cursor_uri)?;
        let doc = cell.borrow();
        return goto_implementation(&doc.text, &doc.lib, doc.root_node(), cursor_uri, cursor_pos);
    };

    let mut locations = Vec::new();
    for (uri, module) in project.iter() {
        let Some(module_root) = module.hir.module.as_ref() else {
            continue;
        };
        let Some(other_cell) = manager.get(uri) else {
            continue;
        };
        let other_doc = other_cell.borrow();
        for decl_id in &module_root.decls {
            let Decl::Type(td) = &module.hir.decls[*decl_id] else {
                continue;
            };
            let candidate_type = &project.symbols()[module.hir.idents[td.name].symbol];
            if !project
                .index
                .is_subtype_of(candidate_type, declaring_type.as_str())
            {
                continue;
            }
            for method_id in &td.methods {
                let Decl::Fn(fnd) = &module.hir.decls[*method_id] else {
                    continue;
                };
                if fnd.modifiers.abstract_ || fnd.modifiers.native {
                    continue;
                }
                if project.symbols()[module.hir.idents[fnd.name].symbol] == *cursor_text {
                    locations.push(Location {
                        uri: uri.clone(),
                        range: byte_range_to_lsp(
                            &other_doc.text,
                            &module.hir.idents[fnd.name].byte_range,
                        ),
                    });
                }
            }
        }
    }
    if locations.is_empty() {
        let cell = manager.get(cursor_uri)?;
        let doc = cell.borrow();
        return goto_implementation(&doc.text, &doc.lib, doc.root_node(), cursor_uri, cursor_pos);
    }
    Some(GotoDefinitionResponse::Array(locations))
}

// P31.3
/// `textDocument/declaration`. Inverse of
/// [`goto_implementation_across_project`]: given the cursor on a
/// concrete method override, jump to the abstract ancestor that
/// declares the method. Walks the supertype chain of the cursor's
/// declaring type, returning the first ancestor whose method with
/// the same name carries the `abstract` modifier.
///
/// Falls through to [`goto_definition_across_project`] when:
/// - no declaring type can be resolved (cursor isn't on a method
///   ident the analyzer can bind), or
/// - the declaring type has no abstract ancestor for this method
///   (the cursor's method has no abstract parent — declaration ==
///   definition).
pub fn goto_declaration_across_project(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
) -> Option<GotoDefinitionResponse> {
    let cell = manager.get(cursor_uri)?;
    let doc = cell.borrow();
    let byte = position_to_byte(&doc.text, cursor_pos);
    let node = node_at_offset(doc.root_node(), byte)?;
    if node.kind() != "ident" {
        return goto_definition_across_project(project, manager, cursor_uri, cursor_pos);
    }
    let cursor_text = doc.text.get(node.byte_range())?.to_string();
    drop(doc);

    let Some(declaring_type) =
        declaring_type_for_method_cursor(project, manager, cursor_uri, cursor_pos)
    else {
        return goto_definition_across_project(project, manager, cursor_uri, cursor_pos);
    };

    let ancestor = project
        .index
        .find_abstract_ancestor_method(declaring_type.as_str(), cursor_text.as_str());
    let Some((foreign_uri, decl_id)) = ancestor else {
        // No abstract ancestor — fall through to goto-definition so
        // the client still produces a useful jump for the cursor.
        return goto_definition_across_project(project, manager, cursor_uri, cursor_pos);
    };

    let foreign_module = project.module(&foreign_uri)?;
    let foreign_cell = manager.get(&foreign_uri)?;
    let foreign_doc = foreign_cell.borrow();
    let name_id = foreign_module.hir.decls[decl_id].name()?;
    let range = foreign_module.hir.idents[name_id].byte_range.clone();
    Some(GotoDefinitionResponse::Scalar(Location {
        uri: foreign_uri.clone(),
        range: byte_range_to_lsp(&foreign_doc.text, &range),
    }))
}

/// Recognise a cursor sitting on a type-name ident — at a binding
/// site (`type Foo {}`'s `Foo`), at a type-ref use (`var x: Foo`,
/// `T extends Foo`, `Array<Foo>`), or at a static-receiver chain head
/// (`Foo::bar`). Returns the canonical type name, which
/// [`type_implementations`] then drives a project-wide subtype scan
/// against.
fn type_target_for_cursor(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
) -> Option<String> {
    let cell = manager.get(cursor_uri)?;
    let doc = cell.borrow();
    let module = project.module(cursor_uri)?;
    let cursor_idx = cursor_ident_idx(&doc.text, doc.root_node(), cursor_pos, &module.hir)?;
    drop(doc);

    // Binding-site: cursor on a `type Foo {}` / `enum Foo` ident.
    if let Some(module_root) = module.hir.module.as_ref() {
        for decl_id in &module_root.decls {
            let name_idx = match &module.hir.decls[*decl_id] {
                Decl::Type(td) => Some(td.name),
                Decl::Enum(ed) => Some(ed.name),
                _ => None,
            };
            if name_idx == Some(cursor_idx) {
                return Some(project.symbols()[module.hir.idents[cursor_idx].symbol].to_string());
            }
        }
    }

    // Use-site: resolver bound this ident to a type decl (local
    // `Definition::Decl` or cross-module `Definition::ProjectDecl`).
    let def = module.resolutions.lookup(cursor_idx)?;
    let (home_uri, decl_idx) = match def {
        Definition::Decl(d) => (cursor_uri.clone(), d),
        Definition::ProjectDecl { uri, decl } => (uri, decl),
        _ => return None,
    };
    let home_module = project.module(&home_uri)?;
    match &home_module.hir.decls[decl_idx] {
        Decl::Type(td) => {
            Some(project.symbols()[home_module.hir.idents[td.name].symbol].to_string())
        }
        Decl::Enum(ed) => {
            Some(project.symbols()[home_module.hir.idents[ed.name].symbol].to_string())
        }
        _ => None,
    }
}

/// Return every concrete (non-`abstract`, non-`native`) subtype of
/// `target_type` across the project as goto-implementation locations.
/// Returns `None` when no subtypes exist (so the caller can fall
/// through to the method path). The target type itself is included
/// when it is concrete — matches the existing
/// `goto_impl_on_no_inheritance_returns_only_self` convention.
fn type_implementations(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    target_type: &str,
) -> Option<GotoDefinitionResponse> {
    let mut locations = Vec::new();
    for (uri, module) in project.iter() {
        let Some(module_root) = module.hir.module.as_ref() else {
            continue;
        };
        let Some(cell) = manager.get(uri) else {
            continue;
        };
        let doc = cell.borrow();
        for decl_id in &module_root.decls {
            let Decl::Type(td) = &module.hir.decls[*decl_id] else {
                continue;
            };
            // `native` types live in the runtime — they have no
            // userland implementation worth jumping to.
            if td.modifiers.abstract_ || td.modifiers.native {
                continue;
            }
            let candidate = &project.symbols()[module.hir.idents[td.name].symbol];
            if !project.index.is_subtype_of(candidate, target_type) {
                continue;
            }
            locations.push(Location {
                uri: uri.clone(),
                range: byte_range_to_lsp(&doc.text, &module.hir.idents[td.name].byte_range),
            });
        }
    }
    if locations.is_empty() {
        return None;
    }
    Some(GotoDefinitionResponse::Array(locations))
}

/// Determine the *declaring type* of the method-name ident under the
/// cursor — the type whose method declaration / binding the cursor
/// is associated with. Used as the root of the subtype filter for
/// `textDocument/implementation` and as the starting point for
/// `textDocument/declaration`'s supertype walk.
///
/// Resolution order:
/// 1. Cursor on a method's own declaration site (`type Foo { fn name() {} }`)
///    → returns `Foo`.
/// 2. Cursor on a member access (`recv.name` / `recv->name`) whose
///    binding is in the cursor module's `member_uses`. Find the
///    `Decl::Type` whose methods contain the bound `Idx<Decl>` and
///    return its name.
/// 3. Same shape but the binding is in `foreign_member_uses` — walk
///    the foreign module's HIR for the owning type.
fn declaring_type_for_method_cursor(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
) -> Option<String> {
    let cell = manager.get(cursor_uri)?;
    let doc = cell.borrow();
    let module = project.module(cursor_uri)?;
    let cursor_idx = cursor_ident_idx(&doc.text, doc.root_node(), cursor_pos, &module.hir)?;
    let cursor_sym = module.hir.idents[cursor_idx].symbol;
    let cursor_range = module.hir.idents[cursor_idx].byte_range.clone();
    drop(doc);

    // Case 1: cursor on a method's declaration site in this module.
    if let Some(module_root) = module.hir.module.as_ref() {
        for decl_id in &module_root.decls {
            let Decl::Type(td) = &module.hir.decls[*decl_id] else {
                continue;
            };
            for method_id in &td.methods {
                let Decl::Fn(fnd) = &module.hir.decls[*method_id] else {
                    continue;
                };
                let name_range = &module.hir.idents[fnd.name].byte_range;
                if *name_range == cursor_range && module.hir.idents[fnd.name].symbol == cursor_sym {
                    return Some(project.symbols()[module.hir.idents[td.name].symbol].to_string());
                }
            }
        }
    }

    // Case 2: cursor on a member access bound in the local module.
    use greycat_analyzer_analysis::analyzer::MemberDef;
    if let Some(MemberDef::Method(decl_id)) = module.analysis.member_lookup(cursor_idx)
        && let Some(module_root) = module.hir.module.as_ref()
    {
        for type_decl_id in &module_root.decls {
            let Decl::Type(td) = &module.hir.decls[*type_decl_id] else {
                continue;
            };
            if td.methods.contains(&decl_id) {
                return Some(project.symbols()[module.hir.idents[td.name].symbol].to_string());
            }
        }
    }

    // Case 3: cursor on a member access bound to a foreign module.
    let foreign = module.analysis.foreign_member_lookup(cursor_idx)?;
    let MemberDef::Method(decl_id) = foreign.member else {
        return None;
    };
    let foreign_module = project.module(&foreign.uri)?;
    let foreign_root = foreign_module.hir.module.as_ref()?;
    for type_decl_id in &foreign_root.decls {
        let Decl::Type(td) = &foreign_module.hir.decls[*type_decl_id] else {
            continue;
        };
        if td.methods.contains(&decl_id) {
            return Some(project.symbols()[foreign_module.hir.idents[td.name].symbol].to_string());
        }
    }
    None
}
