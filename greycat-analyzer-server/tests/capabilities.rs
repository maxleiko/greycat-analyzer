//! Integration tests for the LSP capability handlers.
//!
//! Bypasses the JSON-RPC plumbing and calls each handler in
//! `greycat_analyzer_server::capabilities` with curated source snippets.
//! That gives us solid coverage of the actual logic (HIR walking,
//! resolver / analyzer interaction, position math) without the overhead
//! of spinning up the full server. A separate end-to-end protocol smoke
//! test in [`lsp_smoke.rs`](./lsp_smoke.rs) covers the JSON-RPC half.

use greycat_analyzer_server::capabilities;
use greycat_analyzer_syntax::parse;
use lsp_types::*;

fn pos(line: u32, character: u32) -> Position {
    Position { line, character }
}

fn root<'a>(
    src: &'a str,
    tree_holder: &'a mut Option<greycat_analyzer_syntax::tree_sitter::Tree>,
) -> greycat_analyzer_syntax::tree_sitter::Node<'a> {
    *tree_holder = Some(parse(src));
    tree_holder.as_ref().unwrap().root_node()
}

// =============================================================================
// hover
// =============================================================================

#[test]
fn hover_on_param_returns_inferred_type() {
    let src = "fn id(name: String): String { return name; }\n";
    let mut t = None;
    let r = root(src, &mut t);
    // Position the cursor on the `name` use inside the body. Find it.
    let offset = src.rfind("name").unwrap();
    let line = src[..offset].matches('\n').count() as u32;
    let col = (offset - src[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    let h = capabilities::hover(src, "project", r, pos(line, col)).expect("hover present");
    let HoverContents::Markup(content) = h.contents else {
        panic!("expected markup contents")
    };
    assert!(
        content.value.contains("String"),
        "expected String in hover, got {}",
        content.value
    );
}

#[test]
fn hover_off_named_node_returns_none() {
    let src = "fn f() {}\n";
    let mut t = None;
    let r = root(src, &mut t);
    // Far past EOF — no node at offset.
    assert!(capabilities::hover(src, "project", r, pos(99, 99)).is_none());
}

// =============================================================================
// signature_help
// =============================================================================

#[test]
fn signature_help_renders_function_signature() {
    let src = r#"
fn add(a: int, b: int): int { return a + b; }
fn main(): int { return add(1, 2); }
"#;
    let mut t = None;
    let r = root(src, &mut t);
    // Cursor inside the call_expr `add(1, 2)`.
    let offset = src.find("add(1").unwrap() + "add(".len();
    let line = src[..offset].matches('\n').count() as u32;
    let col = (offset - src[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    let sh =
        capabilities::signature_help(src, "project", r, pos(line, col)).expect("signature help");
    assert_eq!(sh.signatures.len(), 1);
    let sig = &sh.signatures[0];
    assert!(sig.label.starts_with("fn add("));
    assert!(sig.label.contains(": int"));
    let params = sig.parameters.as_ref().expect("params");
    assert_eq!(params.len(), 2);
}

// =============================================================================
// goto_definition
// =============================================================================

#[test]
fn goto_definition_lands_on_decl_name() {
    let src = "fn helper(): int { return 1; }\nfn main(): int { return helper(); }\n";
    let mut t = None;
    let r = root(src, &mut t);
    // Cursor on the `helper` use inside main's body.
    let use_offset = src.rfind("helper").unwrap();
    let line = src[..use_offset].matches('\n').count() as u32;
    let col = (use_offset - src[..use_offset].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    let uri = "file:///mod.gcl".parse::<Uri>().unwrap();
    let resp = capabilities::goto_definition(src, "project", r, &uri, pos(line, col))
        .expect("goto produced a location");
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location")
    };
    // The defining `helper` is on line 0.
    assert_eq!(loc.range.start.line, 0);
    assert_eq!(loc.uri, uri);
}

// =============================================================================
// document_symbols
// =============================================================================

#[test]
fn document_symbols_includes_decl_and_method_children() {
    let src = r#"
type Point {
    x: int;
    y: int;
    fn dist(): int { return 0; }
}

fn outside(): int { return 0; }
"#;
    let mut t = None;
    let r = root(src, &mut t);
    let syms = capabilities::document_symbols(src, "project", r);
    let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Point"));
    assert!(names.contains(&"outside"));

    let point = syms.iter().find(|s| s.name == "Point").unwrap();
    let children = point.children.as_ref().expect("Point has children");
    let child_names: Vec<&str> = children.iter().map(|s| s.name.as_str()).collect();
    assert!(child_names.contains(&"x"));
    assert!(child_names.contains(&"y"));
    assert!(child_names.contains(&"dist"));
}

// =============================================================================
// references + rename
// =============================================================================

#[test]
fn references_finds_every_same_name_occurrence() {
    let src = "fn id(x: int): int { return x; }\nfn main(): int { return id(42); }\n";
    let mut t = None;
    let r = root(src, &mut t);
    let uri = "file:///mod.gcl".parse::<Uri>().unwrap();
    // Cursor on the `id` declaration on line 0.
    let locs = capabilities::references(src, "project", r, &uri, pos(0, 3));
    // Three idents named `id`: the decl, `id` again? actually just two: the
    // decl and the use site in main. (The param `x` is a different name.)
    assert!(
        locs.len() >= 2,
        "expected at least 2 references, got {}",
        locs.len()
    );
}

#[test]
fn rename_emits_one_textedit_per_occurrence() {
    let src = "fn id(x: int): int { return x; }\nfn main(): int { return id(42); }\n";
    let mut t = None;
    let r = root(src, &mut t);
    let uri = "file:///mod.gcl".parse::<Uri>().unwrap();
    let edit =
        capabilities::rename(src, r, &uri, pos(0, 3), "named").expect("rename produced an edit");
    #[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice
    let changes = edit.changes.expect("changes map");
    let edits = changes.get(&uri).expect("uri in changes");
    assert!(edits.len() >= 2);
    assert!(edits.iter().all(|e| e.new_text == "named"));
}

#[test]
fn prepare_rename_advertises_current_name() {
    let src = "fn helper(): int { return 1; }\n";
    let mut t = None;
    let r = root(src, &mut t);
    let resp = capabilities::prepare_rename(src, r, pos(0, 5)).expect("renamable");
    if let PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. } = resp {
        assert_eq!(placeholder, "helper");
    } else {
        panic!("expected RangeWithPlaceholder");
    }
}

// =============================================================================
// folding / selection / highlights
// =============================================================================

#[test]
fn folding_ranges_cover_blocks_and_bodies() {
    let src = r#"
fn long(): int {
    var x: int = 0;
    return x;
}
"#;
    let mut t = None;
    let r = root(src, &mut t);
    let folds = capabilities::folding_ranges(src, r);
    assert!(!folds.is_empty(), "expected at least one fold range");
    assert!(folds.iter().all(|f| f.end_line > f.start_line));
}

#[test]
fn document_highlights_match_same_text_idents() {
    let src = "fn f(x: int): int { return x + x; }\n";
    let mut t = None;
    let r = root(src, &mut t);
    // Cursor on the parameter `x`.
    let hs = capabilities::document_highlights(src, r, pos(0, 5));
    // Three `x` idents: the param decl + two uses.
    assert_eq!(hs.len(), 3);
}

#[test]
fn selection_ranges_form_an_ancestor_chain() {
    let src = "fn f(): int { return 1 + 2; }\n";
    let mut t = None;
    let r = root(src, &mut t);
    let offset = src.find("1").unwrap();
    let line = src[..offset].matches('\n').count() as u32;
    let col = (offset - src[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    let ranges = capabilities::selection_ranges(src, r, &[pos(line, col)]);
    assert_eq!(ranges.len(), 1);
    // Walk the .parent chain — should have several levels (number → binary
    // → return → block → fn_decl → module).
    let mut depth = 1;
    let mut current = ranges[0].parent.as_ref();
    while let Some(p) = current {
        depth += 1;
        current = p.parent.as_ref();
    }
    assert!(
        depth >= 4,
        "expected ancestor chain depth >= 4, got {depth}"
    );
}

// =============================================================================
// inlay hints
// =============================================================================

#[test]
fn inlay_hints_annotate_typeless_locals() {
    let src = "fn f(): int { var n = 42; return n; }\n";
    let mut t = None;
    let r = root(src, &mut t);
    let range = lsp_types::Range {
        start: pos(0, 0),
        end: pos(99, 0),
    };
    let hints = capabilities::inlay_hints(src, "project", r, &range);
    assert_eq!(hints.len(), 1, "expected 1 inlay hint, got {}", hints.len());
    let hint = &hints[0];
    let InlayHintLabel::String(s) = &hint.label else {
        panic!("expected string label")
    };
    assert!(s.contains("int"), "expected int in hint, got `{s}`");
}

/// Anchors the architectural rule: LSP inlay hints MUST run through
/// `inlay_hints_with_project` so the cross-module fixup passes
/// (P15.7 / P16.3 / P16.4) flow into the inferred-type label.
///
/// Reproduces the bug we hit in the IDE on `var s = x.s.size();`
/// where the LSP rendered `s: any` because the single-file
/// `inlay_hints` shim re-ran `analyzer::analyze` without the
/// project pipeline. After P16.3 + P16.4 landed, `dump-types` reported
/// `core::int` correctly; this test prevents regressing the LSP path
/// next to it.
#[test]
fn inlay_hints_with_project_use_cross_module_call_return_types() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    use std::str::FromStr;

    // Two-module project: `Foo` lives in lib.gcl with a `size(): int`
    // method; `main.gcl` calls `x.s.size()` and assigns to a typeless
    // local. The LSP inlay hint must emit `: int`, not `: any`.
    let lib_uri = Uri::from_str("file:///lib.gcl").unwrap();
    let main_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        lib_uri,
        "type Foo {\n    s: String;\n    fn size(): int { return 0; }\n}\n",
        "p",
        false,
    );
    mgr.add_simple(
        main_uri.clone(),
        "fn read(x: Foo) {\n    var n = x.size();\n}\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let user_cell = mgr.get(&main_uri).expect("user doc");
    let user_doc = user_cell.borrow();
    let module = pa.module(&main_uri).expect("user module cached");

    let range = lsp_types::Range {
        start: pos(0, 0),
        end: pos(99, 0),
    };
    let hints = capabilities::inlay_hints_with_project(module, &user_doc.text, &range);
    assert_eq!(
        hints.len(),
        1,
        "expected 1 inlay hint for `var n = x.size();`, got {}: {hints:?}",
        hints.len()
    );
    let InlayHintLabel::String(s) = &hints[0].label else {
        panic!("expected string label, got {:?}", hints[0].label)
    };
    assert_eq!(
        s, ": int",
        "method-call return type should propagate to the inlay hint, got `{s}`"
    );
}

// =============================================================================
// formatting
// =============================================================================

#[test]
fn formatting_returns_no_edits_on_already_formatted_input() {
    // A small known-formatted snippet should produce zero edits.
    let src = greycat_analyzer_fmt::format("fn main() {}\n");
    let mut t = None;
    let r = root(&src, &mut t);
    let edits = capabilities::formatting(&src, r).expect("Some(edits)");
    assert!(edits.is_empty(), "expected no edits, got {edits:?}");
}

#[test]
fn formatting_returns_a_single_full_replacement_on_drift() {
    let src = "fn   sloppy   (   ){}\n";
    let mut t = None;
    let r = root(src, &mut t);
    let edits = capabilities::formatting(src, r).expect("Some(edits)");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].range.start, pos(0, 0));
}

// =============================================================================
// semantic tokens
// =============================================================================

#[test]
fn semantic_tokens_emits_typed_idents_and_literals() {
    let src = "fn add(a: int, b: int): int { return a + b; }\n";
    let mut t = None;
    let r = root(src, &mut t);
    let tokens = capabilities::semantic_tokens(src, "project", r);
    // Each SemanticToken is a 5-tuple (delta_line, delta_start, length, type, mod);
    // `data` is a flat list.
    assert!(
        !tokens.data.is_empty(),
        "expected at least one semantic token"
    );
    // PARAMETER token type is index 5 in our SEMANTIC_TOKEN_TYPES table.
    let param_type_idx = 5u32;
    assert!(
        tokens.data.iter().any(|t| t.token_type == param_type_idx),
        "expected at least one PARAMETER-typed token"
    );
}
