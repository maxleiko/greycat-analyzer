// P1.4 — parse-time diagnostic extraction. P15.5 added
// [`pragma_diagnostics`].
//! Parse-time diagnostic extraction.
//!
//! Walks a tree-sitter [`Tree`] and emits one [`Diagnostic`] per `ERROR`
//! or `MISSING` node. The TS reference produces semantically richer parse
//! diagnostics from its hand-rolled CST (it knows what tokens it expected
//! vs. saw); tree-sitter's recovery is more opaque, so we lean on
//! `node.kind()` plus the node's source-text snippet for context.
//!
//! Semantic diagnostics (resolver, type-check, etc.) are out of scope here
//! — they arrive separately. [`pragma_diagnostics`] surfaces unresolved /
//! duplicate `@include` / `@library` pragmas like other diags.

use std::path::Path;

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Uri};
use rustc_hash::FxHashSet;

use greycat_analyzer_syntax::tree_sitter;

use crate::SourceEncoding;
use crate::conv::byte_to_position;
use crate::module_desc::ModuleDesc;
use crate::resolver::{Context, global_std_dir, library_dir};

/// Source string used as the `source` field of every diagnostic this
/// module produces. Lets editors filter / group them.
pub const DIAGNOSTIC_SOURCE: &str = "greycat-analyzer";

/// Walk `root` and return every parse-stage diagnostic: tree-sitter
/// `ERROR` / `MISSING` recoveries plus the "permissive-grammar,
/// strict-analyzer" shape checks for constructs the grammar accepts
/// to keep mid-edit recovery clean but the analyzer rejects on
/// semantic grounds (`Foo::` / `s.` / `s->` with no property,
/// non-native/non-abstract functions without a body, `var` with no
/// name, `var` terminated by auto-semi rather than an explicit `;`,
/// reserved keywords used as identifiers in positions the runtime
/// rejects or where the declaration would be unreachable, …).
/// Every call site (CLI lint, LSP backend, WASM bridge) goes
/// through this single entry point so a new shape check is one edit
/// here, not five.
pub fn parse_diagnostics(
    root: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if root.has_error() || root.is_missing() {
        walk(root, source, encoding, &mut out);
    }
    walk_shape_checks(root, source, encoding, &mut out);
    out
}

/// Single recursive walk that fires every CST-shape check. Folded
/// together so we only traverse the tree once and adding a new
/// check is a single match arm rather than another standalone
/// walker wired at every call site.
fn walk_shape_checks(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
    out: &mut Vec<Diagnostic>,
) {
    match node.kind() {
        "static_expr" => check_property_after(node, source, encoding, out, "::"),
        "member_expr" => check_property_after(node, source, encoding, out, "."),
        "arrow_expr" => check_property_after(node, source, encoding, out, "->"),
        "fn_decl" | "type_method" => check_function_body(node, source, encoding, out),
        "type_attr" => check_attr_init_modifier(node, source, encoding, out),
        "var_decl" => {
            check_var_name(node, source, encoding, out);
            check_explicit_semi(node, source, encoding, out);
        }
        "modvar" => {
            check_var_name(node, source, encoding, out);
            check_modvar_type(node, source, encoding, out);
            check_modvar_initializer(node, source, encoding, out);
            check_explicit_semi(node, source, encoding, out);
        }
        // Grammar accepts most block-style statements at module scope
        // (wrapped in `mod_stmt`) so doc snippets pretty-print under
        // the same tree-sitter highlighter as real modules. A real
        // project module cannot contain a freestanding stmt; flag the
        // whole wrapper with `top-level-stmt`. Inner shape checks
        // (`Foo::` / `s.` / `s->` / keyword-ident / missing-`;` on
        // the inner expr_stmt) are intentionally NOT recursed: the
        // stmt is invalid as a whole, piling per-fragment diags on
        // top is noise.
        "mod_stmt" => {
            check_top_level_stmt(node, source, encoding, out);
            return;
        }
        // Every stmt kind whose terminator is `choice(_semi, _automatic_semicolon)`
        // in grammar.js — the ASI is a parser convenience for mid-edit
        // recovery, never semantically valid GreyCat.
        "expr_stmt" | "return_stmt" | "throw_stmt" | "break_stmt" | "continue_stmt"
        | "breakpoint_stmt" | "do_while_stmt" => {
            check_explicit_semi(node, source, encoding, out);
        }
        "ident" => check_ident_keyword(node, source, encoding, out),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_shape_checks(child, source, encoding, out);
    }
}

/// `Foo::` / `s.` / `s->` all parse as well-formed `static_expr` /
/// `member_expr` / `arrow_expr` under the permissive grammar so a
/// mid-edit caret doesn't ERROR-recover the following statement; the
/// semantic requirement that an identifier or string property follow
/// the separator is enforced here. `sep` is the literal separator
/// token (`"::"`, `"."`, `"->"`) — diagnostic range points at it.
fn check_property_after(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
    out: &mut Vec<Diagnostic>,
    sep: &str,
) {
    match node.child_by_field_name("property") {
        Some(_) => {
            // if let (_, Some(post)) = cst::optional_flags_around(node, prop.id()) {
            //     out.push(Diagnostic {
            //         range: byte_range_to_lsp(source, &post, encoding),
            //         severity: Some(DiagnosticSeverity::ERROR),
            //         code: Some(NumberOrString::String("unexpected-token".into())),
            //         source: Some(DIAGNOSTIC_SOURCE.into()),
            //         message: "unexpected `?` token".into(),
            //         ..Default::default()
            //     });
            // }
        }
        None => {
            let sep_range = separator_range(node, source, sep).unwrap_or(node.byte_range());
            let code = match sep {
                "::" => "missing-static-property",
                "." => "missing-member-property",
                "->" => "missing-arrow-property",
                _ => "missing-property",
            };
            out.push(Diagnostic {
                range: byte_range_to_lsp(source, &sep_range, encoding),
                severity: Some(DiagnosticSeverity::ERROR),
                code: Some(NumberOrString::String(code.into())),
                source: Some(DIAGNOSTIC_SOURCE.into()),
                message: format!("expected identifier or string property after `{sep}`"),
                ..Default::default()
            });
        }
    }
}

