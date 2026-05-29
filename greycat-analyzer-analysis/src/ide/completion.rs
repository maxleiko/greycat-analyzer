//! Completion subsystem — directive / pragma / library-version / scope /
//! member / static / type-position / object-field completion. This is
//! ~2700 lines of dense logic largely independent from the rest of the
//! analysis pipeline. Emits IDE-shape `CompletionItem` ADTs decoupled
//! from `lsp_types`; the LSP server's `capabilities/completion.rs`
//! converts to `lsp_types::CompletionItem` at the wire boundary and the
//! wasm bridge consumes the same shape unchanged.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use greycat_analyzer_core::lsp_types::{Position, Uri};
use greycat_analyzer_core::{
    ItemId, SourceEncoding, Symbol, SymbolTable, TypeArena, TypeId, TypeKind,
};
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::Decl;
use greycat_analyzer_syntax::cst::node_at_offset;
use greycat_analyzer_syntax::tree_sitter;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::conv::{position_to_byte, stmt_byte_range};
use crate::ide::render::{
    RenderCtx, decl_doc, module_label_for_uri, render_decl_signature, render_fn_signature_compact,
    render_type_ref_with_subst,
};
use crate::ide::types::{Range, TextEdit};
use crate::project::{ModuleAnalysis, ProjectAnalysis};

/// IDE-shape `CompletionItemKind` — mirror of the subset of
/// `lsp_types::CompletionItemKind` constants that the analyzer
/// produces. Field-for-field crossable to LSP at the wire boundary;
/// also reachable from the wasm bridge without `lsp_types` in scope.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionItemKind {
    Function,
    Method,
    Variable,
    Field,
    Class,
    Enum,
    EnumMember,
    Constant,
    Module,
    Folder,
    Keyword,
    Text,
    TypeParameter,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertTextFormat {
    PlainText,
    Snippet,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct CompletionItemLabelDetails {
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub detail: Option<String>,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub description: Option<String>,
}

/// IDE-shape completion item. Field set matches the analyzer's actual
/// production surface; the LSP / wasm shapes wrap or expose this as-is.
///
/// `data` carries an opaque server-side payload for the `@library`
/// version-completion placeholder. It is wasm-skipped — JS consumers
/// reach it through [`extract_lib_version_placeholder`] rather than
/// inspecting the raw JSON. `text_edit` is the simplified single-range
/// edit shape (no insert / replace split — the analyzer never emits the
/// LSP `InsertReplaceEdit` variant), so the server wrapper lifts it
/// into `CompletionTextEdit::Edit(...)` at the wire boundary.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Default)]
pub struct CompletionItem {
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub label: String,
    pub kind: Option<CompletionItemKind>,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub detail: Option<String>,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub documentation: Option<String>,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub insert_text: Option<String>,
    pub insert_text_format: Option<InsertTextFormat>,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub sort_text: Option<String>,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub filter_text: Option<String>,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub label_details: Option<CompletionItemLabelDetails>,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub text_edit: Option<TextEdit>,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub additional_text_edits: Option<Vec<TextEdit>>,
    #[cfg_attr(feature = "wasm", wasm_bindgen(skip))]
    pub data: Option<serde_json::Value>,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Default)]
pub struct CompletionList {
    pub is_incomplete: bool,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub items: Vec<CompletionItem>,
}

// P15.2.3
/// Completion with project context. Same dispatcher chain as
/// [`completion`], but the ident-position branch enumerates scope-
/// visible names (locals / params / generics / in-module decls) plus
/// the cross-module project surface (`ProjectIndex::values` /
/// `decl_locations` / primitives) alongside the keyword list. Typed
/// prefix filters all of them.
pub fn completion_with_project(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    uri: &Uri,
    project: &ProjectAnalysis,
    project_root: Option<&std::path::Path>,
    encoding: SourceEncoding,
) -> Option<CompletionList> {
    let byte = position_to_byte(text, pos, encoding);
    let node = node_at_offset(root, byte)?;

    // `// gcl…` directive completion is line-text based (not CST-node
    // based), so it runs before [`classify_slot`]: the cursor often sits
    // at the comment's trailing edge where `node_at_offset` lands *past*
    // the comment node. Skips the call-paren post-pass — directive items
    // aren't value items.
    if let Some(items) = directive_completion(text, byte) {
        return Some(CompletionList {
            is_incomplete: false,
            items,
        });
    }

    // Classify the cursor's slot once, then dispatch. This is an
    // *allowlist*: every emitter is reached only when the slot is
    // positively recognized, and full-scope (`Expr`) completion fires
    // ONLY at a genuine expression / statement position. Unrecognized
    // positions (`Comment` / `Declaration` / `NoCompletion`) yield
    // nothing rather than leaking the scope dump — see [`classify_slot`].
    let mut items = match classify_slot(text, node, byte) {
        // Inside a comment but not a `// gcl…` directive — nothing.
        CompletionSlot::Comment => return None,
        CompletionSlot::StringArg => {
            if let Some(items) = include_dir_completion(text, node, byte, project_root) {
                items
            } else if let Some(items) = library_version_completion(text, node, byte, encoding) {
                // P15.3 — emit the lazy placeholder. `completion_handler`
                // in server.rs intercepts the placeholder and runs
                // [`resolve_library_version_completion`] against its
                // [`RegistryFetcher`] before forwarding to the editor.
                return Some(CompletionList {
                    is_incomplete: true,
                    items,
                });
            } else {
                return None;
            }
        }
        CompletionSlot::PragmaName => pragma_completion(text, byte)?,
        CompletionSlot::Member => member_completion(text, root, byte, uri, project, encoding)?,
        CompletionSlot::Static => static_completion(text, byte, project, encoding)?,
        CompletionSlot::TypeRef => type_position_completion(text, node, byte, uri, project)?,
        CompletionSlot::ObjectFieldName { body } => {
            // A named-attr type resolves to its field names; an
            // unresolved head or a collection / tuple (no attrs) falls
            // back to expression completion for its positional element.
            match emit_object_field_names(text, body, byte, uri, project) {
                Some(items) => items,
                None => expr_completion(text, byte, uri, project, None)?,
            }
        }
        CompletionSlot::Expr { value } => {
            let expected = value.and_then(|v| resolve_expected(&v, uri, project));
            expr_completion(text, byte, uri, project, expected)?
        }
        CompletionSlot::Declaration | CompletionSlot::NoCompletion => return None,
    };
    apply_call_paren_snippet(&mut items, text, byte, encoding);
    Some(CompletionList {
        is_incomplete: false,
        items,
    })
}

/// The cursor's completion slot — the authoritative classification that
/// replaces the old detector chain + denylist gate. Carries CST node
/// refs (tied to the parse tree's lifetime) where an emitter needs them.
///
/// `Expr` is the *only* slot that produces full-scope completion. Its
/// `value` anchor (when known) lets the dispatcher resolve an *expected
/// type* for type-aware ranking — see [`ValueSlot`] / [`resolve_expected`].
enum CompletionSlot<'t> {
    /// Inside a `// …` comment — directive completion (`// gcl…`) or none.
    Comment,
    /// Inside a non-substitution `string` — pragma string-arg or none.
    StringArg,
    /// Right after `@` — pragma-name completion.
    PragmaName,
    /// After `.` / `->` — member completion.
    Member,
    /// After `::` (ident or `::"…"` string mode) — static completion.
    Static,
    /// Inside a `type_ident` slot — type-position completion.
    TypeRef,
    /// Object-literal field-name slot; `body` is the
    /// `object_initializers` / `object_fields` node.
    ObjectFieldName { body: tree_sitter::Node<'t> },
    /// Expression / statement position — scope + keywords + project
    /// surface. `value` anchors a known value slot for expected-type
    /// ranking; `None` is a bare statement / unrankable position.
    Expr { value: Option<ValueSlot<'t>> },
    /// A declaration-name slot (`fn na|me`, `type Fo|o`, param name, …)
    /// or other non-expression container — suppress completion.
    Declaration,
    /// No completion (unrecognized position).
    NoCompletion,
}

/// A value-bearing expression slot whose expected type the analyzer can
/// recover from the surrounding construct. Pure CST anchors — the HIR
/// lookups happen later in [`resolve_expected`], keeping [`classify_slot`]
/// free of analysis state.
enum ValueSlot<'t> {
    /// `Foo { x: | }` — expected type is attr `x`'s type. Anchor is the
    /// `object_field` node (its `name` child names the attr).
    FieldValue { object_field: tree_sitter::Node<'t> },
    /// `f(a, |)` — expected type is the callee's `params[index]`. Anchor
    /// is the callee CST node (`call_expr`'s `fn` child).
    CallArg {
        callee: tree_sitter::Node<'t>,
        index: usize,
    },
    /// `var x: T = |` — expected type is the binding's declared type.
    /// Anchor is the `var_decl` / `modvar` node (its `name` child).
    Initializer { binder: tree_sitter::Node<'t> },
}

/// Separator immediately preceding the cursor's typed prefix.
enum Sep {
    Dot,
    Arrow,
}

/// Inspect the byte(s) immediately before the cursor's `[A-Za-z0-9_]*`
/// prefix run. Mirrors the separator probe in [`member_completion`] so
/// the classifier and the emitter agree on what counts as member access.
fn separator_before_prefix(text: &str, cursor_byte: usize) -> Option<Sep> {
    let bytes = text.as_bytes();
    let prefix_len = ident_prefix_at_cursor(text, cursor_byte).len();
    let ps = cursor_byte.min(bytes.len()).saturating_sub(prefix_len);
    if ps >= 1 && bytes[ps - 1] == b'.' {
        return Some(Sep::Dot);
    }
    if ps >= 2 && bytes[ps - 2] == b'-' && bytes[ps - 1] == b'>' {
        return Some(Sep::Arrow);
    }
    None
}

/// Field name of `child` within `parent` (e.g. `"name"`, `"value"`,
/// `"type"`), or `None` for an unnamed child. Walks `parent`'s direct
/// children with a `TreeCursor` — the cheap, exact way to distinguish a
/// declaration-name slot from a value slot during the ancestor walk.
fn field_name_of(
    parent: tree_sitter::Node<'_>,
    child: tree_sitter::Node<'_>,
) -> Option<&'static str> {
    let mut c = parent.walk();
    if c.goto_first_child() {
        loop {
            if c.node().id() == child.id() {
                return c.field_name();
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Classify the cursor's completion slot. Two passes:
///
/// 1. **Lexical / operator / comment / string** — contexts defined by a
///    trailing operator or enclosing comment / string that survive the
///    common incomplete-parse case (the cursor sits on a fresh prefix).
/// 2. **Structural ancestor walk** — the cursor's enclosing construct
///    decides the slot, with the field name of the child we ascended
///    from distinguishing a declaration name from a value position.
fn classify_slot<'t>(
    text: &str,
    node: tree_sitter::Node<'t>,
    cursor_byte: usize,
) -> CompletionSlot<'t> {
    // ---- Pass 0 ----
    // Comments own their slot entirely (directive completion or none).
    for k in ["line_comment", "_block_comment", "doc_comment"] {
        if ancestor_with_kind(node, k).is_some() {
            return CompletionSlot::Comment;
        }
    }
    // Strings: a real expression inside `${…}` falls through to Pass 1;
    // `Recv::"…|"` static-string mode routes to Static; any other string
    // content is at most a pragma string-arg.
    if ancestor_with_kind(node, "string").is_some()
        && ancestor_with_kind(node, "string_substitution").is_none()
    {
        if static_receiver_at(text, cursor_byte).is_some() {
            return CompletionSlot::Static;
        }
        return CompletionSlot::StringArg;
    }
    // Pragma name after `@`.
    if pragma_prefix_at_cursor(text, cursor_byte).is_some() {
        return CompletionSlot::PragmaName;
    }
    // Member access `recv.` / `recv->`.
    if separator_before_prefix(text, cursor_byte).is_some() {
        return CompletionSlot::Member;
    }
    // Static access `Recv::` (ident mode; string mode handled above).
    if static_receiver_at(text, cursor_byte).is_some() {
        return CompletionSlot::Static;
    }
    // Annotation interior we don't otherwise complete — don't leak scope.
    if ancestor_with_kind(node, "annotation").is_some() {
        return CompletionSlot::NoCompletion;
    }

    // ---- Pass 1 ----
    classify_structural(text, node, cursor_byte)
}

/// Walk up from `node`, classifying by the first recognized ancestor.
/// `from_field` tracks the field name of the child we ascended from so
/// `fn na|me` (the `name` field of `fn_decl`) suppresses while
/// `fn f(): T { na|me() }` (a statement in the body block) does not.
fn classify_structural<'t>(
    text: &str,
    node: tree_sitter::Node<'t>,
    cursor_byte: usize,
) -> CompletionSlot<'t> {
    let mut cur = node;
    let mut from_field: Option<&'static str> = None;
    loop {
        match cur.kind() {
            // Anywhere in a type expression → type-position completion.
            "type_ident" | "type_decorator" | "attr_type" => return CompletionSlot::TypeRef,
            // A field's `name` slot lists field names; its `value` slot
            // (or any non-name child) is an expression typed by the attr.
            "object_field" => {
                return match (from_field, cur.parent()) {
                    (Some("name"), Some(body))
                        if matches!(body.kind(), "object_fields" | "object_initializers") =>
                    {
                        CompletionSlot::ObjectFieldName { body }
                    }
                    _ => CompletionSlot::Expr {
                        value: Some(ValueSlot::FieldValue { object_field: cur }),
                    },
                };
            }
            "object_initializers" | "object_fields" => {
                return classify_object_body(text, cur, cursor_byte);
            }
            // Call arguments → expected type is the callee's params[index].
            "args" => {
                let value = cur
                    .parent()
                    .filter(|p| p.kind() == "call_expr")
                    .and_then(|call| call.child_by_field_name("fn"))
                    .map(|callee| ValueSlot::CallArg {
                        callee,
                        index: call_arg_index(cur, cursor_byte),
                    });
                return CompletionSlot::Expr { value };
            }
            // Binding initializer → expected type is the declared type.
            "initializer" => {
                let value = cur
                    .parent()
                    .filter(|p| matches!(p.kind(), "var_decl" | "modvar"))
                    .map(|binder| ValueSlot::Initializer { binder });
                return CompletionSlot::Expr { value };
            }
            // Declaration-name slots: suppress (the user is binding a new
            // identifier, not referencing one).
            "fn_decl" | "type_decl" | "enum_decl" | "modvar" | "var_decl"
                if from_field == Some("name") =>
            {
                return CompletionSlot::Declaration;
            }
            // Declaration containers: param lists, generic-param lists,
            // type / enum bodies, modifier runs — never expression slots.
            "fn_params" | "fn_param" | "type_params" | "type_param" | "type_body" | "enum_body"
            | "enum_field" | "modifiers" => return CompletionSlot::Declaration,
            // Statement / module position → expression completion. This
            // is the only path to full-scope completion.
            "block" | "source_file" | "module" => return CompletionSlot::Expr { value: None },
            _ => {}
        }
        let Some(parent) = cur.parent() else {
            return CompletionSlot::NoCompletion;
        };
        from_field = field_name_of(parent, cur);
        cur = parent;
    }
}

/// Decide name-vs-value inside an `object_initializers` / `object_fields`
/// body. After a `:` in the current segment the cursor is at a field
/// *value* (an expression); otherwise it's a *name* slot (the type-resolve
/// decision — named-attr type vs collection — is deferred to the emitter).
fn classify_object_body<'t>(
    text: &str,
    body: tree_sitter::Node<'t>,
    cursor_byte: usize,
) -> CompletionSlot<'t> {
    if object_cursor_after_colon(text, body, cursor_byte) {
        // Recovery-shape value slot (`Foo { a: | }` parsed as
        // `object_initializers` + ERROR colon) — no clean `object_field`
        // anchor here, so expression completion runs unranked.
        return CompletionSlot::Expr { value: None };
    }
    CompletionSlot::ObjectFieldName { body }
}

/// 0-based argument index of the cursor within an `args` node — the
/// count of top-level `,` separators before `cursor_byte`. (Commas
/// nested inside argument sub-expressions live in deeper nodes, so a
/// direct-child scan is correct.)
fn call_arg_index(args: tree_sitter::Node<'_>, cursor_byte: usize) -> usize {
    let mut walker = args.walk();
    args.children(&mut walker)
        .filter(|c| c.kind() == "," && c.start_byte() < cursor_byte)
        .count()
}

/// Resolve the expected type at a value slot from the analyzer's
/// resolved-`TypeId` maps (`object_field_uses` + `type_members`,
/// callee `expr_types` → `Lambda`, `def_types`). Returns `None`
/// whenever the lookup is uncertain — ranking is non-destructive, so a
/// missing expected type simply means "don't rank".
fn resolve_expected(value: &ValueSlot<'_>, uri: &Uri, project: &ProjectAnalysis) -> Option<TypeId> {
    let module = project.module(uri)?;
    match value {
        ValueSlot::FieldValue { object_field } => {
            // The field's `name` ident, once bound, points at its
            // declaring type + attr via `object_field_uses`; the resolved
            // attr type lives in that type's `type_members.attr_types`.
            let name_node = object_field.child_by_field_name("name")?;
            let name_range = name_node.byte_range();
            let (ident_idx, _) = module
                .hir
                .idents
                .iter()
                .find(|(_, i)| i.byte_range == name_range)?;
            let binding = module.analysis.object_field_uses.get(&ident_idx)?;
            let name_sym = module.hir.idents[ident_idx].symbol;
            project
                .index
                .type_members
                .get(&binding.declaring_type)?
                .attr_types
                .get(&name_sym)
                .copied()
        }
        ValueSlot::CallArg { callee, index } => {
            // Map the callee CST node to its HIR expr and read the
            // structural `Lambda` signature the analyzer minted for it.
            let range = callee.byte_range();
            let (expr_idx, _) = module
                .hir
                .exprs
                .iter()
                .filter(|(_, e)| e.byte_range() == range)
                .max_by_key(|(_, e)| e.byte_range().end - e.byte_range().start)?;
            let callee_ty = module.analysis.expr_types.get(&expr_idx).copied()?;
            match &project.arena().get(callee_ty).kind {
                TypeKind::Lambda { params, .. } => params.get(*index).copied(),
                _ => None,
            }
        }
        ValueSlot::Initializer { binder } => {
            // Declared (or inferred) type of the binding, keyed by its
            // `name` ident in `def_types`.
            let name_node = binder.child_by_field_name("name")?;
            let name_range = name_node.byte_range();
            let (ident_idx, _) = module
                .hir
                .idents
                .iter()
                .find(|(_, i)| i.byte_range == name_range)?;
            module.analysis.def_types.get(&ident_idx).copied()
        }
    }
}

/// Whether a scope candidate's type is assignable to the slot's expected
/// type. Variable-like candidates (locals / params / module vars) carry a
/// resolved `def_types` entry; fns / types / generics have no plain value
/// type, so they never match (rank-neutral). `arena` is the throwaway
/// clone the caller owns — `is_assignable_to_with_index` needs `&mut`.
fn type_matches_expected(
    project: &ProjectAnalysis,
    arena: &mut TypeArena,
    module: &ModuleAnalysis,
    source: &crate::ide::scope::NameSource,
    expected: TypeId,
) -> bool {
    use crate::ide::scope::NameSource;
    let cand = match source {
        NameSource::Local(idx) | NameSource::Param(idx) => {
            module.analysis.def_types.get(idx).copied()
        }
        NameSource::ModuleDecl(decl_id) => match &module.hir.decls[*decl_id] {
            Decl::Var(vd) => module.analysis.def_types.get(&vd.name).copied(),
            _ => None,
        },
        NameSource::Generic => None,
    };
    let Some(cand) = cand else {
        return false;
    };
    crate::project::is_assignable_to_with_index(
        &project.index,
        project.well_known(),
        project.decl_registry(),
        arena,
        cand,
        expected,
    )
}

/// `true` when the nearest non-word, non-whitespace char before the
/// cursor (scanning back within `body`) is `:` — i.e. the cursor sits in
/// a field *value*. Handles the recovery shape where an incomplete
/// `Foo { a: | }` parses as `object_initializers` + an `ERROR` colon.
fn object_cursor_after_colon(text: &str, body: tree_sitter::Node<'_>, cursor_byte: usize) -> bool {
    let bytes = text.as_bytes();
    let start = body.byte_range().start;
    let mut i = cursor_byte.min(bytes.len());
    while i > start {
        let b = bytes[i - 1];
        if b == b' '
            || b == b'\t'
            || b == b'\n'
            || b == b'\r'
            || b.is_ascii_alphanumeric()
            || b == b'_'
        {
            i -= 1;
        } else {
            break;
        }
    }
    i > start && bytes[i - 1] == b':'
}

// P15.4
/// `@include("<cursor>")` directory completion. Activated when
/// the cursor sits inside a `string` (or its `string_fragment` child)
/// whose enclosing `mod_pragma`'s annotation name is `include`. Walks
/// the project root directly (a one-level `read_dir`, no recursion)
/// and returns each subdirectory as a `CompletionItem`. Case-insensitive
/// prefix-matches the cursor's already-typed text.
fn include_dir_completion(
    text: &str,
    node: tree_sitter::Node<'_>,
    cursor_byte: usize,
    project_root: Option<&std::path::Path>,
) -> Option<Vec<CompletionItem>> {
    let project_root = project_root?;
    // Walk up to find the enclosing `string` node, then confirm the
    // chain is `string -> args -> annotation` with annotation name
    // `include`.
    let string_node = ancestor_with_kind(node, "string")?;
    let args_node = ancestor_with_kind(string_node, "args")?;
    let annotation_node = ancestor_with_kind(args_node, "annotation")?;
    let mut name_cursor = annotation_node.walk();
    let name_text = annotation_node
        .named_children(&mut name_cursor)
        .find(|c| c.kind() == "ident")
        .and_then(|c| text.get(c.byte_range()))?;
    if name_text != "include" {
        return None;
    }
    let mod_pragma = ancestor_with_kind(annotation_node, "mod_pragma")?;
    let _ = mod_pragma; // confirm we're inside a top-level pragma

    // Read what the user has typed so far (the prefix between `"` and
    // the cursor). The string node's text range is the whole `"..."`;
    // the inner `string_fragment` child holds the unescaped content.
    let typed = string_prefix_at_cursor(text, string_node, cursor_byte);
    // Split on `/`: everything up to the last `/` is the directory
    // path the user is drilling into; the part after is the prefix
    // for the next completion list. Examples:
    //   ""             -> base = project_root, prefix = ""
    //   "src"          -> base = project_root, prefix = "src"
    //   "src/"         -> base = project_root/src, prefix = ""
    //   "src/util"     -> base = project_root/src, prefix = "util"
    let (rel_dir, prefix) = match typed.rsplit_once('/') {
        Some((dir, name)) => (dir.to_string(), name.to_string()),
        None => (String::new(), typed.clone()),
    };
    let mut base = project_root.to_path_buf();
    if !rel_dir.is_empty() {
        for seg in rel_dir.split('/') {
            if seg.is_empty() || seg == "." {
                continue;
            }
            // Reject `..` to keep completion anchored under project_root.
            if seg == ".." {
                return Some(Vec::new());
            }
            base.push(seg);
        }
    }
    let entries = match std::fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return Some(Vec::new()),
    };
    let mut items = Vec::new();
    let prefix_lower = prefix.to_lowercase();
    // Conventional ignores apply at the project root only — a user
    // explicitly drilling into `lib/` or `target/` should still see
    // what's there.
    let at_root = rel_dir.is_empty();
    let skip: &[&str] = if at_root {
        &[
            "node_modules",
            "gcdata",
            ".git",
            "target",
            "lib",
            "bin",
            "files",
            "webroot",
        ]
    } else {
        &["node_modules", ".git"]
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if skip.contains(&name_str) || name_str.starts_with('.') {
            continue;
        }
        if !prefix_lower.is_empty() && !name_str.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        items.push(CompletionItem {
            label: name_str.to_string(),
            kind: Some(CompletionItemKind::Folder),
            detail: Some("@include directory".into()),
            insert_text: Some(name_str.to_string()),
            ..Default::default()
        });
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

fn ancestor_with_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cur = node;
    loop {
        if cur.kind() == kind {
            return Some(cur);
        }
        cur = cur.parent()?;
    }
}

/// Read the text inside a `string` node from its opening quote up to
/// `cursor_byte`. Used for prefix-matching against the typed text
/// before the cursor.
fn string_prefix_at_cursor(
    text: &str,
    string_node: tree_sitter::Node<'_>,
    cursor_byte: usize,
) -> String {
    let r = string_node.byte_range();
    let raw = text.get(r.clone()).unwrap_or("");
    // Strip the leading quote(s).
    let opener_len = if raw.starts_with('"') { 1 } else { 0 };
    let content_start = r.start + opener_len;
    if cursor_byte <= content_start {
        return String::new();
    }
    let upto = cursor_byte.min(r.end);
    text.get(content_start..upto).unwrap_or("").to_string()
}

// =============================================================================
// P15.3 — `@library("<name>", "<cursor>")` version completion
// =============================================================================

/// Discriminator stored under `data.type` to mark a completion list
/// as the lazy version-lookup placeholder. The LSP handler swaps the
/// list with concrete versions before returning to the editor; tests
/// can target the same shape via [`resolve_library_version_completion`].
const LIB_VERSION_PLACEHOLDER_KIND: &str = "greycat.lib.version";

/// Payload attached to the placeholder `CompletionItem.data`. Carries
/// everything the registry resolver needs without round-tripping back
/// to the document text.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct LibVersionPlaceholder {
    #[serde(rename = "type")]
    kind: String,
    /// First-arg lib name, e.g. `std` or `core`.
    lib: String,
    /// What the user has typed inside the version string up to the
    /// cursor — used for channel filtering (`-dev`, `-beta`) when the
    /// resolver emits real items.
    typed: String,
    /// Inner-content range of the version string (between the quotes).
    /// The concrete items use this as their `textEdit.range` so each
    /// version replaces exactly the user's partial input.
    range: Range,
}