/// `native` and `abstract` legitimately permit a body-less function
/// (`native` ≈ FFI-bound, `abstract` ≈ subclass-fills-it); every
/// other function must define a body. Diagnostic range points at
/// the function name.
fn check_function_body(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
    out: &mut Vec<Diagnostic>,
) {
    if node.child_by_field_name("body").is_some() {
        return;
    }
    let mods = node.child_by_field_name("modifiers");
    if has_modifier(mods, source, "native") || has_modifier(mods, source, "abstract") {
        return;
    }
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    let name_text = source.get(name.byte_range()).unwrap_or("?");
    out.push(Diagnostic {
        range: byte_range_to_lsp(source, &name.byte_range(), encoding),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("missing-function-body".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: format!(
            "function '{name_text}' must define a body (only `native` and `abstract` functions may omit it)"
        ),
        ..Default::default()
    });
}

/// Grammar lets every `type_attr` carry an optional `init` slot (one
/// rule covers both `static k: int = 0;` — legal — and
/// `a: int = 0;` — illegal). The GreyCat runtime rejects an
/// initializer on a non-static attribute, so emit a diagnostic at
/// parse-shape time before lowering walks the same construct. Range
/// covers the `init` field so the offending `= expr` is highlighted.
fn check_attr_init_modifier(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
    out: &mut Vec<Diagnostic>,
) {
    let Some(init) = node.child_by_field_name("init") else {
        return;
    };
    let mods = node.child_by_field_name("modifiers");
    if has_modifier(mods, source, "static") {
        return;
    }
    let name_text = node
        .child_by_field_name("name")
        .and_then(|n| source.get(n.byte_range()))
        .unwrap_or("?");
    out.push(Diagnostic {
        range: byte_range_to_lsp(source, &init.byte_range(), encoding),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("non-static-attr-initializer".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: format!(
            "attribute `{name_text}` cannot have an initializer — only `static` attributes may"
        ),
        ..Default::default()
    });
}

/// Grammar accepts most block-style statements at module scope
/// (wrapped in `mod_stmt`) so doc snippets parse under the same
/// highlighter as real modules (see `module` / `mod_stmt` rules in
/// grammar.js). A real project module cannot contain a freestanding
/// stmt — emit a hard error covering the whole wrapper.
fn check_top_level_stmt(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
    out: &mut Vec<Diagnostic>,
) {
    // Range covers the whole stmt so the editor's red underline marks
    // the entire offending snippet — useful when the stmt spans
    // multiple lines (`if (cond) { ... }`).
    let mut range = node.byte_range();
    // Trim a trailing explicit `;` from the range so the squiggle
    // doesn't extend past the visible stmt text.
    if source.get(range.clone()).is_some_and(|s| s.ends_with(';')) {
        range.end -= 1;
    }
    out.push(Diagnostic {
        range: byte_range_to_lsp(source, &range, encoding),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("top-level-stmt".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: "statements cannot appear at module scope — wrap in a `fn` body".into(),
        ..Default::default()
    });
}

/// Grammar accepts a `modvar` without a `type_decorator` so mid-edit
/// `var x;` parses cleanly. Real module variables must declare their
/// type explicitly (the `modvar-shape` lint family further constrains
/// it to a node-tag head). Caret just after the binding name (or after
/// `var` if the name itself is missing — `missing-var-name` already
/// flags that case).
fn check_modvar_type(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
    out: &mut Vec<Diagnostic>,
) {
    let has_type = node
        .named_children(&mut node.walk())
        .any(|c| c.kind() == "type_decorator");
    if has_type {
        return;
    }
    // No type AND no name → `missing-var-name` already fires; skip
    // here to avoid a redundant diagnostic on the same `var ;` shape.
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    let after_name = name.end_byte();
    let range = after_name..after_name;
    let name_text = source.get(name.byte_range()).unwrap_or("?");
    out.push(Diagnostic {
        range: byte_range_to_lsp(source, &range, encoding),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("missing-modvar-type".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: format!("module-level `var {name_text}` must declare a type (e.g. `: node<T?>`)"),
        ..Default::default()
    });
}

/// Grammar accepts `var x: int = expr;` at module scope so mid-edit
/// source — and the common copy-paste from JS / a local `var` — parses
/// cleanly instead of opening an `(ERROR)` recovery span. The runtime
/// rejects an initializer on a module-level `var` (module vars are
/// assigned only via runtime mechanisms / explicit functions), so emit
/// a hard error here pointing at the offending `= expr`.
fn check_modvar_initializer(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
    out: &mut Vec<Diagnostic>,
) {
    let Some(init) = node
        .named_children(&mut node.walk())
        .find(|c| c.kind() == "initializer")
    else {
        return;
    };
    let name_text = node
        .child_by_field_name("name")
        .and_then(|n| source.get(n.byte_range()))
        .unwrap_or("?");
    out.push(Diagnostic {
        range: byte_range_to_lsp(source, &init.byte_range(), encoding),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("modvar-initializer".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: format!(
            "module-level `var {name_text}` cannot have an initializer — only local `var` declarations can"
        ),
        ..Default::default()
    });
}

/// `var` parses with optional `name` so mid-edit `var ` doesn't
/// ERROR-recover the next line. Real GreyCat requires a name. Caret
/// just after the `var` keyword.
fn check_var_name(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
    out: &mut Vec<Diagnostic>,
) {
    if node.child_by_field_name("name").is_some() {
        return;
    }
    let after_var = keyword_end(node, source, "var").unwrap_or(node.end_byte());
    let range = after_var..after_var;
    out.push(Diagnostic {
        range: byte_range_to_lsp(source, &range, encoding),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("missing-var-name".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: "expected variable name after `var`".into(),
        ..Default::default()
    });
}

/// Every stmt whose terminator is `choice(_semi, _automatic_semicolon)`
/// can be closed by the external scanner's zero-width auto-semi token
/// (newline / `}` / EOF) so partial source parses cleanly while editing.
/// Auto-semi is a parser convenience, not valid GreyCat — an explicit
/// `;` is required. Walk the node's children: an explicit `;` is a
/// one-byte token whose source text is `";"`; auto-semi is zero-width
/// and has no source text. Caret points at the end of the last real
/// token (where the `;` should have been written).
fn check_explicit_semi(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
    out: &mut Vec<Diagnostic>,
) {
    let mut explicit_semi = false;
    let mut last_real_end = node.start_byte();
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        let br = c.byte_range();
        if br.is_empty() {
            continue;
        }
        last_real_end = br.end;
        if source.get(br) == Some(";") {
            explicit_semi = true;
        }
    }
    if explicit_semi {
        return;
    }
    let range = last_real_end..last_real_end;
    out.push(Diagnostic {
        range: byte_range_to_lsp(source, &range, encoding),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("missing-semicolon".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: "expected `;` at end of statement".into(),
        ..Default::default()
    });
}

/// Reserved keywords the runtime rejects in identifier-binding /
/// identifier-reference positions. Derived empirically against
/// `greycat run`: every word here is rejected as a `fn_param.name`;
/// words used contextually in grammar.js but accepted by the runtime
/// as binding names (`type`, `null`, `this`, `sampling`, `limit`,
/// `skip`, `from`, `to`) are intentionally absent.
const RESERVED_KEYWORDS: &[&str] = &[
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
    "extends",
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
    "typeof",
    "var",
    "while",
];

/// `true` iff `text` matches one of `RESERVED_KEYWORDS`.
fn is_reserved_keyword(text: &str) -> bool {
    RESERVED_KEYWORDS.binary_search(&text).is_ok()
}

/// Reserved keywords the runtime accepts as a *type* name or reference
/// (`native type null {}`, `var x: null`, `extends null`). `null` is the
/// only one: `any` / `type` name types too but aren't reserved words, so
/// they never reach this check. Every other keyword in type position
/// (`var x: return`) parse-rejects.
const TYPE_NAME_KEYWORDS: &[&str] = &["null"];

/// Tree-sitter's `word: $.ident` rule lets keyword text parse as a
/// plain `ident` whenever the current grammar state has no competing
/// keyword token reachable — e.g. `fn ex(return: int)` parses with
/// `return` as a `fn_param.name` ident. We diagnose them here so the
/// problem surfaces in `parse_diagnostics` instead of a confusing
/// downstream failure.
///
/// What's legal depends on the position: a member/field name accepts any
/// keyword; a type name accepts only [`TYPE_NAME_KEYWORDS`]; everything
/// else (value binding, fn-decl name, generics, plain expr) accepts none.
/// `null` / `this` bind without a parse error but are *unreachable* — the
/// body's `null` / `this` mean the literal / receiver, never the binding —
/// so they're flagged here to surface the footgun early.
fn check_ident_keyword(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
    out: &mut Vec<Diagnostic>,
) {
    let Some(text) = source.get(node.byte_range()) else {
        return;
    };
    if !is_reserved_keyword(text) {
        return;
    }
    let Some(parent) = node.parent() else {
        return;
    };
    let parent_kind = parent.kind();
    let mut field: Option<&str> = None;
    let mut cursor = parent.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().id() == node.id() {
                field = cursor.field_name();
                break;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    // Member / field / property / annotation names are pure naming slots
    // — the runtime accepts any reserved keyword (`type T { return: int }`,
    // `t.return`, `E::return`, `@private`).
    if matches!(
        (parent_kind, field),
        ("type_attr", Some("name"))
            | ("type_method", Some("name"))
            | ("enum_field", _)
            | ("object_field", Some("name"))
            | ("member_expr", Some("property"))
            | ("arrow_expr", Some("property"))
            | ("static_expr", Some("property"))
            | ("annotation", _)
    ) {
        return;
    }
    // Type-name positions (decl name + type reference) admit only
    // `null`. Every other position (var / param binding, fn-decl name,
    // generics, plain expr) admits no keyword.
    let is_type_name = matches!(
        (parent_kind, field),
        ("type_decl", Some("name")) | ("enum_decl", Some("name")) | ("type_ident", Some("name"))
    );
    if is_type_name && TYPE_NAME_KEYWORDS.contains(&text) {
        return;
    }
    out.push(Diagnostic {
        range: byte_range_to_lsp(source, &node.byte_range(), encoding),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("keyword-as-ident".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: format!("`{text}` is a reserved keyword and cannot be used as an identifier here"),
        ..Default::default()
    });
}

/// End-byte of the first child of `node` whose source text equals
/// `kw`. `None` if no such child exists.
fn keyword_end(node: tree_sitter::Node<'_>, source: &str, kw: &str) -> Option<usize> {
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if source.get(c.byte_range()) == Some(kw) {
            return Some(c.end_byte());
        }
    }
    None
}

/// `true` iff the `modifiers` node (if any) contains a child token
/// whose source text matches `needle`. Modifier children are unnamed
/// keyword tokens (`private` / `static` / `abstract` / `native`).
fn has_modifier(node: Option<tree_sitter::Node<'_>>, source: &str, needle: &str) -> bool {
    let Some(node) = node else { return false };
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if source.get(c.byte_range()) == Some(needle) {
            return true;
        }
    }
    false
}

/// Find the byte range of the literal separator token `sep` (`::` /
/// `.` / `->`) inside a static_/member_/arrow_expr — the unnamed
/// child whose source text matches. Falls back to the whole node
/// range when the operator can't be isolated.
fn separator_range(
    node: tree_sitter::Node<'_>,
    source: &str,
    sep: &str,
) -> Option<std::ops::Range<usize>> {
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if c.is_named() {
            continue;
        }
        let range = c.byte_range();
        if source.get(range.clone()) == Some(sep) {
            return Some(range);
        }
    }
    None
}

fn walk(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
    out: &mut Vec<Diagnostic>,
) {
    if node.is_missing() {
        out.push(missing_diagnostic(node, source, encoding));
        return;
    }
    if node.is_error() {
        out.push(error_diagnostic(node, source, encoding));
        return;
    }
    if !node.has_error() {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, source, encoding, out);
    }
}

fn error_diagnostic(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
) -> Diagnostic {
    let snippet = source
        .get(node.byte_range())
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let message = if snippet.is_empty() {
        "syntax error".to_string()
    } else {
        // Trim long snippets so we don't blow up tooltip rendering.
        let mut snippet = snippet;
        if snippet.len() > 80 {
            snippet.truncate(77);
            snippet.push_str("...");
        }
        format!("syntax error near `{snippet}`")
    };
    Diagnostic {
        range: node_range(node, source, encoding),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(lsp_types::NumberOrString::String("parse-error".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message,
        ..Default::default()
    }
}

fn missing_diagnostic(
    node: tree_sitter::Node<'_>,
    source: &str,
    encoding: SourceEncoding,
) -> Diagnostic {
    let kind = node.kind();
    let message = if kind.is_empty() {
        "missing token".to_string()
    } else {
        format!("missing `{kind}`")
    };
    Diagnostic {
        range: node_range(node, source, encoding),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(lsp_types::NumberOrString::String("missing-token".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message,
        ..Default::default()
    }
}

fn node_range(node: tree_sitter::Node<'_>, source: &str, encoding: SourceEncoding) -> Range {
    byte_range_to_lsp(source, &node.byte_range(), encoding)
}

// P15.5
/// Pragma resolution diagnostics. Walks a parsed module's
/// [`ModuleDesc`] and emits warnings for:
///
/// * `unresolved-include` — `@include("path")` whose directory does not
///   exist under `project_dir`.
/// * `unresolved-library` — `@library("name", ...)` not found at
///   `<project_dir>/lib/<name>` (and not under `<greycat_home>/lib/std/`
///   for the global `std` fallback).
/// * `duplicate-include` / `duplicate-library` — second-and-later
///   occurrences of the same pragma path / name in this module.
///
/// `text` is the module's source so byte ranges can be converted to LSP
/// `Position`s. `project_dir` is the entrypoint's parent (where `lib/`
/// and `@include` paths anchor). Pure — no I/O beyond what `ctx.is_dir`
/// performs.
pub fn pragma_diagnostics(
    text: &str,
    desc: &ModuleDesc,
    project_dir: &Path,
    ctx: &dyn Context,
    encoding: SourceEncoding,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut seen_includes: FxHashSet<&str> = FxHashSet::default();
    for inc in &desc.includes {
        if !seen_includes.insert(inc.value.as_str()) {
            out.push(make_pragma_diag(
                text,
                &inc.byte_range,
                "duplicate-include",
                DiagnosticSeverity::WARNING,
                format!("duplicate @include('{}')", inc.value),
                encoding,
            ));
            continue;
        }
        // P15.x — runtime rejects absolute paths in @include (it
        // joins the path as `./<value>`), so flag them and don't run
        // the dir-existence check.
        if Path::new(&inc.value).is_absolute() {
            out.push(make_pragma_diag(
                text,
                &inc.byte_range,
                "absolute-include",
                DiagnosticSeverity::WARNING,
                format!(
                    "@include('{}'): absolute paths are not supported (use a project-relative path)",
                    inc.value
                ),
                encoding,
            ));
            continue;
        }
        let dir = project_dir.join(&inc.value);
        if !ctx.is_dir(&dir) {
            out.push(make_pragma_diag(
                text,
                &inc.byte_range,
                "unresolved-include",
                DiagnosticSeverity::WARNING,
                format!("@include('{}'): directory not found", inc.value),
                encoding,
            ));
        }
    }
    let mut seen_libs: FxHashSet<&str> = FxHashSet::default();
    for lib in &desc.libraries {
        if !seen_libs.insert(lib.name.as_str()) {
            out.push(make_pragma_diag(
                text,
                &lib.byte_range,
                "duplicate-library",
                DiagnosticSeverity::WARNING,
                format!("duplicate @library('{}')", lib.name),
                encoding,
            ));
            continue;
        }
        // P17.4 — a library is "resolved" when at least one of the
        // following holds:
        //   1. `<project>/lib/<name>/` is a directory (the canonical
        //      code-library shape).
        //   2. `<project>/webroot/<name>/` is a directory (asset-only
        //      libraries like `explorer` ship as webroot bundles with
        //      no `.gcl` content).
        //   3. `<project>/lib/installed` lists `<name>=...` (the manifest
        //      `greycat install` writes; counts even when the dir hasn't
        //      been materialized in this checkout).
        //   4. The `std` fallback at `<greycat_home>/lib/std/`.
        // Only when *none* of these match do we surface a diagnostic.
        let local = library_dir(project_dir, &lib.name);
        let webroot = project_dir.join("webroot").join(&lib.name);
        let resolved = ctx.is_dir(&local)
            || ctx.is_dir(&webroot)
            || installed_manifest_lists(ctx, project_dir, &lib.name)
            || (lib.name == "std" && ctx.is_dir(&global_std_dir(ctx.greycat_home())));
        if !resolved {
            out.push(make_pragma_diag(
                text,
                &lib.byte_range,
                "unresolved-library",
                DiagnosticSeverity::WARNING,
                format!("@library('{}'): library not found", lib.name),
                encoding,
            ));
        }
    }
    out
}

/// `true` iff `<project>/lib/installed` exists and contains a line
/// starting with `<name>=`. The `installed` manifest is what
/// `greycat install` writes when it materializes a library, and a
/// name being listed there is a strong signal the library is meant
/// to be present even if its directory hasn't been extracted yet.
fn installed_manifest_lists(ctx: &dyn Context, project_dir: &std::path::Path, name: &str) -> bool {
    let manifest = project_dir.join("lib").join("installed");
    let Ok(text) = ctx.read(&manifest) else {
        return false;
    };
    let prefix = format!("{name}=");
    text.lines().any(|line| line.starts_with(&prefix))
}

fn make_pragma_diag(
    text: &str,
    byte_range: &std::ops::Range<usize>,
    code: &str,
    severity: DiagnosticSeverity,
    message: String,
    encoding: SourceEncoding,
) -> Diagnostic {
    Diagnostic {
        range: byte_range_to_lsp(text, byte_range, encoding),
        severity: Some(severity),
        code: Some(NumberOrString::String(code.to_string())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message,
        ..Default::default()
    }
}

fn byte_range_to_lsp(
    text: &str,
    range: &std::ops::Range<usize>,
    encoding: SourceEncoding,
) -> Range {
    crate::conv::byte_range_to_lsp(text, range, encoding)
}

// P32.5
/// File-spanning advisory: this `.gcl` file is not part of any
/// GreyCat project (no `project.gcl` was found walking up from its
/// directory to its workspace folder root). Used by the LSP server
/// alongside parse diagnostics to dim the whole file in the editor
/// and explain why nothing else is being analysed.
///
/// Tagged `UNNECESSARY` so VSCode / other editors render the file
/// greyed out. Severity is `Information` — this is guidance, not
/// an error.
pub fn orphan_module_diagnostic(text: &str, encoding: SourceEncoding) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: byte_to_position(text, text.len(), encoding),
        },
        severity: Some(DiagnosticSeverity::INFORMATION),
        code: Some(NumberOrString::String("orphan-module".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: "This file is not part of any GreyCat project (no `project.gcl` was found from this file's directory up to the workspace folder root). Add a `project.gcl` to enable full analysis.".into(),
        tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
        ..Default::default()
    }
}

// P33.1
/// File-spanning error: the GreyCat `std` library couldn't be found
/// (neither `<project_dir>/lib/std/` nor `<greycat_home>/lib/std/`).
/// The analyzer can't run real type-checking without std, so this
/// dims the project.gcl and explains why every other module is
/// drowning in "unresolved type" diagnostics.
///
/// Severity is `Error` (this is a hard blocker for any meaningful
/// analysis) and the diag is also tagged `UNNECESSARY` so editors
/// dim the whole file as a visual cue.
pub fn missing_std_diagnostic(text: &str, encoding: SourceEncoding) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: byte_to_position(text, text.len(), encoding),
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("missing-std".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: "GreyCat `std` library not found. Looked under `<project>/lib/std/` and `$HOME/.greycat/lib/std/`. Run `greycat install` (or populate the local `lib/std/`) — without std the analyzer can't resolve built-in types.".into(),
        tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
        ..Default::default()
    }
}

/// File-spanning hard error: this `.gcl` file's stem matches the
/// module name of another file already ingested into the project.
/// GreyCat requires module names to be unique within a project — two
/// files named `foo.gcl` in different directories both claim module
/// `foo`, so only the first is analysed and the rest are excluded.
///
/// Severity is `Error` (this is a project-structure invariant
/// violation, not a hint) AND tagged `UNNECESSARY` so editors dim the
/// excluded file as a visual cue. Not in `LINT_RULES` and not
/// suppressible via `// gcl-lint-off` — same gravity as a parse error.
pub fn duplicate_module_name_diagnostic(
    text: &str,
    module_name: &str,
    existing_uri: &Uri,
    encoding: SourceEncoding,
) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: byte_to_position(text, text.len(), encoding),
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("duplicate-module-name".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: format!(
            "module name `{module_name}` is already used by `{}`; rename this file (or move it to a different library) so every module in the project has a unique name",
            existing_uri.as_str()
        ),
        tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
        ..Default::default()
    }
}

// P32.6
/// File-spanning advisory: this `.gcl` file is reachable from
/// multiple projects' `@include` closures. Lists the conflicting
/// project roots so the user can collapse the overlap if it's
/// unintended.
///
/// Tagged `UNNECESSARY` (dim) and `Information` severity.
pub fn multi_project_owner_diagnostic(
    text: &str,
    roots: &[std::path::PathBuf],
    encoding: SourceEncoding,
) -> Diagnostic {
    let mut roots_msg = String::new();
    for (i, r) in roots.iter().enumerate() {
        if i > 0 {
            roots_msg.push_str(", ");
        }
        roots_msg.push_str(&r.display().to_string());
    }
    let message = format!(
        "This file is reachable from multiple GreyCat projects ({roots_msg}). Analysis is anchored to the first owner; if the overlap is unintended, restructure your `@include` paths so only one project includes it."
    );
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: byte_to_position(text, text.len(), encoding),
        },
        severity: Some(DiagnosticSeverity::INFORMATION),
        code: Some(NumberOrString::String("multi-project-owner".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message,
        tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
        ..Default::default()
    }
}

/// Format a single diagnostic into the `path:line:col [severity] message`
/// shape the cli lint subcommand prints. The `_` prefix on `code` is a
/// reminder that the rich struct fields (related info, code, tags) get
/// dropped for cli output.
pub fn print_compact_diagnostic(path: &str, diag: &Diagnostic, color: bool) -> String {
    let severity = match diag.severity {
        Some(DiagnosticSeverity::ERROR) => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::INFORMATION) => "info",
        Some(DiagnosticSeverity::HINT) => "hint",
        _ => "diag",
    };
    // Append the rule / diagnostic code when present so users see
    // which lint or analyzer rule fired — same info the pretty
    // (miette) renderer surfaces; rustc-style `severity[code]`.
    let code = match &diag.code {
        Some(NumberOrString::String(s)) => format!("[{s}]"),
        Some(NumberOrString::Number(n)) => format!("[{n}]"),
        None => String::new(),
    };
    if color {
        // ANSI: bold + per-severity color on `severity[code]`, bold
        // on the path; everything else plain. Matches the visual
        // hierarchy the pretty (miette) renderer uses on the same
        // information so terminal users see the same emphasis in
        // both modes.
        let sev_color = match diag.severity {
            Some(DiagnosticSeverity::ERROR) => "\x1b[31m", // bold red
            Some(DiagnosticSeverity::WARNING) => "\x1b[33m", // bold yellow
            Some(DiagnosticSeverity::INFORMATION) => "\x1b[34m", // bold blue
            Some(DiagnosticSeverity::HINT) => "\x1b[36m",  // bold cyan
            _ => "\x1b[1m",                                // bold
        };
        let reset = "\x1b[0m";
        let grey = "\x1b[90m";
        format!(
            "{grey}{}:{}:{}:{reset} {sev_color}{severity}{code}{reset}: {}",
            path,
            diag.range.start.line + 1,
            diag.range.start.character + 1,
            diag.message,
        )
    } else {
        format!(
            "{}:{}:{}: {}{}: {}",
            path,
            diag.range.start.line + 1,
            diag.range.start.character + 1,
            severity,
            code,
            diag.message,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rustc_hash::FxHashMap;

    use crate::module_desc::parse_module_desc;
    use lsp_types::Uri;

    use super::*;

    fn diags(source: &str) -> Vec<Diagnostic> {
        let tree = greycat_analyzer_syntax::parse(source);
        parse_diagnostics(tree.root_node(), source, SourceEncoding::UTF8)
    }

    // P17.4 added the optional `path → contents` map.
    /// In-memory `Context` for pragma_diagnostics tests. Tracks
    /// known directories plus an optional `path → contents` map for
    /// reading the `lib/installed` manifest.
    struct PragmaCtx {
        dirs: FxHashSet<PathBuf>,
        files: FxHashMap<PathBuf, String>,
        greycat_home: PathBuf,
    }

    impl Context for PragmaCtx {
        fn read(&self, path: &Path) -> std::io::Result<String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::other("not found"))
        }
        fn iter_gcl(&self, _dir: &Path) -> Vec<PathBuf> {
            Vec::new()
        }
        fn is_dir(&self, path: &Path) -> bool {
            self.dirs.contains(path)
        }
        fn greycat_home(&self) -> &Path {
            &self.greycat_home
        }
    }

    fn pragma_diags(source: &str, dirs: &[&str]) -> FxHashMap<String, Diagnostic> {
        let tree = greycat_analyzer_syntax::parse(source);
        let uri = Uri::from_str("file:///proj/project.gcl").unwrap();
        let desc = parse_module_desc(uri, source, tree.root_node());
        let ctx = PragmaCtx {
            dirs: dirs.iter().map(PathBuf::from).collect(),
            files: FxHashMap::default(),
            greycat_home: PathBuf::from("/gcat"),
        };
        let project_dir = Path::new("/proj");
        pragma_diagnostics(source, &desc, project_dir, &ctx, SourceEncoding::UTF8)
            .into_iter()
            .map(|d| {
                let code = match &d.code {
                    Some(NumberOrString::String(s)) => s.clone(),
                    _ => String::new(),
                };
                (code, d)
            })
            .collect()
    }
    use std::str::FromStr;

    #[test]
    fn clean_source_produces_no_diagnostics() {
        assert!(diags("fn main() {}\n").is_empty());
    }

    #[test]
    fn missing_token_surfaces() {
        // Trigger an actual missing-token recovery — ERROR-recovery
        // inserts `}` here.
        let ds = diags("fn main() {\n");
        assert!(
            ds.iter()
                .any(|d| d.message.starts_with("missing `}`") || d.message.starts_with("missing")),
            "expected a missing-token diagnostic, got: {ds:?}"
        );
        assert!(
            ds.iter()
                .all(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
        );
        assert!(
            ds.iter()
                .all(|d| d.source.as_deref() == Some(DIAGNOSTIC_SOURCE))
        );
    }

    #[test]
    fn syntax_error_surfaces_with_snippet() {
        // Open paren never closed — produces an ERROR node.
        let ds = diags("fn main( {\n");
        assert!(!ds.is_empty(), "expected at least one diagnostic");
        assert!(ds.iter().any(|d| d.message.starts_with("syntax error")));
    }

    fn codes(ds: &[Diagnostic]) -> Vec<&str> {
        ds.iter()
            .filter_map(|d| match &d.code {
                Some(NumberOrString::String(s)) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    /// `Foo::` (no property) parses as a well-formed `static_expr`
    /// under the permissive grammar; the semantic requirement that a
    /// property follow `::` is enforced here.
    #[test]
    fn missing_static_property_surfaces() {
        let src = "fn main() { var _ = Foo::; }\n";
        let ds = diags(src);
        let static_diags: Vec<_> = ds
            .iter()
            .filter(|d| {
                matches!(&d.code, Some(NumberOrString::String(s)) if s == "missing-static-property")
            })
            .collect();
        assert_eq!(
            static_diags.len(),
            1,
            "expected exactly one diag, got: {ds:?}"
        );
        let d = static_diags[0];
        assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
        assert!(d.message.contains("after `::`"));
        // Range points at the `::` token.
        let start_byte: usize = src
            .lines()
            .take(d.range.start.line as usize)
            .map(|l| l.len() + 1)
            .sum::<usize>()
            + d.range.start.character as usize;
        assert_eq!(src.get(start_byte..start_byte + 2), Some("::"));
    }

    /// Well-formed `Foo::bar` doesn't trip the missing-property
    /// diagnostic.
    #[test]
    fn well_formed_static_expr_no_diag() {
        let ds = diags("fn main() { var _ = Foo::bar; }\n");
        assert!(
            !codes(&ds).contains(&"missing-static-property"),
            "expected no missing-static-property, got: {ds:?}"
        );
    }

    /// `s.` (no property) parses cleanly under the permissive
    /// grammar; `missing-member-property` fires here.
    #[test]
    fn missing_member_property_surfaces() {
        let src = "fn f(s: String) {\n    s.\n    if (true) {}\n}\n";
        let ds = diags(src);
        let cs = codes(&ds);
        assert!(cs.contains(&"missing-member-property"), "got: {ds:?}");
        // Following `if` should parse, not ERROR-cascade.
        assert!(!cs.contains(&"parse-error"), "got: {ds:?}");
    }

    /// `s->` (no property) parses cleanly under the permissive
    /// grammar; `missing-arrow-property` fires here.
    #[test]
    fn missing_arrow_property_surfaces() {
        let src = "fn f(s: node) {\n    s->\n    if (true) {}\n}\n";
        let ds = diags(src);
        let cs = codes(&ds);
        assert!(cs.contains(&"missing-arrow-property"), "got: {ds:?}");
        assert!(!cs.contains(&"parse-error"), "got: {ds:?}");
    }

    /// Well-formed `s.length` and `s->name` don't trip the
    /// missing-property diagnostics.
    #[test]
    fn well_formed_member_arrow_no_diag() {
        let ds = diags("fn f(s: String, n: node) {\n    s.length;\n    n->name;\n}\n");
        let cs = codes(&ds);
        assert!(!cs.contains(&"missing-member-property"), "got: {ds:?}");
        assert!(!cs.contains(&"missing-arrow-property"), "got: {ds:?}");
    }

    /// Non-native, non-abstract function declared without a body
    /// surfaces a `missing-function-body` error. Catches both
    /// top-level `fn_decl` and `type_method` shapes.
    #[test]
    fn missing_function_body_surfaces() {
        let src = "type T {\n    static fn not_valid();\n}\nfn top_level();\n";
        let ds = diags(src);
        let body_diags: Vec<_> = ds
            .iter()
            .filter(|d| {
                matches!(&d.code, Some(NumberOrString::String(s)) if s == "missing-function-body")
            })
            .collect();
        assert_eq!(body_diags.len(), 2, "expected two diags, got: {ds:?}");
        for d in &body_diags {
            assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
        }
        assert!(body_diags[0].message.contains("not_valid"));
        assert!(body_diags[1].message.contains("top_level"));
    }

    /// `native` and `abstract` functions may legitimately omit the
    /// body — no diagnostic for either; functions with a real body
    /// stay silent too.
    #[test]
    fn body_or_exempt_modifier_no_diag() {
        let src = "type T {\n    abstract fn a();\n    native fn b();\n    fn c() {}\n}\nnative fn top();\nfn ok() {}\n";
        let ds = diags(src);
        assert!(
            !codes(&ds).contains(&"missing-function-body"),
            "expected no missing-function-body, got: {ds:?}"
        );
    }

    /// Non-static instance attribute with an initializer is rejected
    /// by the runtime; the grammar lets it parse (one `type_attr` rule
    /// covers both static and non-static) so we flag it at parse-shape
    /// time. `static k: int = 0;` is legal and must stay silent.
    #[test]
    fn non_static_attr_initializer_surfaces() {
        let src = "type T {\n    a: int = 0;\n    static k: int = 1;\n}\n";
        let ds = diags(src);
        let hits: Vec<_> = ds
            .iter()
            .filter(|d| {
                matches!(&d.code, Some(NumberOrString::String(s)) if s == "non-static-attr-initializer")
            })
            .collect();
        assert_eq!(hits.len(), 1, "expected one diag for `a`, got: {ds:?}");
        assert!(hits[0].message.contains('`'), "got: {}", hits[0].message);
        assert!(
            hits[0].message.contains("`a`"),
            "should name `a`, got: {}",
            hits[0].message
        );
    }

    /// Grammar accepts `var x: int = expr;` at module scope (so
    /// mid-edit / JS-habit copy-paste doesn't cascade-recover), but the
    /// runtime rejects an initializer on a module-level `var`. Flag the
    /// `= expr` span.
    #[test]
    fn modvar_initializer_surfaces() {
        let src = "var x: int = 42;\n";
        let ds = diags(src);
        let hits: Vec<_> = ds
            .iter()
            .filter(
                |d| matches!(&d.code, Some(NumberOrString::String(s)) if s == "modvar-initializer"),
            )
            .collect();
        assert_eq!(hits.len(), 1, "expected one diag, got: {ds:?}");
        assert_eq!(hits[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(
            hits[0].message.contains("var x"),
            "should name `var x`, got: {}",
            hits[0].message
        );
        // Range covers the `= 42` slice.
        let start_byte: usize = src
            .lines()
            .take(hits[0].range.start.line as usize)
            .map(|l| l.len() + 1)
            .sum::<usize>()
            + hits[0].range.start.character as usize;
        let end_byte: usize = src
            .lines()
            .take(hits[0].range.end.line as usize)
            .map(|l| l.len() + 1)
            .sum::<usize>()
            + hits[0].range.end.character as usize;
        assert_eq!(src.get(start_byte..end_byte), Some("= 42"));
    }

    /// Canonical module-var (no initializer) stays silent.
    #[test]
    fn modvar_without_initializer_no_diag() {
        let ds = diags("var x: int;\n");
        assert!(
            !codes(&ds).contains(&"modvar-initializer"),
            "expected no modvar-initializer, got: {ds:?}"
        );
    }

    /// Grammar accepts `var x;` (no `: T`) at module scope so mid-edit
    /// source doesn't ERROR-recover. Semantic rule: modvars must
    /// declare a type. Caret just after the binding name.
    #[test]
    fn modvar_missing_type_surfaces() {
        let src = "var x;\n";
        let ds = diags(src);
        let hits: Vec<_> = ds
            .iter()
            .filter(|d| matches!(&d.code, Some(NumberOrString::String(s)) if s == "missing-modvar-type"))
            .collect();
        assert_eq!(hits.len(), 1, "expected one diag, got: {ds:?}");
        assert_eq!(hits[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(
            hits[0].message.contains("var x"),
            "should name `var x`, got: {}",
            hits[0].message
        );
    }

    /// `var x = 1;` (no type but with initializer) — both
    /// `missing-modvar-type` AND `modvar-initializer` fire; the user
    /// has two distinct issues, surface both.
    #[test]
    fn modvar_no_type_with_initializer_surfaces_both() {
        let ds = diags("var x = 1;\n");
        let cs = codes(&ds);
        assert!(cs.contains(&"missing-modvar-type"), "got: {ds:?}");
        assert!(cs.contains(&"modvar-initializer"), "got: {ds:?}");
    }

    /// Grammar accepts `foo();` at module scope (wrapped in
    /// `mod_stmt`) so doc snippets work for syntax highlighting. As a
    /// project module, that's invalid — `top-level-stmt` fires
    /// covering the whole wrapper.
    #[test]
    fn top_level_stmt_surfaces_on_expr() {
        let src = "foo();\n";
        let ds = diags(src);
        let hits: Vec<_> = ds
            .iter()
            .filter(|d| matches!(&d.code, Some(NumberOrString::String(s)) if s == "top-level-stmt"))
            .collect();
        assert_eq!(hits.len(), 1, "expected one diag, got: {ds:?}");
        assert_eq!(hits[0].severity, Some(DiagnosticSeverity::ERROR));
        // Range covers `foo()` (the trailing `;` is trimmed).
        let start_byte: usize = src
            .lines()
            .take(hits[0].range.start.line as usize)
            .map(|l| l.len() + 1)
            .sum::<usize>()
            + hits[0].range.start.character as usize;
        let end_byte: usize = src
            .lines()
            .take(hits[0].range.end.line as usize)
            .map(|l| l.len() + 1)
            .sum::<usize>()
            + hits[0].range.end.character as usize;
        assert_eq!(src.get(start_byte..end_byte), Some("foo()"));
    }

    /// `top-level-stmt` covers other stmt kinds too — `if`, `while`,
    /// `return`, etc. all wrap in `mod_stmt` at module scope.
    #[test]
    fn top_level_stmt_surfaces_on_if() {
        let ds = diags("if (cond) { foo(); }\n");
        let cs = codes(&ds);
        assert!(cs.contains(&"top-level-stmt"), "got: {ds:?}");
    }

    /// `foo()` without trailing `;` at top level — `top-level-stmt`
    /// fires but `missing-semicolon` does NOT (the wrapper short-
    /// circuits recursion into the inner expr_stmt, where the semi
    /// check lives — that suppression is the point).
    #[test]
    fn top_level_stmt_suppresses_inner_diagnostics() {
        let ds = diags("foo()\n");
        let cs = codes(&ds);
        assert!(cs.contains(&"top-level-stmt"), "got: {ds:?}");
        assert!(
            !cs.contains(&"missing-semicolon"),
            "missing-semicolon should be suppressed on a top-level stmt, got: {ds:?}"
        );
    }

    /// `expr_stmt` inside a `fn` body stays unaffected — `mod_stmt`
    /// only fires at module scope (it's a direct child of `module`).
    #[test]
    fn nested_expr_stmt_unaffected() {
        let ds = diags("fn f() { foo(); }\n");
        let cs = codes(&ds);
        assert!(!cs.contains(&"top-level-stmt"), "got: {ds:?}");
    }

    /// A top-level `Foo::;` produces ONE clean `top-level-stmt`
    /// diagnostic — the `missing-static-property` recursion is
    /// suppressed so the user sees the headline error without
    /// per-fragment noise on top.
    #[test]
    fn top_level_stmt_suppresses_inner_shape_checks() {
        let ds = diags("Foo::;\n");
        let cs = codes(&ds);
        assert!(cs.contains(&"top-level-stmt"), "got: {ds:?}");
        assert!(
            !cs.contains(&"missing-static-property"),
            "inner shape checks should be suppressed under mod_stmt, got: {ds:?}"
        );
    }

    /// `var;` (no name) → `missing-var-name` fires (same code as
    /// var_decl). `missing-modvar-type` is suppressed because the
    /// name diagnostic already covers the user's editing state.
    #[test]
    fn modvar_missing_name_surfaces_missing_var_name() {
        let ds = diags("var;\n");
        let cs = codes(&ds);
        assert!(cs.contains(&"missing-var-name"), "got: {ds:?}");
        assert!(
            !cs.contains(&"missing-modvar-type"),
            "missing-modvar-type should be suppressed when name is also missing, got: {ds:?}"
        );
    }

    /// Both well-formed shapes — instance attr without init, static
    /// attr with init — stay silent.
    #[test]
    fn attr_initializer_well_formed_no_diag() {
        let src = "type T {\n    a: int;\n    b: String?;\n    static k: int = 1;\n}\n";
        let ds = diags(src);
        assert!(
            !codes(&ds).contains(&"non-static-attr-initializer"),
            "expected no non-static-attr-initializer, got: {ds:?}"
        );
    }

    /// `var` with no name AND no explicit `;` fires both diagnostics.
    #[test]
    fn var_decl_missing_name_and_semi() {
        let src = "fn f() {\n    var\n}\n";
        let ds = diags(src);
        let cs = codes(&ds);
        assert!(cs.contains(&"missing-var-name"), "got: {ds:?}");
        assert!(cs.contains(&"missing-semicolon"), "got: {ds:?}");
    }

    /// `var x` (name present, auto-semi at `}`) fires only the
    /// missing-`;` diagnostic.
    #[test]
    fn var_decl_missing_semi_only() {
        let src = "fn f() {\n    var x\n}\n";
        let ds = diags(src);
        let cs = codes(&ds);
        assert!(!cs.contains(&"missing-var-name"), "got: {ds:?}");
        assert!(cs.contains(&"missing-semicolon"), "got: {ds:?}");
    }

    /// Well-formed `var x = 1;` fires neither diagnostic.
    #[test]
    fn var_decl_well_formed() {
        let ds = diags("fn f() {\n    var x = 1;\n}\n");
        let cs = codes(&ds);
        assert!(!cs.contains(&"missing-var-name"), "got: {ds:?}");
        assert!(!cs.contains(&"missing-semicolon"), "got: {ds:?}");
    }

    /// `var x: int;` (no initializer, explicit `;`) is well-formed.
    #[test]
    fn var_decl_type_only() {
        let ds = diags("fn f() {\n    var x: int;\n}\n");
        let cs = codes(&ds);
        assert!(!cs.contains(&"missing-var-name"), "got: {ds:?}");
        assert!(!cs.contains(&"missing-semicolon"), "got: {ds:?}");
    }

    /// Expression statement terminated by ASI (newline before `}`)
    /// fires `missing-semicolon`. The arrow_expr `f->n` parses cleanly
    /// (with property `n`); only the missing `;` is flagged.
    #[test]
    fn expr_stmt_asi_termination_flagged() {
        let src = "fn f() {\n    bar()\n}\n";
        let ds = diags(src);
        assert!(codes(&ds).contains(&"missing-semicolon"), "got: {ds:?}");
    }

    /// `return value` terminated by ASI fires `missing-semicolon`.
    #[test]
    fn return_stmt_asi_termination_flagged() {
        let src = "fn f(): int {\n    return 1\n}\n";
        let ds = diags(src);
        assert!(codes(&ds).contains(&"missing-semicolon"), "got: {ds:?}");
    }

    /// `break` / `continue` / `throw` / `breakpoint` ASI-terminated
    /// all flagged.
    #[test]
    fn stmt_keywords_asi_termination_flagged() {
        for src in [
            "fn f() {\n    while (true) {\n        break\n    }\n}\n",
            "fn f() {\n    while (true) {\n        continue\n    }\n}\n",
            "fn f() {\n    throw 0\n}\n",
            "fn f() {\n    breakpoint\n}\n",
        ] {
            let ds = diags(src);
            assert!(
                codes(&ds).contains(&"missing-semicolon"),
                "missing-semicolon expected for: {src:?}, got: {ds:?}"
            );
        }
    }

    /// Well-formed statements with explicit `;` don't fire.
    #[test]
    fn explicit_semi_no_diag() {
        let src = "fn f(): int {\n    bar();\n    throw 0;\n    return 1;\n}\n";
        let ds = diags(src);
        assert!(!codes(&ds).contains(&"missing-semicolon"), "got: {ds:?}");
    }

    /// Reserved keyword used as a `fn_param.name` — the runtime parse-rejects
    /// `fn ex(return: int) {}`, so we flag it here.
    #[test]
    fn keyword_as_fn_param_name_surfaces() {
        let ds = diags("fn ex(return: int) { println(1); }\n");
        let hits: Vec<_> = ds
            .iter()
            .filter(
                |d| matches!(&d.code, Some(NumberOrString::String(s)) if s == "keyword-as-ident"),
            )
            .collect();
        assert_eq!(hits.len(), 1, "expected exactly one diag, got: {ds:?}");
        assert!(
            hits[0].message.contains("`return`"),
            "got: {}",
            hits[0].message
        );
        assert_eq!(hits[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    /// Reserved keyword used as a `var_decl.name` — also runtime parse-reject.
    #[test]
    fn keyword_as_var_decl_name_surfaces() {
        let ds = diags("fn f() {\n    var return = 1;\n}\n");
        assert!(codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    /// Reserved keyword in plain-expression value position
    /// (`1 + return`) — parse-rejected by the runtime.
    #[test]
    fn keyword_as_plain_expr_ident_surfaces() {
        let ds = diags("fn f() {\n    var x = 1 + return;\n}\n");
        assert!(codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    /// Reserved keyword as a `type_ident` reference (`x: return`) —
    /// parse-rejected by the runtime.
    #[test]
    fn keyword_as_type_ident_reference_surfaces() {
        let ds = diags("fn f() {\n    var x: return;\n}\n");
        assert!(codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    /// Declaration-only positions the runtime currently *accepts* but
    /// renders unreachable (the call site `return()` parse-rejects).
    /// Flagged pre-emptively — the runtime fix is imminent.
    #[test]
    fn keyword_as_fn_decl_name_surfaces() {
        let ds = diags("fn return() {}\n");
        assert!(codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    #[test]
    fn keyword_as_type_decl_name_surfaces() {
        let ds = diags("type return { x: int; }\n");
        assert!(codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    #[test]
    fn keyword_as_enum_decl_name_surfaces() {
        let ds = diags("enum return { A }\n");
        assert!(codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    #[test]
    fn keyword_as_type_params_entry_surfaces() {
        let ds = diags("type T<return> {}\n");
        assert!(codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    /// Positions the runtime supports — must stay silent.
    #[test]
    fn keyword_as_type_attr_name_no_diag() {
        let ds = diags("type T { return: int; }\n");
        assert!(!codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    #[test]
    fn keyword_as_type_method_name_no_diag() {
        let ds = diags("type T {\n    fn return(): int { return 1; }\n}\n");
        assert!(!codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    #[test]
    fn keyword_as_enum_field_name_no_diag() {
        let ds = diags("enum E { return }\n");
        assert!(!codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    #[test]
    fn keyword_as_object_slot_key_no_diag() {
        let ds = diags("fn f() {\n    var t = T { return: 1 };\n}\n");
        assert!(!codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    #[test]
    fn keyword_as_member_property_no_diag() {
        let ds = diags("fn f(t: T) {\n    println(t.return);\n}\n");
        assert!(!codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    #[test]
    fn keyword_as_arrow_property_no_diag() {
        let ds = diags("fn f(n: node<T>) {\n    println(n->return);\n}\n");
        assert!(!codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    #[test]
    fn keyword_as_static_property_no_diag() {
        let ds = diags("fn f() {\n    println(E::return);\n}\n");
        assert!(!codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    /// Annotations: `@private` is a real annotation name even though
    /// `private` is also a keyword — don't false-positive.
    #[test]
    fn keyword_as_annotation_name_no_diag() {
        let ds = diags("@private\nfn f() {}\n");
        assert!(!codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    /// `null` is a real builtin type, so it's reachable as a type name /
    /// reference — `native type null {}` (stdlib) and `var x: null` must
    /// stay silent even though `null` is a reserved keyword.
    #[test]
    fn null_as_type_name_no_diag() {
        for src in [
            "native type null {}\n",
            "fn f() {\n    var x: null;\n}\n",
            "type T extends null {}\n",
        ] {
            let ds = diags(src);
            assert!(
                !codes(&ds).contains(&"keyword-as-ident"),
                "`null` is a type; should be silent in {src:?}, got: {ds:?}"
            );
        }
    }

    /// `null` / `this` bind without a parse error but are unreachable in
    /// the body (`null` is the literal, `this` the receiver), so they're
    /// flagged as binding names. The user's footgun: `fn foo(null: String)
    /// { return null.size(); }` resolves `null` to the type, not the param.
    #[test]
    fn unreachable_soft_keyword_bindings_surface() {
        for src in [
            "fn foo(null: String): int { return null.size(); }\n",
            "fn foo(this: int) { println(1); }\n",
            "fn f() {\n    var null = 1;\n}\n",
            "fn f() {\n    var this = 1;\n}\n",
        ] {
            assert!(
                codes(&diags(src)).contains(&"keyword-as-ident"),
                "expected keyword-as-ident in {src:?}, got: {:?}",
                diags(src)
            );
        }
    }

    /// Non-keyword identifiers never trip the check.
    #[test]
    fn non_keyword_ident_no_diag() {
        let ds = diags("fn ex(answer: int) {\n    var x = answer + 1;\n    println(x);\n}\n");
        assert!(!codes(&ds).contains(&"keyword-as-ident"), "got: {ds:?}");
    }

    /// Non-reserved contextual words (`type` is not a reserved keyword;
    /// `sampling` / `limit` / `skip` / `from` / `to` are for-in clause
    /// words) parse as ordinary idents and bind fine — don't false-positive.
    /// (`null` / `this` ARE reserved and unreachable as bindings — see
    /// `unreachable_soft_keyword_bindings_surface`.)
    #[test]
    fn contextual_keywords_as_param_names_no_diag() {
        for kw in ["type", "sampling", "limit", "skip", "from", "to"] {
            let src = format!("fn ex({kw}: int) {{ println(1); }}\n");
            let ds = diags(&src);
            assert!(
                !codes(&ds).contains(&"keyword-as-ident"),
                "`{kw}` should be allowed as a param name; got: {ds:?}"
            );
        }
    }

    /// The deprecated for-in `sampling` / `limit` / `skip` clause keywords are
    /// NOT reserved words — they must parse as ordinary identifiers in every
    /// other position (method names, member access, call arguments, locals).
    /// Regression for the `other(f.skip())` parse-ERROR: those clauses used to
    /// be global keyword tokens, which leaked into expression-completing states.
    #[test]
    fn for_in_clause_keywords_as_names_no_parse_error() {
        let srcs = [
            // `skip` as a method, called bare AND as a call argument.
            "type Foo {\n    fn skip(): int { return 42; }\n}\n\
             fn main(f: Foo) {\n    var x = f.skip();\n    other(f.skip());\n}\n",
            // `skip` / `limit` / `sampling` as a local, member, and method name.
            "fn main(c: any) {\n    var skip = 1;\n    var limit = c.limit;\n    \
             var s = c.sampling();\n    println(skip);\n}\n",
        ];
        for src in srcs {
            let ds = diags(src);
            let cs = codes(&ds);
            assert!(
                !cs.contains(&"parse-error"),
                "unexpected parse-error in: {src:?}"
            );
            assert!(
                !cs.contains(&"keyword-as-ident"),
                "unexpected keyword-as-ident in: {src:?}"
            );
        }
    }

    // P15.5
    /// `@include` to a missing directory surfaces as
    /// `unresolved-include` with the pragma's range.
    #[test]
    fn pragma_diagnostics_unresolved_include() {
        let src = "@include(\"does_not_exist\");\n";
        let map = pragma_diags(src, &[]);
        assert!(
            map.contains_key("unresolved-include"),
            "expected unresolved-include, got: {map:?}"
        );
        assert_eq!(
            map["unresolved-include"].severity,
            Some(DiagnosticSeverity::WARNING)
        );
    }

    // P15.x
    /// Runtime rejects absolute paths in `@include`; the
    /// analyzer mirrors that with an `absolute-include` warning.
    #[test]
    fn pragma_diagnostics_absolute_include() {
        let src = "@include(\"/tmp/anything\");\n";
        let map = pragma_diags(src, &["/tmp/anything"]);
        assert!(
            map.contains_key("absolute-include"),
            "expected absolute-include warning, got: {map:?}"
        );
        // The dir-not-found check should be skipped — emitting both
        // would be noisy.
        assert!(
            !map.contains_key("unresolved-include"),
            "absolute path should not also fire unresolved-include: {map:?}"
        );
    }

    // P15.5
    /// Duplicate `@include` of the same dir warns on the
    /// second occurrence.
    #[test]
    fn pragma_diagnostics_duplicate_include() {
        let src = "@include(\"a\");\n@include(\"a\");\n";
        let map = pragma_diags(src, &["/proj/a"]);
        assert!(
            map.contains_key("duplicate-include"),
            "expected duplicate-include, got: {map:?}"
        );
        // Second `@include` is line 1 (0-indexed).
        assert_eq!(map["duplicate-include"].range.start.line, 1);
    }

    // P15.5
    /// `@library` whose name has no local `lib/` dir and isn't
    /// the global `std` fallback surfaces as `unresolved-library`.
    #[test]
    fn pragma_diagnostics_unresolved_library() {
        let src = "@library(\"missing\", \"1.0\");\n";
        let map = pragma_diags(src, &[]);
        assert!(
            map.contains_key("unresolved-library"),
            "expected unresolved-library, got: {map:?}"
        );
    }

    // P17.4
    /// `@library("explorer", ...)` resolves to a webroot
    /// asset library: `<project>/webroot/<name>/` exists even though
    /// `<project>/lib/<name>/` does not.
    #[test]
    fn pragma_diagnostics_library_resolves_from_webroot() {
        let src = "@library(\"explorer\", \"1.0\");\n";
        let map = pragma_diags(src, &["/proj/webroot/explorer"]);
        assert!(
            !map.contains_key("unresolved-library"),
            "expected webroot fallback to resolve, got: {map:?}"
        );
    }

    // P17.4
    /// `@library("foo", ...)` resolves when `lib/installed`
    /// lists the name. Useful for asset-only libs that haven't yet
    /// extracted their dir.
    #[test]
    fn pragma_diagnostics_library_resolves_from_installed_manifest() {
        let src = "@library(\"foo\", \"1.0\");\n";
        let tree = greycat_analyzer_syntax::parse(src);
        let uri = Uri::from_str("file:///proj/project.gcl").unwrap();
        let desc = parse_module_desc(uri, src, tree.root_node());
        let mut files = FxHashMap::default();
        files.insert(
            PathBuf::from("/proj/lib/installed"),
            "std=8.0.269-dev\nfoo=1.0\n".into(),
        );
        let ctx = PragmaCtx {
            dirs: Default::default(),
            files,
            greycat_home: PathBuf::from("/gcat"),
        };
        let project_dir = Path::new("/proj");
        let out = pragma_diagnostics(src, &desc, project_dir, &ctx, SourceEncoding::UTF8);
        assert!(
            !out.iter().any(
                |d| matches!(&d.code, Some(NumberOrString::String(s)) if s == "unresolved-library")
            ),
            "expected `lib/installed` manifest to resolve `foo`, got: {out:?}"
        );
    }

    // P15.5
    /// `@library("std", "...")` resolves under the global
    /// `<greycat_home>/lib/std/` fallback when no local `lib/std`
    /// exists; no diagnostic emitted.
    #[test]
    fn pragma_diagnostics_std_resolves_under_greycat_home() {
        let src = "@library(\"std\", \"1.0\");\n";
        let map = pragma_diags(src, &["/gcat/lib/std"]);
        assert!(
            !map.contains_key("unresolved-library"),
            "std should resolve via greycat_home, got: {map:?}"
        );
    }

    // P15.5
    /// Duplicate `@library` of the same name warns on the
    /// second occurrence.
    #[test]
    fn pragma_diagnostics_duplicate_library() {
        let src = "@library(\"std\", \"1.0\");\n@library(\"std\", \"1.0\");\n";
        let map = pragma_diags(src, &["/gcat/lib/std"]);
        assert!(
            map.contains_key("duplicate-library"),
            "expected duplicate-library, got: {map:?}"
        );
        assert_eq!(map["duplicate-library"].range.start.line, 1);
    }

    #[test]
    fn format_cli_one_indexed() {
        let diag = Diagnostic {
            range: Range {
                start: Position {
                    line: 4,
                    character: 7,
                },
                end: Position {
                    line: 4,
                    character: 9,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            message: "boom".into(),
            ..Default::default()
        };
        assert_eq!(
            print_compact_diagnostic("a.gcl", &diag, false),
            "a.gcl:5:8: error: boom"
        );
    }

    /// When a diagnostic carries a `code`, the cli line includes
    /// `severity[code]:` so users see which rule / analyzer check
    /// fired — matches the miette pretty renderer's output.
    #[test]
    fn format_cli_includes_code() {
        let diag = Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String("missing-function-body".into())),
            message: "boom".into(),
            ..Default::default()
        };
        assert_eq!(
            print_compact_diagnostic("a.gcl", &diag, false),
            "a.gcl:1:1: error[missing-function-body]: boom"
        );
    }

    /// With `color=true` the path:line:col is grey (bright black)
    /// and the `severity[code]` is bold-colored per severity (red /
    /// yellow / blue / cyan). ANSI reset closes both runs. The
    /// message stays plain.
    #[test]
    fn format_cli_color_ansi() {
        let diag = Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String("unknown-member".into())),
            message: "boom".into(),
            ..Default::default()
        };
        let out = print_compact_diagnostic("a.gcl", &diag, true);
        assert_eq!(
            out,
            "\x1b[90ma.gcl:1:1:\x1b[0m \x1b[31merror[unknown-member]\x1b[0m: boom"
        );
    }
}