/// Detect when the cursor sits inside the *version* slot of an
/// `@library("name", "<cursor>")` pragma and emit a single lazy
/// placeholder item. The LSP server intercepts the placeholder and
/// resolves it via [`resolve_library_version_completion`] using its
/// configured [`RegistryFetcher`]. Returns `None` when we're not in
/// the version slot so the parent dispatcher falls through.
///
/// Why a placeholder rather than fetching here: the analyzer-side
/// completion path is sync and I/O-free by design, so the registry
/// walk lives one layer up where a `RegistryFetcher` is in scope.
fn library_version_completion(
    text: &str,
    node: tree_sitter::Node<'_>,
    cursor_byte: usize,
    encoding: SourceEncoding,
) -> Option<Vec<CompletionItem>> {
    let string_node = ancestor_with_kind(node, "string")?;
    let args_node = ancestor_with_kind(string_node, "args")?;
    let annotation_node = ancestor_with_kind(args_node, "annotation")?;
    let mut name_cursor = annotation_node.walk();
    let name_text = annotation_node
        .named_children(&mut name_cursor)
        .find(|c| c.kind() == "ident")
        .and_then(|c| text.get(c.byte_range()))?;
    if name_text != "library" {
        return None;
    }
    // Confirm the cursor's string is the second named arg AND the
    // first arg is also a string literal (the lib name). Anything
    // else — a non-string first arg, the cursor on the first arg —
    // bails out so other dispatchers get a chance.
    let mut walk = args_node.walk();
    let args: Vec<tree_sitter::Node<'_>> = args_node
        .named_children(&mut walk)
        .filter(|c| c.kind() == "string")
        .collect();
    if args.len() < 2 || args[1].id() != string_node.id() {
        return None;
    }
    let lib_name = string_inner_text(text, args[0])?.to_string();

    // Inner-content range (between the quotes). Used as both the
    // resolver's `textEdit` target and as the channel-filter source
    // (the slice from content-start to cursor).
    let r = string_node.byte_range();
    let raw = text.get(r.clone())?;
    let open = if raw.starts_with('"') { 1 } else { 0 };
    let close = if raw.ends_with('"') && raw.len() > open {
        1
    } else {
        0
    };
    let inner_start = r.start + open;
    let inner_end = r.end.saturating_sub(close).max(inner_start);
    let typed = text
        .get(inner_start..cursor_byte.min(inner_end))
        .unwrap_or("")
        .to_string();
    let range = Range::from_byte_range(text, &(inner_start..inner_end), encoding);
    let placeholder = LibVersionPlaceholder {
        kind: LIB_VERSION_PLACEHOLDER_KIND.into(),
        lib: lib_name.clone(),
        typed,
        range,
    };
    let item = CompletionItem {
        label: format!("Fetching '{lib_name}' versions..."),
        kind: Some(CompletionItemKind::Module),
        data: Some(serde_json::to_value(&placeholder).ok()?),
        ..Default::default()
    };
    Some(vec![item])
}

/// Read the inner content of a `string` node (everything between the
/// quotes). Returns `None` for malformed strings.
fn string_inner_text<'a>(text: &'a str, string_node: tree_sitter::Node<'_>) -> Option<&'a str> {
    let r = string_node.byte_range();
    let raw = text.get(r.clone())?;
    let open = if raw.starts_with('"') { 1 } else { 0 };
    let close = if raw.ends_with('"') && raw.len() > open {
        1
    } else {
        0
    };
    text.get(r.start + open..r.end.saturating_sub(close))
}

/// Pull the placeholder payload out of a [`CompletionList`] when it
/// looks like the lazy `@library` version-completion shape (single
/// item with `data.type == "greycat.lib.version"`). Returns `None`
/// for any other completion shape so the LSP handler returns the
/// list verbatim.
pub fn extract_lib_version_placeholder(list: &CompletionList) -> Option<LibVersionPayload> {
    if list.items.len() != 1 {
        return None;
    }
    let data = list.items[0].data.as_ref()?;
    let p: LibVersionPlaceholder = serde_json::from_value(data.clone()).ok()?;
    if p.kind != LIB_VERSION_PLACEHOLDER_KIND {
        return None;
    }
    Some(LibVersionPayload {
        lib: p.lib,
        typed: p.typed,
        range: p.range,
    })
}

/// Public-facing decoded placeholder. Server-side glue uses this to
/// invoke its [`RegistryFetcher`] and build the concrete item list.
#[derive(Debug, Clone)]
pub struct LibVersionPayload {
    pub lib: String,
    pub typed: String,
    pub range: Range,
}

/// Replace the lazy placeholder with concrete version items. Driven
/// by the LSP server once it sees [`extract_lib_version_placeholder`]
/// match.
///
/// Design difference from the TS reference: the TS impl *hard-filters*
/// to the user's typed channel (`-dev`/`-stable`/…), which makes the
/// channel feel like a constraint and forces backspacing to pivot
/// between channels. We treat channel as a *preference* instead
/// every version surfaces in every list, but matching-channel entries
/// rank first via `sortText` and the editor's own fuzzy match against
/// the full label decides what's visible. Result: the user can pivot
/// between `-dev` and `-stable` without re-fetching, and a blank
/// string still shows the full set.
///
/// Channel info also lands in `labelDetails.detail` (e.g. `[stable]`)
/// so the popup is readable at a glance without parsing the version
/// suffix; `last_modification` keeps the `description` slot.
pub fn resolve_library_version_completion(
    payload: &LibVersionPayload,
    fetcher: &dyn greycat_analyzer_core::registry::RegistryFetcher,
) -> CompletionList {
    let versions = greycat_analyzer_core::registry::get_lib_versions(&payload.lib, fetcher);
    let preferred = greycat_analyzer_core::registry::prerelease_tag(&payload.typed);
    let items: Vec<CompletionItem> = versions
        .into_iter()
        .enumerate()
        .map(|(i, v)| {
            let channel = version_channel(&v.text);
            // Two-tier sort key: `0_…` for matching-channel hits (or
            // every entry when no preference is expressed), `1_…`
            // otherwise. Within each tier, registry order — already
            // semver-descending — is preserved by the index suffix.
            let tier = match preferred {
                Some(tag) => {
                    if channel.map(|c| c.contains(tag)).unwrap_or(false) {
                        0
                    } else {
                        1
                    }
                }
                None => 0,
            };
            let detail = channel.map(|c| format!("[{c}]"));
            CompletionItem {
                label: v.text.clone(),
                kind: Some(CompletionItemKind::Constant),
                label_details: Some(CompletionItemLabelDetails {
                    detail,
                    description: Some(v.last_modification.clone()),
                }),
                sort_text: Some(format!("{tier}_{i:05}")),
                text_edit: Some(TextEdit {
                    range: payload.range,
                    new_text: v.text,
                }),
                ..Default::default()
            }
        })
        .collect();
    CompletionList {
        is_incomplete: false,
        items,
    }
}

/// Extract the prerelease channel from a version string
/// (`7.8.166-stable` → `Some("stable")`). Empty / non-prerelease
/// versions return `None` so the popup shows the bare version with
/// no `[…]` suffix.
fn version_channel(version: &str) -> Option<&str> {
    match version.split_once('-') {
        Some((_, pre)) if !pre.is_empty() => Some(pre),
        _ => None,
    }
}

// =============================================================================
// P15.2.1 — pragma completion after `@`
// =============================================================================

/// Emit pragma completion items when the cursor sits right after a `@`
/// or partway through an annotation name (`@li|brary`). Mirrors the TS
/// reference's `PRAGMA_COMPLETION_ITEMS` set
/// (`packages/lang/src/project/analysis_result.ts:2537`). Returns `None`
/// when the cursor isn't in an annotation-start position so the parent
/// dispatcher can fall through to other completion shapes.
fn pragma_completion(text: &str, cursor_byte: usize) -> Option<Vec<CompletionItem>> {
    let typed = pragma_prefix_at_cursor(text, cursor_byte)?;
    let prefix_lower = typed.to_lowercase();
    let mut items = pragma_items()
        .into_iter()
        .filter(|item| {
            // Strip the leading `@` from the label before prefix-matching.
            let name = item.label.trim_start_matches('@');
            prefix_lower.is_empty() || name.to_lowercase().starts_with(&prefix_lower)
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

/// Walk back from `cursor_byte` over `[A-Za-z0-9_]*` and check that the
/// preceding byte is `@`. Returns the typed prefix between `@` and the
/// cursor (empty string when the user just hit `@`). `None` when there's
/// no `@` or the run isn't word-shaped.
fn pragma_prefix_at_cursor(text: &str, cursor_byte: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let cap = cursor_byte.min(bytes.len());
    let mut i = cap;
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i -= 1;
        } else {
            break;
        }
    }
    if i == 0 || bytes[i - 1] != b'@' {
        return None;
    }
    Some(text.get(i..cap).unwrap_or("").to_string())
}

/// The pragma list. Snippet bodies match the TS reference shape so
/// editors that honor `InsertTextFormat::Snippet` get tabstop-driven
/// completions for the parametric forms (`@library`, `@include`,
/// `@role`, `@permission`). Bare forms (`@expose`, `@volatile`) skip
/// the snippet format flag.
fn pragma_items() -> Vec<CompletionItem> {
    vec![
        CompletionItem {
            label: "@library".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("library(\"$1\", \"$2\");$0".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some("Adds a library to the project".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "@include".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("include(\"$1\");$0".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some("Adds a source directory to the project".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "@role".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("role(\"$1\", \"$2\");$0".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some("Defines a role for the project".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "@permission".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("permission(\"$1\")$0".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some(
                "Defines a permission for the project, or give a permission to a function".into(),
            ),
            ..Default::default()
        },
        CompletionItem {
            label: "@expose".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("expose".into()),
            detail: Some("Registers the function as an http endpoint".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "@volatile".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("volatile".into()),
            detail: Some(
                "Volatile types cannot be stored in graph and have loose upgrade rules".into(),
            ),
            ..Default::default()
        },
        // Fmt-specific pragmas
        CompletionItem {
            label: "@fmt_line_width".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("fmt_line_width($1);$0".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some("Maximum line width before a `Group` breaks. Default: `120`".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "@fmt_indent".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("fmt_indent($1);$0".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some("Spaces per indent step. Default: `4`".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "@fmt_eol_last".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("fmt_eol_last($1);$0".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some("Append a trailing newline at end of file. Default: `false`".into()),
            ..Default::default()
        },
        // Lint-specific pragmas
        CompletionItem {
            label: "@lint_off".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("lint_off(\"$1\");$0".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some(
                "Silence specific lint rule(s) globally. Variadic string arguments.".into(),
            ),
            ..Default::default()
        },
        CompletionItem {
            label: "@lint_on".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("lint_on(\"$1\");$0".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some(
                "Enable advisory lint rule(s) that ship off by default. Variadic string arguments."
                    .into(),
            ),
            ..Default::default()
        },
    ]
}

// =============================================================================
// P23.5 — directive completion for `// gcl-…` line comments
// =============================================================================

/// Emit completion items when the cursor sits inside a `line_comment`
/// whose text starts with `// gcl` (allowing leading whitespace). Two
/// shapes:
///
/// - **Directive name** — when the cursor sits on the directive name
///   itself (`// gcl-` / `// gcl-lint-o<cursor>`), emit one item per
///   directive form (`gcl-lint-off`, `gcl-lint-next-off`, …) with
///   snippet bodies that include placeholder rule lists for the lint
///   forms.
/// - **Rule name** — when the cursor sits in a `// gcl-lint-off-* `
///   directive's rule list, emit one item per registered
///   [`crate::lint::LINT_RULES`] entry.
///
/// Returns `None` when the cursor isn't inside a `// gcl…` comment so
/// the parent dispatcher can fall through to the other completion shapes.
fn directive_completion(text: &str, cursor_byte: usize) -> Option<Vec<CompletionItem>> {
    let line = current_line_slice(text, cursor_byte);
    let line_start = current_line_start(text, cursor_byte);
    let in_line_byte = cursor_byte - line_start;
    let trimmed = line.trim_start();
    let leading_ws = line.len() - trimmed.len();
    if !trimmed.starts_with("//") {
        return None;
    }
    // Position relative to the comment payload (after `//` plus any
    // following whitespace).
    let after_slashes_offset = leading_ws + 2;
    if in_line_byte < after_slashes_offset {
        return None;
    }
    let payload = &line[after_slashes_offset..];
    let payload_offset_in_line = after_slashes_offset;
    let cursor_in_payload = in_line_byte - payload_offset_in_line;
    let payload_trimmed = payload.trim_start();
    let payload_leading = payload.len() - payload_trimmed.len();
    if cursor_in_payload < payload_leading {
        return None;
    }

    // Decide: are we still typing the directive name, or are we past
    // the first whitespace and typing rule names?
    let after_payload_ws = cursor_in_payload - payload_leading;
    let first_ws = payload_trimmed
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i);

    let in_name_slot = match first_ws {
        None => true,
        Some(idx) => after_payload_ws <= idx,
    };
    if in_name_slot {
        let typed = &payload_trimmed[..after_payload_ws.min(payload_trimmed.len())];
        if !"gcl".starts_with(typed) && !typed.starts_with("gcl") {
            return None;
        }
        let mut items: Vec<CompletionItem> = directive_items()
            .into_iter()
            .filter(|item| typed.is_empty() || item.label.starts_with(typed))
            .collect();
        if items.is_empty() {
            return None;
        }
        items.sort_by(|a, b| a.label.cmp(&b.label));
        return Some(items);
    }

    // Cursor is in the rule-list slot (after the first whitespace).
    // Only fire for `gcl-lint-off`, `gcl-lint-next-off`,
    // `gcl-lint-file-off`, `gcl-lint-on` — the four forms that accept
    // a rule list.
    let idx = first_ws?;
    let directive_name = &payload_trimmed[..idx];
    if !matches!(
        directive_name,
        "gcl-lint-off" | "gcl-lint-on" | "gcl-lint-next-off" | "gcl-lint-file-off"
    ) {
        return None;
    }
    let rule_typed = current_word_around(payload_trimmed, after_payload_ws);
    let mut items: Vec<CompletionItem> = crate::lint::LINT_RULES
        .iter()
        .filter(|r| rule_typed.is_empty() || r.name.starts_with(rule_typed))
        .map(|r| CompletionItem {
            label: r.name.into(),
            kind: Some(CompletionItemKind::EnumMember),
            insert_text: Some(r.name.into()),
            detail: Some(r.summary.into()),
            ..Default::default()
        })
        .collect();
    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

/// Snippet items for every `gcl-…` directive form. Snippet bodies for
/// the lint forms include a `${1:rule}` placeholder so editors that
/// honor `InsertTextFormat::Snippet` get an immediate tabstop.
fn directive_items() -> Vec<CompletionItem> {
    vec![
        CompletionItem {
            label: "gcl-lint-off".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("gcl-lint-off ${1:rule}".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some("silence the named rule(s) until matching `gcl-lint-on` (or EOF)".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "gcl-lint-on".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("gcl-lint-on ${1:rule}".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some("close a prior `gcl-lint-off` for the named rule(s)".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "gcl-lint-next-off".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("gcl-lint-next-off ${1:rule}".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some("silence the named rule(s) for the next AST item only".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "gcl-lint-file-off".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("gcl-lint-file-off ${1:rule}".into()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            detail: Some(
                "silence the named rule(s) for the whole file (must appear at module head)".into(),
            ),
            ..Default::default()
        },
        CompletionItem {
            label: "gcl-fmt-off".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("gcl-fmt-off".into()),
            detail: Some("preserve source verbatim until matching `gcl-fmt-on` (or EOF)".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "gcl-fmt-on".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("gcl-fmt-on".into()),
            detail: Some("close a prior `gcl-fmt-off`".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "gcl-fmt-skip".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("gcl-fmt-skip".into()),
            detail: Some("preserve the next AST node verbatim".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "gcl-fmt-file-off".into(),
            kind: Some(CompletionItemKind::Keyword),
            insert_text: Some("gcl-fmt-file-off".into()),
            detail: Some("preserve the whole file verbatim (must appear at module head)".into()),
            ..Default::default()
        },
    ]
}

fn current_line_slice(text: &str, byte: usize) -> &str {
    let start = current_line_start(text, byte);
    let end = text[byte..]
        .find('\n')
        .map(|i| byte + i)
        .unwrap_or(text.len());
    &text[start..end]
}

fn current_line_start(text: &str, byte: usize) -> usize {
    text[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Walk back from `cursor` over `[A-Za-z0-9_-]*` to find the word the
/// user is currently typing. Returns the slice between the word's start
/// and the cursor.
fn current_word_around(s: &str, cursor: usize) -> &str {
    let bytes = s.as_bytes();
    let cap = cursor.min(bytes.len());
    let mut start = cap;
    while start > 0 {
        let b = bytes[start - 1];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            start -= 1;
        } else {
            break;
        }
    }
    &s[start..cap]
}

// =============================================================================
// P15.2.2 — keyword completion at statement / expression positions
// =============================================================================

/// Walk back from `cursor_byte` over `[A-Za-z0-9_]*` and return the
/// typed run as an owned string. Used to prefix-filter keyword and
/// (later) ident completion.
fn ident_prefix_at_cursor(text: &str, cursor_byte: usize) -> String {
    let bytes = text.as_bytes();
    let cap = cursor_byte.min(bytes.len());
    let mut i = cap;
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i -= 1;
        } else {
            break;
        }
    }
    text.get(i..cap).unwrap_or("").to_string()
}

/// `true` iff `s` matches the grammar's `ident` shape
/// (`[A-Za-z_][A-Za-z0-9_]*`). Names that fail this — e.g. enum
/// variants declared as `"Africa/Abidjan"` — must be re-quoted
/// when the completion machinery surfaces them at a `Type::|`
/// insertion site.
fn is_ident_like(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Build a [`CompletionItem`] for an enum variant. `name` is the
/// variant's HIR-stored spelling (always unquoted, even for
/// string-named variants — quotes are stripped at lowering time so
/// `member_uses` matches against the chain's property text).
///
/// Spelling rules at the insertion site:
///   - If the cursor is inside a quoted property (`Foo::"Tim|"`),
///     the opening `"` is already in the buffer; emit the bare
///     variant text.
///   - Otherwise, emit ident-shaped names bare (`Foo::alpha`) and
///     escape + wrap non-ident names so the result is valid syntax
///     (`Foo::"Africa/Abidjan"`).
///
/// `replace_range` is the LSP range covering the surrounding word
/// at the cursor. Threading it through `text_edit` is what makes
/// "ask for completion mid-word" honest — the accepted text
/// replaces the existing word instead of doubling it via a naive
/// `insert_text` insertion at the cursor.
fn enum_variant_completion_item(
    name: &str,
    in_string: bool,
    replace_range: Range,
) -> CompletionItem {
    let display = if in_string || is_ident_like(name) {
        name.to_string()
    } else {
        let mut s = String::with_capacity(name.len() + 2);
        s.push('"');
        for c in name.chars() {
            match c {
                '\\' => s.push_str("\\\\"),
                '"' => s.push_str("\\\""),
                _ => s.push(c),
            }
        }
        s.push('"');
        s
    };
    CompletionItem {
        label: display.clone(),
        kind: Some(CompletionItemKind::EnumMember),
        text_edit: Some(TextEdit {
            range: replace_range,
            new_text: display,
        }),
        ..Default::default()
    }
}

/// Build a [`CompletionItem`] for a static method or module-level
/// decl reached through `Recv::|`. Same `text_edit`-based
/// replace-range plumbing as [`enum_variant_completion_item`] so
/// mid-ident invocation doesn't duplicate the typed prefix.
///
/// `detail` and `documentation` carry the signature + doc-comment of
/// the resolved decl so the popup's right-rail tooltip lights up the
/// same way it does for instance access ( / member completion).
fn static_completion_item(
    name: String,
    kind: CompletionItemKind,
    replace_range: Range,
    label_details: Option<CompletionItemLabelDetails>,
    detail: Option<String>,
    documentation: Option<String>,
) -> CompletionItem {
    CompletionItem {
        label: name.clone(),
        label_details,
        kind: Some(kind),
        text_edit: Some(TextEdit {
            range: replace_range,
            new_text: name,
        }),
        detail,
        documentation,
        ..Default::default()
    }
}

/// Every reserved word the user can type at a statement / expression
/// position. Mirrors the keywords baked into the tree-sitter grammar
/// (`grammar.js`): the modifiers (`private`, `static`, `abstract`,
/// `native`), decl-level (`fn`, `type`, `enum`, `var`), control-flow
/// (`if`, `else`, `for`, `while`, `do`, `return`, `throw`, `try`,
/// `catch`, `at`, `in`, `break`, `continue`, `breakpoint`), and
/// expression-level (`is`, `as`, `null`, `true`, `false`, `this`).
/// Context-only keywords (`extends`, `typeof`) are not listed — they
/// only parse in a single fixed slot (after a type-decl name / on a
/// fn-param type) and are completed by the contextual handlers, not
/// the stmt/expr fallback.
const ALL_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "at",
    "break",
    "breakpoint",
    "catch",
    "continue",
    "do",
    "else",
    "enum",
    "false",
    "fn",
    "for",
    "if",
    "in",
    "is",
    "native",
    "null",
    "private",
    "return",
    "static",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "var",
    "while",
];

// =============================================================================
// P15.2.3 — scope-aware ident completion
// =============================================================================

/// Emit a unified list of keywords + scope-visible names + project-wide
/// surface at an ident position. Mirrors the TS reference's
/// `Environment::suggest` (`packages/lang/src/analysis/environment.ts`)
/// — the per-suggestion `kind` is derived from each name's
/// `Definition` shape.
fn expr_completion(
    text: &str,
    cursor_byte: usize,
    uri: &Uri,
    project: &ProjectAnalysis,
    expected: Option<TypeId>,
) -> Option<Vec<CompletionItem>> {
    let typed = ident_prefix_at_cursor(text, cursor_byte);
    let prefix_lower = typed.to_lowercase();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut items: Vec<CompletionItem> = Vec::new();

    // Keywords first, alphabetic-sorted under `sort_text` so they land
    // toward the bottom of the suggestion popup (idents typically win).
    for kw in ALL_KEYWORDS {
        if !prefix_lower.is_empty() && !kw.starts_with(&prefix_lower) {
            continue;
        }
        if seen.insert((*kw).into()) {
            items.push(CompletionItem {
                label: (*kw).into(),
                kind: Some(CompletionItemKind::Keyword),
                insert_text: Some((*kw).into()),
                sort_text: Some(format!("z_{kw}")),
                ..Default::default()
            });
        }
    }

    // Scope-visible names — this module's HIR walked top-to-cursor.
    if let Some(module) = project.module(uri) {
        // Type-aware ranking: a candidate variable whose type is
        // assignable to the slot's expected type sorts above everything
        // (tier `0_`). Non-destructive — non-matching candidates keep
        // their normal tier, so nothing is hidden. Uses a throwaway
        // arena clone (the shared arena stays immutable on this read
        // path); only allocated when an expected type is present.
        let mut rank_arena = expected.map(|_| project.arena().clone());
        let names = scope_names_at(&module.hir, project.symbols(), cursor_byte);
        for (name, kind, sort_pri, source) in names {
            if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            let (label_details, detail, documentation) = scope_name_meta(
                module,
                project.arena(),
                project.decl_registry(),
                project.symbols(),
                &source,
                uri,
            );
            let mut sort_text = sort_pri.to_string();
            if let (Some(exp), Some(arena)) = (expected, rank_arena.as_mut())
                && type_matches_expected(project, arena, module, &source, exp)
            {
                sort_text = format!("0_{name}");
            }
            items.push(CompletionItem {
                label: name.clone(),
                label_details,
                kind: Some(kind),
                insert_text: Some(name),
                sort_text: Some(sort_text),
                detail,
                documentation,
                ..Default::default()
            });
        }
    }

    // Project surface — every cross-module top-level decl + primitives
    // + runtime types + native fn signatures. `module(uri)` guarded
    // to avoid double-emitting in-module decls.
    let in_module: FxHashSet<String> = project
        .module(uri)
        .map(|m| {
            m.hir
                .module
                .as_ref()
                .map(|md| {
                    md.decls
                        .iter()
                        .filter_map(|d| m.hir.decls[*d].name())
                        .map(|n| project.symbols()[m.hir.idents[n].symbol].to_string())
                        .collect()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();

    for (name_sym, locs) in &project.index.decl_locations {
        let name = project.index.symbols.resolve(name_sym);
        if in_module.contains(name) {
            continue;
        }
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        if !seen.insert(name.to_string()) {
            continue;
        }
        let kind = decl_locs_kind(project, locs);
        let (label_details, detail, documentation) = foreign_decl_completion_meta(project, locs);
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(kind),
            insert_text: Some(name.to_string()),
            sort_text: Some(format!("y_{name}")),
            detail,
            documentation,
            label_details,
            ..Default::default()
        });
    }
    for name_sym in project.index.values.iter() {
        let name = project.index.symbols.resolve(name_sym);
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        if !seen.insert(name.to_string()) {
            continue;
        }
        // `values` lumps non-native fns, top-level vars, and runtime
        // value-position globals (`NaN`, `Infinity`) together. Emit
        // FUNCTION only for actual fn names — otherwise the call-paren
        // post-pass appends `($0)` and turns `NaN` into `NaN()`.
        let kind = if project.index.non_native_fn_names.contains(name_sym) {
            CompletionItemKind::Function
        } else if project.index.runtime_globals.contains_key(name_sym) {
            CompletionItemKind::Constant
        } else {
            CompletionItemKind::Variable
        };
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(kind),
            insert_text: Some(name.to_string()),
            sort_text: Some(format!("y_{name}")),
            ..Default::default()
        });
    }
    for name_sym in project.index.module_names.keys() {
        let name = project.index.symbols.resolve(name_sym);
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        if !seen.insert(name.to_string()) {
            continue;
        }
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::Module),
            insert_text: Some(name.to_string()),
            sort_text: Some(format!("x_{name}")),
            ..Default::default()
        });
    }

    if items.is_empty() {
        return None;
    }
    Some(items)
}

/// Post-process completion items so the LSP edit honors what the user
/// already typed:
///
/// 1. **Replace-range** — when the cursor sits mid-identifier
///    (`x.cha|rs()`), convert each item's `insert_text` into an
///    explicit `TextEdit` that spans the whole word
///    (`[ident_start..ident_end]`). Without this, editors that follow
///    the LSP literally insert at the cursor and leave the suffix
///    behind (`x.endsWith()chars()`). When there's no identifier under
///    the cursor (`x.|`), we leave `insert_text` alone — editors apply
///    their own prefix-deletion heuristic and the existing shape is
///    correct.
///
/// 2. **Call-parens** — for FUNCTION / METHOD items whose `(...)` isn't
///    already present immediately after the identifier, append `($0)`
///    and switch to `InsertTextFormat::Snippet` so the cursor lands
///    between the parens. The "parens already there" check probes the
///    byte right after `ident_end`, *not* the cursor — so on
///    `x.|chars()` (cursor before `chars`, parens after `chars`) the
///    snippet is suppressed because the user already opened the call.
///
/// Skips items already carrying a `SNIPPET` body (e.g. pragma
/// templates like `@library("$1", "$2")`) for the call-paren rewrite,
/// and skips items already carrying their own `text_edit` for the
/// replace-range conversion.
fn apply_call_paren_snippet(
    items: &mut [CompletionItem],
    text: &str,
    cursor_byte: usize,
    encoding: SourceEncoding,
) {
    let prefix_len = ident_prefix_at_cursor(text, cursor_byte).len();
    let suffix_len = ident_suffix_at_cursor(text, cursor_byte).len();
    let ident_start = cursor_byte.saturating_sub(prefix_len);
    let ident_end = cursor_byte + suffix_len;
    let parens_already_there = next_non_ws_is_open_paren(text.as_bytes(), ident_end);
    let replace_range =
        (suffix_len > 0).then(|| Range::from_byte_range(text, &(ident_start..ident_end), encoding));

    for item in items.iter_mut() {
        // 1) Append `($0)` to FUNCTION / METHOD items unless the
        //    surrounding source already opens the call. When the item
        //    already carries an explicit `text_edit` (e.g. enum-variant
        //    or static-completion shapes that bake in a replace-range),
        //    we mutate `text_edit.new_text` so the editor honors it;
        //    otherwise we mutate `insert_text`.
        if !parens_already_there
            && matches!(
                item.kind,
                Some(CompletionItemKind::Function) | Some(CompletionItemKind::Method)
            )
            && !matches!(item.insert_text_format, Some(InsertTextFormat::Snippet))
        {
            if let Some(te) = item.text_edit.as_mut() {
                te.new_text = format!("{}($0)", te.new_text);
            } else {
                let base = item
                    .insert_text
                    .clone()
                    .unwrap_or_else(|| item.label.clone());
                item.insert_text = Some(format!("{base}($0)"));
            }
            item.insert_text_format = Some(InsertTextFormat::Snippet);
        }

        // 2) When the cursor is mid-identifier, lift `insert_text` into
        //    a `TextEdit` covering the full word so the editor replaces
        //    `chars` (rather than inserting between `.` and `chars`).
        //    `text_edit` already set by an upstream emitter wins.
        if let Some(range) = replace_range
            && item.text_edit.is_none()
        {
            let new_text = item
                .insert_text
                .clone()
                .unwrap_or_else(|| item.label.clone());
            item.text_edit = Some(TextEdit { range, new_text });
        }
    }
}

/// Word characters appearing immediately after `cursor_byte`. Mirrors
/// [`ident_prefix_at_cursor`]'s walk but goes forward instead of
/// backward.
fn ident_suffix_at_cursor(text: &str, cursor_byte: usize) -> &str {
    let bytes = text.as_bytes();
    let mut end = cursor_byte;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    &text[cursor_byte..end]
}

fn next_non_ws_is_open_paren(bytes: &[u8], cursor_byte: usize) -> bool {
    let mut i = cursor_byte;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    i < bytes.len() && bytes[i] == b'('
}

/// Render completion-popup metadata for a scope-visible name:
/// `(label_details, detail, documentation)`. `label_details.detail`
/// renders inline next to the label in the popup row (rust-analyzer
/// style — `(args): Ret` for fns); `detail` is the larger side-panel
/// signature; `documentation` is the doc-comment. Locals / params
/// surface their inferred type as `detail` only.
fn scope_name_meta(
    module: &ModuleAnalysis,
    arena: &TypeArena,
    decl_registry: &crate::well_known::DeclRegistry,
    symbols: &SymbolTable,
    source: &crate::ide::scope::NameSource,
    uri: &Uri,
) -> (
    Option<CompletionItemLabelDetails>,
    Option<String>,
    Option<String>,
) {
    use crate::ide::scope::NameSource;
    match source {
        NameSource::ModuleDecl(decl_id) => {
            let decl = &module.hir.decls[*decl_id];
            // For fns the compact `(args): Ret` form goes into BOTH
            // `label_details.detail` (VSCode renders it inline next
            // to label) AND `detail` (Zed shows `detail` as the
            // popup-row suffix, ignoring `label_details.detail`).
            // Hover provides the full source-form signature, so the
            // duplication isn't a regression.
            //
            // For types / enums / vars `render_decl_signature` would
            // just repeat the label (`type Reader` next to label
            // `Reader`), wasting popup-row width. Surface the home
            // module's stem instead so the row distinguishes
            // same-named decls from different modules.
            let (label_details, detail) = match decl {
                Decl::Fn(fnd) => {
                    let compact = render_fn_signature_compact(&module.hir, symbols, fnd, None);
                    (
                        Some(CompletionItemLabelDetails {
                            detail: Some(compact.clone()),
                            description: None,
                        }),
                        Some(compact),
                    )
                }
                Decl::Type(_) | Decl::Enum(_) | Decl::Var(_) => {
                    let module_label = module_label_for_uri(uri);
                    (
                        Some(CompletionItemLabelDetails {
                            detail: None,
                            description: Some(module_label.clone()),
                        }),
                        Some(module_label),
                    )
                }
                _ => (
                    None,
                    Some(render_decl_signature(&module.hir, symbols, decl, None)),
                ),
            };
            (label_details, detail, doc_to_markup(decl_doc(decl)))
        }
        NameSource::Local(name_idx) | NameSource::Param(name_idx) => {
            let detail = module.analysis.def_types.get(name_idx).map(|ty| {
                crate::project::display_type(arena, decl_registry, symbols, *ty).to_string()
            });
            (None, detail, None)
        }
        NameSource::Generic => (None, None, None),
    }
}

/// Render completion-popup metadata for a cross-module decl
/// surfaced via [`ProjectIndex::decl_locations`]:
/// `(label_details, detail, documentation)`. `label_details.detail`
/// renders the compact fn signature (rust-analyzer style) when the
/// foreign decl is a `Decl::Fn`; `label_details.description` always
/// carries the home module's stem (`model` for
/// `file:///proj/src/model.gcl`); `detail` is the full signature;
/// `documentation` is the doc-comment. All three fall through to
/// `None` when the decl's home module isn't cached.
fn foreign_decl_completion_meta(
    project: &ProjectAnalysis,
    locs: &[(
        Uri,
        greycat_analyzer_hir::arena::Idx<Decl>,
        crate::stdlib::Namespace,
    )],
) -> (
    Option<CompletionItemLabelDetails>,
    Option<String>,
    Option<String>,
) {
    let Some((uri, decl_id, _)) = locs.first() else {
        return (None, None, None);
    };
    let Some(m) = project.module(uri) else {
        return (None, None, None);
    };
    let decl = &m.hir.decls[*decl_id];
    let documentation = doc_to_markup(decl_doc(decl));
    let description = module_label_for_uri(uri);
    // Fns: mirror the compact `(args): Ret` form into both
    //   `label_details.detail` (VSCode reads this) and `detail`
    //   (Zed reads this). Hover keeps the full source-form signature.
    // Types / enums / vars: `render_decl_signature` would just
    //   repeat the label (`type Reader` next to label `Reader`).
    //   Surface the home module's stem in `detail` instead — that's
    //   the useful signal for disambiguating same-named decls.
    let (compact_inline, detail) = match decl {
        Decl::Fn(fnd) => {
            let c = render_fn_signature_compact(&m.hir, project.symbols(), fnd, None);
            (Some(c.clone()), Some(c))
        }
        Decl::Type(_) | Decl::Enum(_) | Decl::Var(_) => (None, Some(description.clone())),
        _ => (
            None,
            Some(render_decl_signature(&m.hir, project.symbols(), decl, None)),
        ),
    };
    let label_details = Some(CompletionItemLabelDetails {
        detail: compact_inline,
        description: Some(description),
    });
    (label_details, detail, documentation)
}

/// Pick the `CompletionItemKind` for a name resolving through the
/// project index's decl table. When the name has multiple home
/// locations we pick the first; that's the same disambiguation policy
/// the resolver uses.
fn decl_locs_kind(
    project: &ProjectAnalysis,
    locs: &[(
        Uri,
        greycat_analyzer_hir::arena::Idx<Decl>,
        crate::stdlib::Namespace,
    )],
) -> CompletionItemKind {
    if let Some((uri, decl_id, _)) = locs.first()
        && let Some(m) = project.module(uri)
    {
        match &m.hir.decls[*decl_id] {
            Decl::Fn(_) => CompletionItemKind::Function,
            Decl::Type(_) => CompletionItemKind::Class,
            Decl::Enum(_) => CompletionItemKind::Enum,
            Decl::Var(_) => CompletionItemKind::Variable,
            Decl::Pragma(_) => CompletionItemKind::Constant,
        }
    } else {
        CompletionItemKind::Text
    }
}

/// Bridge from the shared scope walker's [`ScopeNameKind`] to the
/// LSP-shaped `(CompletionItemKind, sort_priority)` tuple completion
/// renders into popup items.
///
/// `sort_priority` is a static prefix string — lower strings sort
/// earlier in the popup, so locals/params (`a_` / `b_`) win over
/// module decls (`n_`) at the same name.
fn scope_kind_to_completion(
    kind: crate::ide::scope::ScopeNameKind,
) -> (CompletionItemKind, &'static str) {
    use crate::ide::scope::ScopeNameKind as SK;
    match kind {
        SK::Fn => (CompletionItemKind::Function, "n_"),
        SK::Type => (CompletionItemKind::Class, "n_"),
        SK::Enum => (CompletionItemKind::Enum, "n_"),
        SK::Var => (CompletionItemKind::Variable, "n_"),
        SK::Param => (CompletionItemKind::Variable, "a_"),
        SK::Local => (CompletionItemKind::Variable, "b_"),
        SK::Generic => (CompletionItemKind::TypeParameter, "g_"),
    }
}

/// Walk the HIR to collect every name visible at `cursor_byte`, with
/// the completion-popup metadata layered on. Thin wrapper around the
/// shared [`crate::ide::scope::scope_names_at`].
fn scope_names_at(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    cursor_byte: usize,
) -> Vec<(
    String,
    CompletionItemKind,
    &'static str,
    crate::ide::scope::NameSource,
)> {
    crate::ide::scope::scope_names_at(hir, symbols, cursor_byte)
        .into_iter()
        .map(|entry| {
            let (kind, sort) = scope_kind_to_completion(entry.kind);
            (symbols[entry.symbol].to_string(), kind, sort, entry.source)
        })
        .collect()
}

// =============================================================================
// P15.2.4 — member completion after `.` / `->`
// =============================================================================

/// Member-access completion: when the cursor sits in `recv.|prop` /
/// `recv->|prop`, list the receiver type's attrs + methods. Cross-
/// module receivers consult `ProjectIndex::decl_locations` to navigate
/// to the foreign type's HIR.
///
/// Tolerant of error-recovery: when the user has typed `p.` (a
/// half-formed member access whose receiver lives inside an `ERROR`
/// node), the HIR doesn't carry an `Expr::Ident` for the receiver, so
/// we fall back to a CST-based ident lookup that consults the
/// resolver and `def_types` for the receiver's type.
fn member_completion(
    text: &str,
    root: tree_sitter::Node<'_>,
    cursor_byte: usize,
    uri: &Uri,
    project: &ProjectAnalysis,
    encoding: SourceEncoding,
) -> Option<Vec<CompletionItem>> {
    let typed = ident_prefix_at_cursor(text, cursor_byte);
    let prefix_lower = typed.to_lowercase();
    let prefix_start = cursor_byte.saturating_sub(typed.len());
    let bytes = text.as_bytes();
    if prefix_start > bytes.len() {
        return None;
    }
    // Determine separator: `.` (member) or `->` (arrow). `sep_start`
    // is the byte offset of the first separator char so we can build
    // a `.` → `->` rewrite range for P16.5's auto-deref nudge.
    let (recv_end, is_arrow, sep_start, sep_end) =
        if prefix_start >= 1 && bytes[prefix_start - 1] == b'.' {
            (prefix_start - 1, false, prefix_start - 1, prefix_start)
        } else if prefix_start >= 2
            && bytes[prefix_start - 2] == b'-'
            && bytes[prefix_start - 1] == b'>'
        {
            (prefix_start - 2, true, prefix_start - 2, prefix_start)
        } else {
            return None;
        };

    let module = project.module(uri)?;
    let arena = project.arena();
    let well_known = project.well_known();
    let recv_ty = receiver_type_at(text, root, module, project.symbols(), recv_end)?;
    let name = type_head_name(project, arena, recv_ty)?;

    // `@deref`-driven completion: when the receiver's type declares
    // `@deref("methodName")`, the `*` / `->` deref desugars to a call
    // through that method, and member completion on `n->|` lists the
    // *deref target*'s members (not the tag's own). Read the cached
    // `TypeMembers::deref_return_ty` (populated by
    // `crate::project::populate_deref_caches` once signature lowering
    // settles), substitute the receiver's instantiation, and pull the
    // head name of the resulting type.
    let _ = well_known;
    let inner_head: Option<String> = (|| {
        let (recv_id, recv_args): (ItemId, Vec<TypeId>) = match &arena.get(recv_ty).kind {
            TypeKind::Type(d) => (*d, Vec::new()),
            TypeKind::Generic { decl, args } => (*decl, args.to_vec()),
            _ => return None,
        };
        let members = project.index.type_members.get(&recv_id)?;
        let deref_ret = members.deref_return_ty?;
        // Substitute the receiver's generic args into the cached
        // (still-abstract) deref-method return type.
        if recv_args.is_empty() {
            return type_head_name(project, arena, deref_ret).map(|s| s.to_string());
        }
        let mut subst: FxHashMap<Symbol, TypeId> = FxHashMap::default();
        for (i, gp_sym) in members.generics.iter().enumerate() {
            if let Some(arg) = recv_args.get(i) {
                subst.insert(*gp_sym, *arg);
            }
        }
        // Read-only completion path — clone the arena once, do the
        // substitution against the clone so the project's shared
        // arena stays untouched.
        let mut working_arena = arena.clone();
        let resolved = working_arena.substitute(deref_ret, &subst);
        type_head_name(project, &working_arena, resolved).map(|s| s.to_string())
    })();

    let mut items: Vec<CompletionItem> = Vec::new();

    // Substitution context for receiver-instantiation rendering. When
    // the receiver is `Array<String>`, build `{T → String}` so each
    // method completion item's `detail` shows `value: String` instead
    // of `value: T`.
    let recv_subst = project.method_subst_from_receiver_ty(recv_ty);
    let recv_ctx = recv_subst
        .as_ref()
        .map(|subst| RenderCtx { project, subst });

    // For `->` on a node-tag receiver, skip the tag's own members
    // entirely — those are reachable via `.` only. The analyzer's
    // `arrow_deref_receiver` mirrors this dispatch.
    let list_tag_members = !(is_arrow && inner_head.is_some());
    if list_tag_members {
        let name_sym = project.symbols().lookup(name);
        if let Some(name_sym) = name_sym
            && let Some(decl_id) = module.analysis.type_decls.get(&name_sym).copied()
            && let Decl::Type(td) = &module.hir.decls[decl_id]
        {
            collect_type_members(
                &module.hir,
                project.symbols(),
                td,
                &prefix_lower,
                &mut items,
                recv_ctx.as_ref(),
            );
        }
        if items.is_empty()
            && let Some(name_sym) = name_sym
            && let Some((foreign_uri, foreign_decl_id)) = project
                .index
                .locate_decl_in_ns(name_sym, crate::stdlib::Namespace::Type)
                .next()
            && let Some(fmod) = project.module(foreign_uri)
            && let Decl::Type(td) = &fmod.hir.decls[foreign_decl_id]
        {
            collect_type_members(
                &fmod.hir,
                project.symbols(),
                td,
                &prefix_lower,
                &mut items,
                recv_ctx.as_ref(),
            );
        }
    }

    // Inner type's members. `.` rewrites to `->` via
    // `additional_text_edits`; `->` lands the items verbatim.
    //
    // Deref-target subst is intentionally `None` here: the deref
    // resolution above mints types into a cloned arena, so any
    // generic args carried by `resolved` aren't addressable through
    // the shared project arena. Inner-method rendering stays in the
    // declared form for the deref branch.
    if let Some(inner) = inner_head.as_deref() {
        let inner_sym = project.symbols().lookup(inner);
        let mut inner_items: Vec<CompletionItem> = Vec::new();
        if let Some(inner_sym) = inner_sym
            && let Some(decl_id) = module.analysis.type_decls.get(&inner_sym).copied()
            && let Decl::Type(td) = &module.hir.decls[decl_id]
        {
            collect_type_members(
                &module.hir,
                project.symbols(),
                td,
                &prefix_lower,
                &mut inner_items,
                None,
            );
        }
        if inner_items.is_empty()
            && let Some(inner_sym) = inner_sym
            && let Some((foreign_uri, foreign_decl_id)) = project
                .index
                .locate_decl_in_ns(inner_sym, crate::stdlib::Namespace::Type)
                .next()
            && let Some(fmod) = project.module(foreign_uri)
            && let Decl::Type(td) = &fmod.hir.decls[foreign_decl_id]
        {
            collect_type_members(
                &fmod.hir,
                project.symbols(),
                td,
                &prefix_lower,
                &mut inner_items,
                None,
            );
        }
        if !is_arrow && !inner_items.is_empty() {
            // `.` → `->` rewrite. The edit replaces the `.` byte with
            // `->` so the accepted item lands in the correct shape.
            let edit_range = Range::from_byte_range(text, &(sep_start..sep_end), encoding);
            for item in &mut inner_items {
                item.additional_text_edits = Some(vec![TextEdit {
                    range: edit_range,
                    new_text: "->".into(),
                }]);
            }
        }
        items.extend(inner_items);
    }

    // P19.17 — when the receiver is nullable and the user typed `.` /
    // `->` (not `?.` / `?->`), each accepted item should land as the
    // null-safe form: insert a `?` immediately before the separator
    // via `additional_text_edits`. The label is rewritten to `?.size`
    // so the user *sees* what they're inserting before they accept;
    // `filter_text` and `sort_text` keep the bare name so typing `s`
    // still filters to `size` and the list ordering is unchanged from
    // the non-null case.
    //
    // **Skip when the receiver chain has an upstream `?.`** — optional
    // chaining short-circuits the whole suffix, so `n?.resolve().|`
    // is runtime-safe even though `n?.resolve()`'s type is `String?`.
    // Only the leading `?.` is needed; pushing more would be noise.
    let receiver_nullable = arena.get(recv_ty).nullable;
    let already_nullsafe = recv_end > 0 && bytes[recv_end - 1] == b'?';
    let chain_protected = module
        .hir
        .exprs
        .iter()
        .filter(|(_, e)| e.byte_range().end == recv_end)
        .max_by_key(|(_, e)| e.byte_range().end - e.byte_range().start)
        .map(|(id, _)| crate::lint::chain_has_upstream_nullsafe(&module.hir, id))
        .unwrap_or(false);
    if receiver_nullable && !already_nullsafe && !chain_protected {
        let insert_at = Range::from_byte_range(text, &(sep_start..sep_start), encoding);
        let prefix = if is_arrow { "?->" } else { "?." };
        for item in &mut items {
            let bare = item.label.clone();
            let mut edits = item.additional_text_edits.take().unwrap_or_default();
            edits.push(TextEdit {
                range: insert_at,
                new_text: "?".into(),
            });
            item.additional_text_edits = Some(edits);
            item.label = format!("{prefix}{bare}");
            if item.filter_text.is_none() {
                item.filter_text = Some(bare.clone());
            }
            if item.sort_text.is_none() {
                item.sort_text = Some(bare);
            }
        }
    }

    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

/// Find the receiver's `TypeId` whose CST span ends at `recv_end`.
/// Three-stage:
/// 1. **HIR fast path** — match an `Expr` whose byte_range ends there
///    against `analysis.expr_types`. Works for the common `recv.x`
///    case where the parser produced a `member_expr`.
/// 2. **CST + resolver fallback** — when the parser put the half-
///    formed access in an `ERROR` recovery node, no `Expr` covers
///    the receiver. We walk up the CST from the byte before
///    `recv_end`, find a named node whose `end_byte == recv_end`,
///    and try to resolve it via `Resolutions` + `def_types`.
/// 3. **CST + name-in-scope fallback** — when the receiver isn't
///    even in `Resolutions` (because the lowering skipped its
///    enclosing ERROR), look the receiver text up by name in the
///    enclosing fn's scope (params + nested var decls).
fn receiver_type_at(
    text: &str,
    root: tree_sitter::Node<'_>,
    module: &ModuleAnalysis,
    symbols: &SymbolTable,
    recv_end: usize,
) -> Option<TypeId> {
    // The user may type the `.` / `->` on a new line, with whitespace
    // (newline + indent) between the receiver expression's end and the
    // cursor's separator. Skip whitespace backward so the HIR fast path
    // finds the expression that ends at the *visible* receiver
    // position, not at the run of trivia that precedes the separator.
    // The bare-`.` parse here is `(member_expr <recv>)` with the
    // receiver salvaged into the block as a `Stmt::Expr`, so the
    // receiver's `byte_range().end` is the actual end of the receiver
    // (e.g. the `)` of the trailing call), not the dot itself.
    let bytes = text.as_bytes();
    let effective_end = trim_trailing_trivia(bytes, recv_end);
    if let Some((id, _)) = module
        .hir
        .exprs
        .iter()
        .filter(|(_, e)| e.byte_range().end == effective_end)
        .max_by_key(|(_, e)| e.byte_range().end - e.byte_range().start)
        && let Some(ty) = module.analysis.expr_types.get(&id).copied()
    {
        return Some(ty);
    }
    if recv_end == 0 {
        return None;
    }
    let leaf = node_at_offset(root, recv_end - 1)?;
    let mut cur = leaf;
    let recv_node = loop {
        if cur.is_named() && cur.end_byte() == recv_end {
            break cur;
        }
        match cur.parent() {
            Some(p) if p.end_byte() <= recv_end + 1 => cur = p,
            _ => return None,
        }
    };
    if recv_node.kind() != "ident" {
        return None;
    }
    let r = recv_node.byte_range();
    // Stage 2: ident already lowered into the HIR — resolver path.
    if let Some((ident_idx, _)) = module.hir.idents.iter().find(|(_, i)| i.byte_range == r) {
        use crate::resolver::Definition;
        use greycat_analyzer_hir::types::Decl;
        if let Some(def) = module.resolutions.lookup(ident_idx) {
            // P35.10 — `Definition::Decl(decl_id)` for a top-level
            // `var` resolves through the modvar's binding ident, now
            // present in `def_types` via the updated `visit_top_var`.
            // Without this the receiver of `n.foo` (where `n` is a
            // modvar) silently misses stage 2 and falls through to
            // the text-based stage 3.
            let ident_for_lookup = match def {
                Definition::Param(id) | Definition::Local(id) | Definition::Generic(id) => Some(id),
                Definition::Decl(decl_id) => match &module.hir.decls[decl_id] {
                    Decl::Var(vd) => Some(vd.name),
                    _ => None,
                },
                _ => None,
            };
            if let Some(id) = ident_for_lookup
                && let Some(ty) = module.analysis.def_types.get(&id).copied()
            {
                return Some(ty);
            }
        }
    }
    // Stage 3: ident dropped by lowering (lives inside an ERROR);
    // resolve by name lookup.
    let recv_text = text.get(r)?.to_string();
    lookup_name_type_at(&module.hir, symbols, &module.analysis, recv_end, &recv_text)
}

/// Skip ASCII whitespace bytes immediately before `end`, returning the
/// position just after the last non-whitespace byte. Used by
/// [`receiver_type_at`] so a `.` / `->` typed on a new line still
/// resolves to the chain on the previous line.
fn trim_trailing_trivia(bytes: &[u8], end: usize) -> usize {
    let mut e = end.min(bytes.len());
    while e > 0 && bytes[e - 1].is_ascii_whitespace() {
        e -= 1;
    }
    e
}

/// Walk the HIR for a Param / Local binding whose name matches `name`
/// and whose enclosing scope contains `cursor_byte`. Returns its
/// `TypeId` from `def_types`.
fn lookup_name_type_at(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    analysis: &crate::analyzer::AnalysisResult,
    cursor_byte: usize,
    name: &str,
) -> Option<TypeId> {
    use greycat_analyzer_hir::types::Decl as HD;
    let module = hir.module.as_ref()?;
    // P35.10 — first pass: top-level `Decl::Var`. Module-level vars
    // are visible from *anywhere* in the module body (unlike fn / type
    // method scopes which are bounded by the decl's byte_range), so
    // they're checked once before any byte-range filter. This is what
    // makes the ERROR-recovery completion path work for receivers
    // like `var n: node<int?>; ... n.` where the body's `n` ident
    // lives inside a skipped `(ERROR (ident))` and the fn-scope walk
    // below never sees it.
    for &decl_id in &module.decls {
        if let HD::Var(vd) = &hir.decls[decl_id]
            && symbols[hir.idents[vd.name].symbol] == *name
            && let Some(t) = analysis.def_types.get(&vd.name).copied()
        {
            return Some(t);
        }
    }
    for &decl_id in &module.decls {
        let r = hir.decls[decl_id].byte_range();
        if !(r.start <= cursor_byte && cursor_byte <= r.end) {
            continue;
        }
        match &hir.decls[decl_id] {
            HD::Fn(fnd) => {
                if let Some(t) =
                    lookup_name_type_in_fn(hir, symbols, analysis, cursor_byte, fnd, name)
                {
                    return Some(t);
                }
            }
            HD::Type(td) => {
                for &m_id in &td.methods {
                    let mr = hir.decls[m_id].byte_range();
                    if !(mr.start <= cursor_byte && cursor_byte <= mr.end) {
                        continue;
                    }
                    if let HD::Fn(fnd) = &hir.decls[m_id]
                        && let Some(t) =
                            lookup_name_type_in_fn(hir, symbols, analysis, cursor_byte, fnd, name)
                    {
                        return Some(t);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn lookup_name_type_in_fn(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    analysis: &crate::analyzer::AnalysisResult,
    cursor_byte: usize,
    fnd: &greycat_analyzer_hir::types::FnDecl,
    name: &str,
) -> Option<TypeId> {
    for p_id in &fnd.params {
        let p = &hir.fn_params[*p_id];
        if symbols[hir.idents[p.name].symbol] == *name {
            return analysis.def_types.get(&p.name).copied();
        }
    }
    if let Some(body) = fnd.body {
        return lookup_name_type_in_stmt(hir, symbols, analysis, cursor_byte, body, name);
    }
    None
}

fn lookup_name_type_in_block(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    analysis: &crate::analyzer::AnalysisResult,
    cursor_byte: usize,
    block: &greycat_analyzer_hir::types::BlockStmt,
    name: &str,
) -> Option<TypeId> {
    use greycat_analyzer_hir::types::Stmt as HS;
    if !(block.byte_range.start <= cursor_byte && cursor_byte <= block.byte_range.end) {
        return None;
    }
    for s in &block.stmts {
        let r = stmt_byte_range(hir, *s);
        if r.end <= cursor_byte {
            if let HS::Var(lv) = &hir.stmts[*s]
                && symbols[hir.idents[lv.name].symbol] == *name
            {
                return analysis.def_types.get(&lv.name).copied();
            }
        } else if r.start <= cursor_byte
            && cursor_byte <= r.end
            && let Some(t) = lookup_name_type_in_stmt(hir, symbols, analysis, cursor_byte, *s, name)
        {
            return Some(t);
        }
    }
    None
}

fn lookup_name_type_in_stmt(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    analysis: &crate::analyzer::AnalysisResult,
    cursor_byte: usize,
    stmt_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Stmt>,
    name: &str,
) -> Option<TypeId> {
    use greycat_analyzer_hir::types::Stmt as HS;
    match &hir.stmts[stmt_id] {
        HS::Block(b) => lookup_name_type_in_block(hir, symbols, analysis, cursor_byte, b, name),
        HS::If(s) => {
            if let Some(t) =
                lookup_name_type_in_block(hir, symbols, analysis, cursor_byte, &s.then_branch, name)
            {
                return Some(t);
            }
            if let Some(eb) = s.else_branch {
                let er = stmt_byte_range(hir, eb);
                if er.start <= cursor_byte && cursor_byte <= er.end {
                    return lookup_name_type_in_stmt(hir, symbols, analysis, cursor_byte, eb, name);
                }
            }
            None
        }
        HS::While(s) => {
            lookup_name_type_in_block(hir, symbols, analysis, cursor_byte, &s.body, name)
        }
        HS::DoWhile(s) => {
            lookup_name_type_in_block(hir, symbols, analysis, cursor_byte, &s.body, name)
        }
        HS::For(s) => {
            if let Some(name_id) = s.init_name
                && symbols[hir.idents[name_id].symbol] == *name
            {
                return analysis.def_types.get(&name_id).copied();
            }
            lookup_name_type_in_block(hir, symbols, analysis, cursor_byte, &s.body, name)
        }
        HS::ForIn(s) => {
            for p in &s.params {
                if symbols[hir.idents[p.name].symbol] == *name {
                    return analysis.def_types.get(&p.name).copied();
                }
            }
            lookup_name_type_in_block(hir, symbols, analysis, cursor_byte, &s.body, name)
        }
        HS::Try(s) => {
            if let Some(t) =
                lookup_name_type_in_block(hir, symbols, analysis, cursor_byte, &s.try_block, name)
            {
                return Some(t);
            }
            if s.catch_block.byte_range.start <= cursor_byte
                && cursor_byte <= s.catch_block.byte_range.end
            {
                if let Some(err_id) = s.error_param
                    && symbols[hir.idents[err_id].symbol] == *name
                {
                    return analysis.def_types.get(&err_id).copied();
                }
                return lookup_name_type_in_block(
                    hir,
                    symbols,
                    analysis,
                    cursor_byte,
                    &s.catch_block,
                    name,
                );
            }
            None
        }
        HS::At(s) => lookup_name_type_in_block(hir, symbols, analysis, cursor_byte, &s.block, name),
        _ => None,
    }
}

/// Read the head name of `id` from `arena` — the bare type name
/// stripped of nullability / generic args. Returns `None` for shapes
/// without a single name (lambdas, tuples, anonymous structures).
fn type_head_name<'a>(
    pa: &'a ProjectAnalysis,
    arena: &'a TypeArena,
    id: TypeId,
) -> Option<&'a str> {
    use greycat_analyzer_core::TypeKind;
    let t = arena.get(id);
    match &t.kind {
        // P35.7 — handle-keyed variants carry the name in the `ItemId`.
        TypeKind::Type(d) => Some(pa.decl_name(*d)),
        TypeKind::Generic { decl, .. } => Some(pa.decl_name(*decl)),
        TypeKind::Primitive(p) => Some(p.name()),
        _ => None,
    }
}

/// Walk a `TypeDecl`'s attrs + methods and emit one `CompletionItem`
/// per name that survives the `prefix_lower` filter. Skips abstract /
/// native methods only on the static-completion side;
/// instance access lists everything.
fn collect_type_members(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    td: &greycat_analyzer_hir::types::TypeDecl,
    prefix_lower: &str,
    items: &mut Vec<CompletionItem>,
    ctx: Option<&RenderCtx<'_>>,
) {
    for attr_id in &td.attrs {
        let a = &hir.type_attrs[*attr_id];
        // `static` attrs (e.g. `int::min`, `int::max`) belong to the
        // static-access path (`Type::|`), not instance access (`x.|`).
        // Listing them on an instance leaks `min` / `max` into `42.|`
        // completion where they aren't reachable.
        if a.modifiers.static_ {
            continue;
        }
        let name = symbols[hir.idents[a.name].symbol].to_string();
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(prefix_lower) {
            continue;
        }
        let ty =
            a.ty.map(|t| render_type_ref_with_subst(hir, symbols, t, ctx))
                .unwrap_or_else(|| "any".into());
        // Compact `: T` form — label already carries the name, so
        // repeating it in detail (`a a: int`) just wastes width.
        // Matches the fn/method `(args): Ret` convention.
        let compact = format!(": {ty}");
        items.push(CompletionItem {
            label: name.clone(),
            label_details: Some(CompletionItemLabelDetails {
                detail: Some(compact.clone()),
                description: None,
            }),
            kind: Some(CompletionItemKind::Field),
            insert_text: Some(name.clone()),
            detail: Some(compact),
            documentation: doc_to_markup(a.doc.as_deref()),
            ..Default::default()
        });
    }
    for method_id in &td.methods {
        let Decl::Fn(m) = &hir.decls[*method_id] else {
            continue;
        };
        // `static` methods don't apply to instance access (P15.2.5
        // owns the static-call path); skip them here.
        if m.modifiers.static_ {
            continue;
        }
        let name = symbols[hir.idents[m.name].symbol].to_string();
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(prefix_lower) {
            continue;
        }
        let compact = render_fn_signature_compact(hir, symbols, m, ctx);
        items.push(CompletionItem {
            label: name.clone(),
            label_details: Some(CompletionItemLabelDetails {
                detail: Some(compact.clone()),
                description: None,
            }),
            kind: Some(CompletionItemKind::Method),
            insert_text: Some(name),
            detail: Some(compact),
            documentation: doc_to_markup(m.doc.as_deref()),
            ..Default::default()
        });
    }
}

/// Carry a doc-comment paragraph through to the completion item as
/// markdown text. The server wrapper lifts it into the LSP
/// `Documentation::MarkupContent(MarkupKind::Markdown)` shape; the
/// wasm bridge surfaces the bare string. Returns `None` for missing /
/// blank docs so the field stays absent on the wire.
fn doc_to_markup(doc: Option<&str>) -> Option<String> {
    let trimmed = doc?.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

// =============================================================================
// P15.2.5 — static completion after `::`
// =============================================================================

/// Static-access completion: when the cursor sits in `Type::|prop` or
/// `module::|name`, list the type's static methods or the module's
/// top-level decls. Receiver detection:
/// - Walk back from the cursor over `[A-Za-z0-9_]*` (typed prefix).
/// - Confirm `::` precedes the prefix.
/// - Walk back further over `[A-Za-z0-9_]+` to capture the receiver
///   ident.
///
/// Two dispatch shapes:
/// 1. `Type::|` — receiver matches a known type decl. Emit its
///    `static` methods. Chain context (`module::Type::|`) is
///    transparent: we still look the type up by name.
/// 2. `module::|` — receiver matches `ProjectIndex::module_names`.
///    Emit that module's top-level decls.
fn static_completion(
    text: &str,
    cursor_byte: usize,
    project: &crate::project::ProjectAnalysis,
    encoding: SourceEncoding,
) -> Option<Vec<CompletionItem>> {
    let ctx = static_receiver_at(text, cursor_byte)?;
    let prefix_lower = ctx.typed.to_lowercase();
    let replace_range = Range::from_byte_range(text, &ctx.replace_range, encoding);

    let mut items: Vec<CompletionItem> = Vec::new();

    // Receiver branch: type-decl → static methods, enum-decl →
    // variants. The `recv` text matches a top-level decl name in some
    // module (resolved through the project decl table). Filter to the
    // type namespace — a value-namespace `fn ctx.recv()` is irrelevant
    // here, the receiver is a static dispatch target.
    if let Some(recv_sym) = project.symbols().lookup(&ctx.recv)
        && let Some((foreign_uri, foreign_decl_id)) = project
            .index
            .locate_decl_in_ns(recv_sym, crate::stdlib::Namespace::Type)
            .next()
        && let Some(fmod) = project.module(foreign_uri)
    {
        match &fmod.hir.decls[foreign_decl_id] {
            Decl::Type(td) => {
                for method_id in &td.methods {
                    let Decl::Fn(m) = &fmod.hir.decls[*method_id] else {
                        continue;
                    };
                    if !m.modifiers.static_ {
                        continue;
                    }
                    let name = project.symbols()[fmod.hir.idents[m.name].symbol].to_string();
                    if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
                        continue;
                    }
                    let compact =
                        render_fn_signature_compact(&fmod.hir, project.symbols(), m, None);
                    let label_details = Some(CompletionItemLabelDetails {
                        detail: Some(compact.clone()),
                        description: None,
                    });
                    let documentation = doc_to_markup(m.doc.as_deref());
                    items.push(static_completion_item(
                        name,
                        CompletionItemKind::Method,
                        replace_range,
                        label_details,
                        Some(compact),
                        documentation,
                    ));
                }
                for attr_id in &td.attrs {
                    let attr = &fmod.hir.type_attrs[*attr_id];
                    if !attr.modifiers.static_ {
                        continue;
                    }
                    let name = project.symbols()[fmod.hir.idents[attr.name].symbol].to_string();
                    if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
                        continue;
                    }
                    let detail = attr.ty.map(|tr| {
                        format!(
                            ": {}",
                            crate::ide::render::render_type_ref(&fmod.hir, project.symbols(), tr,)
                        )
                    });
                    let label_details = detail.clone().map(|d| CompletionItemLabelDetails {
                        detail: Some(d),
                        description: None,
                    });
                    let documentation = doc_to_markup(attr.doc.as_deref());
                    items.push(static_completion_item(
                        name,
                        CompletionItemKind::Constant,
                        replace_range,
                        label_details,
                        detail,
                        documentation,
                    ));
                }
            }
            Decl::Enum(ed) => {
                // `Foo::|` where `Foo` is an enum — surface every
                // variant. Common in stdlib: `core::TimeZone` ships
                // 600+ IANA-spelled variants (`"Africa/Abidjan"`,
                // `"America/New_York"`, …) so we keep the per-item
                // path allocation-light.
                for f in &ed.fields {
                    let name =
                        &project.symbols()[fmod.hir.idents[fmod.hir.enum_fields[*f].name].symbol];
                    if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
                        continue;
                    }
                    items.push(enum_variant_completion_item(
                        name,
                        ctx.in_string,
                        replace_range,
                    ));
                }
            }
            _ => {}
        }
    }

    // Module-receiver branch: enumerate the module's top-level decls.
    if let Some(recv_sym) = project.symbols().lookup(&ctx.recv)
        && let Some(mod_uri) = project.index.module_names.get(&recv_sym).cloned()
        && let Some(mod_analysis) = project.module(&mod_uri)
        && let Some(module_hir) = mod_analysis.hir.module.as_ref()
    {
        for &decl_id in &module_hir.decls {
            let Some(name_id) = mod_analysis.hir.decls[decl_id].name() else {
                continue;
            };
            let name = project.symbols()[mod_analysis.hir.idents[name_id].symbol].to_string();
            if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
                continue;
            }
            let decl = &mod_analysis.hir.decls[decl_id];
            let kind = match decl {
                Decl::Fn(_) => CompletionItemKind::Function,
                Decl::Type(_) => CompletionItemKind::Class,
                Decl::Enum(_) => CompletionItemKind::Enum,
                Decl::Var(_) => CompletionItemKind::Variable,
                Decl::Pragma(_) => continue,
            };
            let (label_details, detail) = match decl {
                Decl::Fn(fnd) => {
                    let compact = render_fn_signature_compact(
                        &mod_analysis.hir,
                        project.symbols(),
                        fnd,
                        None,
                    );
                    (
                        Some(CompletionItemLabelDetails {
                            detail: Some(compact.clone()),
                            description: None,
                        }),
                        Some(compact),
                    )
                }
                Decl::Type(_) | Decl::Enum(_) | Decl::Var(_) => {
                    let module_label = module_label_for_uri(&mod_uri);
                    (
                        Some(CompletionItemLabelDetails {
                            detail: None,
                            description: Some(module_label.clone()),
                        }),
                        Some(module_label),
                    )
                }
                _ => (
                    None,
                    Some(render_decl_signature(
                        &mod_analysis.hir,
                        project.symbols(),
                        decl,
                        None,
                    )),
                ),
            };
            let documentation = doc_to_markup(decl_doc(decl));
            items.push(static_completion_item(
                name,
                kind,
                replace_range,
                label_details,
                detail,
                documentation,
            ));
        }
    }

    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

/// Receiver context for `Recv::|prop` / `Recv::"prop|"` completion.
///
/// `replace_range` covers the whole property token at the cursor
/// from the start of the typed prefix to the end of the surrounding
/// ident run (or to the closing `"` for string-mode). Threading this
/// into every completion item's `text_edit` is what keeps "ask for
/// completion in the middle of a word" honest: the accepted text
/// replaces the existing word instead of doubling it via a naive
/// `insert_text` insertion at the cursor.
struct StaticRecvCtx {
    /// Receiver name (`Foo`, `runtime`, …). Plain ident text, no
    /// quotes / separators.
    recv: String,
    /// What the user has typed so far at the cursor; the prefix
    /// filter for completion items. Always derived from the source
    /// chars from `replace_range.start..cursor_byte`.
    typed: String,
    /// Replace-range as UTF-8 byte offsets. For ident-mode this is
    /// `[prop_start..prop_end]` covering every alphanumeric run
    /// around the cursor. For string-mode this is the inner content
    /// span `[after_open_quote..before_close_quote]` (cursor INSIDE
    /// the quotes, opening/closing kept).
    replace_range: std::ops::Range<usize>,
    /// `true` when the cursor sits inside `Recv::"…|"`. The opening
    /// `"` is in the buffer, so completion items emit bare names
    /// (no re-quoting).
    in_string: bool,
}

/// Walk back from `cursor_byte` to extract the static-access receiver
/// and the byte range to replace. Returns `None` when the cursor
/// isn't in a `Recv::|` / `Recv::"|"` shape.
fn static_receiver_at(text: &str, cursor_byte: usize) -> Option<StaticRecvCtx> {
    let bytes = text.as_bytes();
    let cap = cursor_byte.min(bytes.len());

    // String-mode: the cursor sits inside `recv::"…|…"`. Walk back
    // over non-`"` chars to find the opening quote, then forward
    // from the cursor over non-`"` chars to find the closing quote
    // (or EOL — we stop at `\n` so an unterminated string doesn't
    // swallow the rest of the file).
    {
        let mut i = cap;
        while i > 0 && bytes[i - 1] != b'"' && bytes[i - 1] != b'\n' {
            i -= 1;
        }
        if i >= 3 && bytes[i - 1] == b'"' && bytes[i - 2] == b':' && bytes[i - 3] == b':' {
            let inner_start = i;
            let mut j = cap;
            while j < bytes.len() && bytes[j] != b'"' && bytes[j] != b'\n' {
                j += 1;
            }
            let inner_end = j;
            let typed = text.get(inner_start..cap).unwrap_or("").to_string();
            let sep_start = i - 3;
            let recv = walk_back_receiver(bytes, sep_start, text)?;
            return Some(StaticRecvCtx {
                recv,
                typed,
                replace_range: inner_start..inner_end,
                in_string: true,
            });
        }
    }

    // Ident-mode: `recv::|prop`. Walk back over `[A-Za-z0-9_]` for
    // the prefix and forward from the cursor over the same class
    // for the rest of the surrounding ident — so completion in the
    // middle of `Foo::Tim|eZone` replaces the whole `TimeZone` run.
    let typed = ident_prefix_at_cursor(text, cursor_byte);
    let prefix_start = cap.saturating_sub(typed.len());
    if prefix_start < 2 || bytes[prefix_start - 1] != b':' || bytes[prefix_start - 2] != b':' {
        return None;
    }
    let mut j = cap;
    while j < bytes.len() {
        let b = bytes[j];
        if b.is_ascii_alphanumeric() || b == b'_' {
            j += 1;
        } else {
            break;
        }
    }
    let sep_start = prefix_start - 2;
    let recv = walk_back_receiver(bytes, sep_start, text)?;
    Some(StaticRecvCtx {
        recv,
        typed,
        replace_range: prefix_start..j,
        in_string: false,
    })
}

/// Shared receiver walk-back used by both static-completion modes
/// (ident property and string property). Walks left from `sep_start`
/// over `[A-Za-z0-9_]` chars and slices the receiver text. Returns
/// `None` when no receiver run is present.
fn walk_back_receiver(bytes: &[u8], sep_start: usize, text: &str) -> Option<String> {
    let mut i = sep_start;
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i -= 1;
        } else {
            break;
        }
    }
    if i == sep_start {
        return None;
    }
    text.get(i..sep_start).map(str::to_string)
}

// =============================================================================
// P15.2.6 — type-position completion
// =============================================================================

/// Type-position completion: when the cursor sits inside a
/// `type_ident` slot — `var x: |`, `<|`, `extends |`, fn param /
/// return type, etc. — emit only type-shaped names (in-module type /
/// enum decls + every type registered in the project's
/// [`ProjectIndex`] + runtime types + primitives) prefix-filtered.
fn type_position_completion(
    text: &str,
    node: tree_sitter::Node<'_>,
    cursor_byte: usize,
    uri: &Uri,
    project: &crate::project::ProjectAnalysis,
) -> Option<Vec<CompletionItem>> {
    ancestor_with_kind(node, "type_ident")?;
    // Bail if we're on the RHS of a member / static / annotation chain.
    let bytes = text.as_bytes();
    let cap = cursor_byte.min(bytes.len());
    let mut i = cap;
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i -= 1;
        } else {
            break;
        }
    }
    if i > 0 && matches!(bytes[i - 1], b'.' | b'>' | b'@') {
        return None;
    }
    // Allow `module::|Type` — the static branch already handles that.
    if i >= 2 && bytes[i - 1] == b':' && bytes[i - 2] == b':' {
        return None;
    }
    let typed = ident_prefix_at_cursor(text, cursor_byte);
    let prefix_lower = typed.to_lowercase();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut items: Vec<CompletionItem> = Vec::new();
    let push = |items: &mut Vec<CompletionItem>,
                seen: &mut FxHashSet<String>,
                name: &str,
                kind: CompletionItemKind| {
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
            return;
        }
        if seen.insert(name.into()) {
            items.push(CompletionItem {
                label: name.into(),
                kind: Some(kind),
                insert_text: Some(name.into()),
                ..Default::default()
            });
        }
    };

    // In-module decls (always visible at top level).
    if let Some(module) = project.module(uri)
        && let Some(m) = module.hir.module.as_ref()
    {
        for decl_id in &m.decls {
            let kind = match &module.hir.decls[*decl_id] {
                Decl::Type(_) => CompletionItemKind::Class,
                Decl::Enum(_) => CompletionItemKind::Enum,
                _ => continue,
            };
            if let Some(name_id) = module.hir.decls[*decl_id].name() {
                let name = project.symbols()[module.hir.idents[name_id].symbol].to_string();
                push(&mut items, &mut seen, &name, kind);
            }
        }
    }
    // In-scope generic type-params from the enclosing fn / type.
    if let Some(module) = project.module(uri) {
        for (name, kind, _, _) in scope_names_at(&module.hir, project.symbols(), cursor_byte) {
            if matches!(kind, CompletionItemKind::TypeParameter) {
                push(&mut items, &mut seen, &name, kind);
            }
        }
    }
    // Project-level type / enum decls.
    for (name_sym, locs) in &project.index.decl_locations {
        let name = project.index.symbols.resolve(name_sym);
        if let Some((u, d, _)) = locs.first()
            && let Some(m) = project.module(u)
        {
            let kind = match &m.hir.decls[*d] {
                Decl::Type(_) => CompletionItemKind::Class,
                Decl::Enum(_) => CompletionItemKind::Enum,
                _ => continue,
            };
            push(&mut items, &mut seen, name, kind);
        }
    }
    // Primitives.
    for &p in &[
        "int", "float", "bool", "char", "String", "time", "duration", "geo", "any",
    ] {
        push(&mut items, &mut seen, p, CompletionItemKind::Class);
    }
    // Module names — type slots can read `module::Foo`, so module
    // names are valid here as the leading segment.
    for name_sym in project.index.module_names.keys() {
        let name = project.index.symbols.resolve(name_sym);
        push(&mut items, &mut seen, name, CompletionItemKind::Module);
    }

    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

// =============================================================================
// P15.2.7 — object literal field completion
// =============================================================================

/// Emit object-literal field names for the literal whose body is
/// `body` (an `object_initializers` / `object_fields` node, supplied by
/// the slot classifier). Resolves the surrounding `object_expr`'s
/// `type_ident` head, walks the local supertype chain, and emits each
/// non-static `FIELD` not already named in the literal. Returns `None`
/// when the head doesn't resolve to a type with attrs (a collection /
/// tuple / unknown head) — the caller then falls back to positional
/// expression completion.
fn emit_object_field_names(
    text: &str,
    body: tree_sitter::Node<'_>,
    cursor_byte: usize,
    uri: &Uri,
    project: &crate::project::ProjectAnalysis,
) -> Option<Vec<CompletionItem>> {
    let object_expr = ancestor_with_kind(body, "object_expr")?;
    let type_ident = children_by_field_name(object_expr, "type")?;
    let type_name_node = type_ident.named_child(0)?;
    if type_name_node.kind() != "ident" {
        return None;
    }
    let type_name = text.get(type_name_node.byte_range())?.to_string();

    let typed = ident_prefix_at_cursor(text, cursor_byte);
    let prefix_lower = typed.to_lowercase();

    // Collect already-supplied field names from sibling `object_field`
    // entries in this literal so we don't suggest the same name twice.
    // Skip the field whose own ident the cursor sits inside (the user
    // is editing that one — let normal prefix-matching surface it).
    let supplied = supplied_field_names(text, body, cursor_byte);

    // Find the type's HIR (in-module first, then cross-module). Then
    // walk the local supertype chain, accumulating attrs from each
    // ancestor we can resolve.
    let module = project.module(uri)?;
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut emitted: FxHashSet<String> = FxHashSet::default();

    let visit_type_decl = |hir: &greycat_analyzer_hir::Hir,
                           td: &greycat_analyzer_hir::types::TypeDecl,
                           items: &mut Vec<CompletionItem>,
                           emitted: &mut FxHashSet<String>| {
        emit_attrs(
            hir,
            project.symbols(),
            td,
            &prefix_lower,
            &supplied,
            emitted,
            items,
        );
    };

    // Whether the head resolved to a named-attr type at all. Kept
    // distinct from `items` being empty: a resolved type with every
    // field already supplied returns `Some(vec![])` (no completion,
    // no leak), whereas an unresolved head returns `None` so the caller
    // falls back to positional expression completion (collections, etc).
    let mut resolved = false;

    if let Some(type_sym) = project.symbols().lookup(type_name.as_str())
        && let Some(decl_id) = module.analysis.type_decls.get(&type_sym).copied()
    {
        resolved = true;
        // Walk Sub → Super (local chain) just like the analyzer's
        // required-attr check.
        let mut hops = 0usize;
        let mut seen: FxHashSet<Symbol> = FxHashSet::default();
        let mut cursor_decl: Option<Idx<Decl>> = Some(decl_id);
        let mut cursor_name = type_sym;
        while let Some(d_id) = cursor_decl {
            if !seen.insert(cursor_name) || hops > 32 {
                break;
            }
            hops += 1;
            let Decl::Type(td) = &module.hir.decls[d_id] else {
                break;
            };
            visit_type_decl(&module.hir, td, &mut items, &mut emitted);
            let Some(sup_ref) = td.supertype else { break };
            let sup_tr = &module.hir.type_refs[sup_ref];
            if !sup_tr.qualifier.is_empty() {
                break;
            }
            let sup_name_sym = module.hir.idents[sup_tr.name].symbol;
            cursor_name = sup_name_sym;
            cursor_decl = module.analysis.type_decls.get(&sup_name_sym).copied();
        }
    }
    if !resolved
        && let Some(type_sym) = project.symbols().lookup(type_name.as_str())
        && let Some((foreign_uri, foreign_decl_id)) = project
            .index
            .locate_decl_in_ns(type_sym, crate::stdlib::Namespace::Type)
            .next()
        && let Some(fmod) = project.module(foreign_uri)
        && let Decl::Type(td) = &fmod.hir.decls[foreign_decl_id]
    {
        resolved = true;
        visit_type_decl(&fmod.hir, td, &mut items, &mut emitted);
    }
    // Unresolved head (collection / tuple / unknown) — let the caller
    // do positional expression completion instead.
    if !resolved {
        return None;
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

/// Read the `name:` idents of every `object_field` sibling inside
/// the given `object_initializers` / `object_fields` body, dropping
/// the field whose own name range contains the cursor (user is
/// editing that field's name — we still want it in completion).
fn supplied_field_names(
    text: &str,
    body: tree_sitter::Node<'_>,
    cursor_byte: usize,
) -> FxHashSet<String> {
    let mut out: FxHashSet<String> = FxHashSet::default();
    let mut walker = body.walk();
    for child in body.named_children(&mut walker) {
        if child.kind() != "object_field" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let range = name_node.byte_range();
        // Cursor inside this field's name → skip; it's the one being
        // edited and we still want prefix-matched completion for it.
        if cursor_byte >= range.start && cursor_byte <= range.end {
            continue;
        }
        if let Some(name) = text.get(range) {
            out.insert(name.to_string());
        }
    }
    out
}

fn emit_attrs(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    td: &greycat_analyzer_hir::types::TypeDecl,
    prefix_lower: &str,
    supplied: &FxHashSet<String>,
    emitted: &mut FxHashSet<String>,
    items: &mut Vec<CompletionItem>,
) {
    for attr_id in &td.attrs {
        let a = &hir.type_attrs[*attr_id];
        // Static attrs aren't part of the per-instance schema; they
        // belong to `Type::|` static access, not object-literal init.
        if a.modifiers.static_ {
            continue;
        }
        let name = symbols[hir.idents[a.name].symbol].to_string();
        if supplied.contains(&name) {
            continue;
        }
        if !emitted.insert(name.clone()) {
            // Already emitted by a deeper level in the chain — a child
            // type's attr shadows the parent's same-named one.
            continue;
        }
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(prefix_lower) {
            continue;
        }
        let ty =
            a.ty.map(|t| render_type_ref_with_subst(hir, symbols, t, None))
                .unwrap_or_else(|| "any".into());
        let compact = format!(": {ty}");
        items.push(CompletionItem {
            label: name.clone(),
            label_details: Some(CompletionItemLabelDetails {
                detail: Some(compact.clone()),
                description: None,
            }),
            kind: Some(CompletionItemKind::Field),
            insert_text: Some(format!("{name}: ")),
            detail: Some(compact),
            documentation: doc_to_markup(a.doc.as_deref()),
            ..Default::default()
        });
    }
}

/// Helper mirroring `tree_sitter::Node::child_by_field_name` that
/// returns an `Option`.
fn children_by_field_name<'a>(
    node: tree_sitter::Node<'a>,
    field: &str,
) -> Option<tree_sitter::Node<'a>> {
    node.child_by_field_name(field)
}
