//! Integration tests for the LSP capability handlers.
//!
//! Bypasses the JSON-RPC plumbing and calls each handler in
//! `greycat_analyzer_server::capabilities` with curated source snippets.
//! That gives us solid coverage of the actual logic (HIR walking,
//! resolver / analyzer interaction, position math) without the overhead
//! of spinning up the full server. A separate end-to-end protocol smoke
//! test in [`lsp_smoke.rs`](./lsp_smoke.rs) covers the JSON-RPC half.

use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_server::capabilities;
use greycat_analyzer_syntax::parse;
use lsp_types::*;

mod support;
use support::{TestProject, position_of};

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
    let test_project = TestProject::single_file(src);
    // Cursor on the `name` *use* inside the body (`name;` is unique).
    let cursor = position_of(src, "name;");
    let h = test_project.hover(cursor).expect("hover present");
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
    let test_project = TestProject::single_file(src);
    // Far past EOF — no node at offset.
    assert!(test_project.hover(pos(99, 99)).is_none());
}

#[test]
fn hover_shows_runtime_erased_type_for_generic_result() {
    // `wrap<T>` constructs & returns `Box<T>`, which the GreyCat runtime
    // erases to `Box<any?>`. Hover shows BOTH: the analyzer's
    // materialized `Box<int>` and a `runtime:` note with the erased
    // shape, so the user isn't misled by the optimistic monomorphization.
    let src = "\
type Box<T> { item: T; }
fn wrap<T>(x: T): Box<T> { return Box<T> { item: x }; }
fn main() {
    var b = wrap(42);
    println(b);
}
";
    let test_project = TestProject::single_file(src);
    let cursor = position_of(src, "b)"); // the `b` in `println(b)`
    let h = test_project.hover(cursor).expect("hover present");
    let HoverContents::Markup(content) = h.contents else {
        panic!("expected markup contents")
    };
    let v = &content.value;
    assert!(
        v.contains("Box<int>"),
        "expected materialized `Box<int>` in hover, got {v}"
    );
    assert!(
        v.contains("runtime:") && v.contains("Box<any?>"),
        "expected runtime-erased note `Box<any?>` in hover, got {v}"
    );
}

#[test]
fn hover_on_object_expr_field_shows_attr_signature_doc_and_provenance() {
    let src = "\
type Reader {
    /// The documentation
    private path: String;
}

fn main() {
    var r = Reader { path: \"\" };
}
";
    let test_project = TestProject::single_file(src);
    // Cursor on the `path` field name inside the object expression.
    let cursor = position_of(src, "path: \"\"");
    let h = test_project
        .hover(cursor)
        .expect("hover present on object-expr field");
    let HoverContents::Markup(content) = h.contents else {
        panic!("expected markup contents")
    };
    let v = &content.value;
    assert!(
        v.contains("private path: String"),
        "expected `private path: String` in hover, got {v}"
    );
    assert!(
        v.contains("The documentation"),
        "expected attr doc in hover, got {v}"
    );
    assert!(
        v.contains("*defined in `main::Reader`*"),
        "expected `module::Type` provenance footer in hover, got {v}"
    );
}

#[test]
fn hover_on_type_attr_declaration_shows_same_shape_as_object_field() {
    let src = "\
type Reader {
    /// The documentation
    private path: String;
}

fn main() {
    var r = Reader { path: \"\" };
}
";
    let test_project = TestProject::single_file(src);
    // Cursor on the `path` in the type body — the declaring site.
    let cursor = position_of(src, "path: String");
    let h = test_project
        .hover(cursor)
        .expect("hover present on type-attr decl site");
    let HoverContents::Markup(content) = h.contents else {
        panic!("expected markup contents")
    };
    let v = &content.value;
    assert!(
        v.contains("private path: String"),
        "expected `private path: String` in hover, got {v}"
    );
    assert!(
        v.contains("The documentation"),
        "expected attr doc in hover, got {v}"
    );
    assert!(
        v.contains("*defined in `main::Reader`*"),
        "expected `module::Type` provenance footer in hover, got {v}"
    );
}

#[test]
fn hover_on_object_expr_field_resolves_inherited_attr() {
    let src = "\
type Base {
    /// inherited doc
    label: String;
}
type Derived extends Base {}

fn main() {
    var d = Derived { label: \"hi\" };
}
";
    let test_project = TestProject::single_file(src);
    let cursor = position_of(src, "label: \"hi\"");
    let h = test_project
        .hover(cursor)
        .expect("hover present on inherited object-expr field");
    let HoverContents::Markup(content) = h.contents else {
        panic!("expected markup contents")
    };
    let v = &content.value;
    assert!(
        v.contains("label: String"),
        "expected `label: String` in hover, got {v}"
    );
    assert!(
        v.contains("inherited doc"),
        "expected inherited attr doc, got {v}"
    );
    assert!(
        v.contains("*defined in `main::Base`*"),
        "provenance must name the *declaring* type (Base), not the constructed one (Derived), got {v}"
    );
}

#[test]
fn hover_on_fn_decl_shows_annotations_above_signature() {
    let src = "\
@expose(\"renamed\")
@deref(\"resolve\")
fn helper(): int { return 1; }
fn main(): int { return helper(); }
";
    let test_project = TestProject::single_file(src);
    // Cursor on the *use* of `helper` inside main.
    let cursor = position_of(src, "helper();");
    let h = test_project.hover(cursor).expect("hover present on fn use");
    let HoverContents::Markup(content) = h.contents else {
        panic!("expected markup contents")
    };
    let v = &content.value;
    assert!(
        v.contains("@expose(\"renamed\")"),
        "expected `@expose(\"renamed\")` in hover, got {v}"
    );
    assert!(
        v.contains("@deref(\"resolve\")"),
        "expected `@deref(\"resolve\")` in hover, got {v}"
    );
    assert!(
        v.contains("fn helper(): int"),
        "expected signature in hover, got {v}"
    );
    // Annotations must precede the signature within the same code
    // block (no blank line between them).
    let expose_at = v.find("@expose").expect("annotation present");
    let fn_at = v.find("fn helper").expect("signature present");
    assert!(
        expose_at < fn_at,
        "annotations must render before the signature, got {v}"
    );
}

#[test]
fn hover_on_type_decl_shows_annotations_above_signature() {
    let src = "\
@iterable
@deref(\"resolve\")
type Iter {}
fn main(): Iter { return Iter {}; }
";
    let test_project = TestProject::single_file(src);
    let cursor = position_of(src, "Iter { return");
    let h = test_project.hover(cursor).expect("hover on type use");
    let HoverContents::Markup(content) = h.contents else {
        panic!("expected markup contents")
    };
    let v = &content.value;
    assert!(
        v.contains("@iterable"),
        "expected `@iterable` in hover, got {v}"
    );
    assert!(
        v.contains("@deref(\"resolve\")"),
        "expected `@deref(\"resolve\")` in hover, got {v}"
    );
    let ann_at = v.find("@iterable").unwrap();
    let ty_at = v.find("type Iter").unwrap();
    assert!(
        ann_at < ty_at,
        "annotations must render before `type` signature, got {v}"
    );
}

#[test]
fn hover_on_enum_decl_shows_annotations_above_signature() {
    let src = "\
@iterable
enum Color { red; green; blue; }
fn main(): Color { return Color::red; }
";
    let test_project = TestProject::single_file(src);
    let cursor = position_of(src, "Color::red");
    let h = test_project.hover(cursor).expect("hover on enum use");
    let HoverContents::Markup(content) = h.contents else {
        panic!("expected markup contents")
    };
    let v = &content.value;
    assert!(
        v.contains("@iterable"),
        "expected `@iterable` in enum hover, got {v}"
    );
    let ann_at = v.find("@iterable").unwrap();
    let kw_at = v.find("enum Color").unwrap();
    assert!(
        ann_at < kw_at,
        "annotations must render before `enum` signature, got {v}"
    );
}

#[test]
fn hover_on_var_decl_shows_annotations_above_signature() {
    let src = "\
@volatile
var counter: int = 0;
fn main(): int { return counter; }
";
    let test_project = TestProject::single_file(src);
    let cursor = position_of(src, "counter;");
    let h = test_project.hover(cursor).expect("hover on var use");
    let HoverContents::Markup(content) = h.contents else {
        panic!("expected markup contents")
    };
    let v = &content.value;
    assert!(
        v.contains("@volatile"),
        "expected `@volatile` in var hover, got {v}"
    );
    let ann_at = v.find("@volatile").unwrap();
    let kw_at = v.find("var counter").unwrap();
    assert!(
        ann_at < kw_at,
        "annotations must render before `var` signature, got {v}"
    );
}

#[test]
fn completion_after_dot_on_blank_line_in_chain() {
    // Typing `.` on a blank line inside a multi-line member chain
    // should fire completion against the receiver expression above
    // (here `t.other().other()`), not give up because the new `.`
    // sits past trailing whitespace from the previous line.
    let src = "\
type T {
    name: String;
    fn other(): T;
}

fn f(t: T) {
    var result = t
        .other()
        .other()
        .
        .name
        .startsWith(\"xx\");
}
";
    let needle = "        .\n        .name";
    let off = src.find(needle).unwrap() + 9; // after "        ."
    let line = src[..off].matches('\n').count() as u32;
    let col = (off - src[..off].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    let cursor = Position {
        line,
        character: col,
    };
    let test_project = TestProject::single_file(src);
    let list = test_project
        .completion(cursor)
        .expect("completion list at chain `.<blank>`");
    let labels: Vec<&str> = list.items.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"name") && labels.contains(&"other"),
        "expected `name` and `other` on chain `.` completion, got {labels:?}"
    );
}

#[test]
fn completion_after_arrow_on_blank_line_in_chain() {
    // `->` mirror of `completion_after_dot_on_blank_line_in_chain` —
    // typing `->` on a new line after a multi-line chain that lands
    // on a `@deref`-annotated type should list the deref target's
    // members.
    let src = "\
type Inner {
    name: String;
}

@deref(\"get\")
type Wrapper {
    fn get(): Inner { return Inner { name: \"\" }; }
    fn next(): Wrapper { return this; }
}

fn f(w: Wrapper) {
    var result = w
        .next()

        ->
        ;
}
";
    let needle = "        ->\n";
    let off = src.find(needle).unwrap() + 10; // after "        ->"
    let line = src[..off].matches('\n').count() as u32;
    let col = (off - src[..off].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    let cursor = Position {
        line,
        character: col,
    };
    let test_project = TestProject::single_file(src);
    let list = test_project
        .completion(cursor)
        .expect("completion list at chain `->`");
    let labels: Vec<&str> = list.items.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"name"),
        "expected `name` (Inner attribute) on chain `->` completion, got {labels:?}"
    );
}

#[test]
fn completion_method_label_details_is_compact_signature() {
    // Methods should show as `fnName(args): Ret` in the popup row —
    // not buried under `static private @expose("...") fn name(...)`.
    // The `label` is the bare name; `label_details.detail` carries
    // the compact `(args): Ret` form right next to it (rust-analyzer
    // style). The full signature stays in `detail` for the side
    // panel. Annotations belong in hover, not the popup row.
    let src = "\
type Bag {
    @expose(\"pushItem\")
    @deref(\"resolve\")
    fn push(value: int): int { return value; }
}

fn main() {
    var b = Bag {};
    b.;
}
";
    let test_project = TestProject::single_file(src);
    let cursor = position_of(src, "b.;");
    let cursor = Position {
        line: cursor.line,
        character: cursor.character + 2, // skip "b."
    };
    let list = test_project
        .completion(cursor)
        .expect("completion list at b.|");
    let push_item = list
        .items
        .iter()
        .find(|c| c.label == "push")
        .unwrap_or_else(|| {
            panic!(
                "expected `push` in completion items, got {:?}",
                list.items.iter().map(|c| &c.label).collect::<Vec<_>>()
            )
        });
    assert_eq!(push_item.label, "push", "label must be the bare name");
    let ld = push_item
        .label_details
        .as_ref()
        .expect("method completion items must carry label_details");
    let compact = ld
        .detail
        .as_deref()
        .expect("label_details.detail must carry the compact signature");
    assert_eq!(
        compact, "(value: int): int",
        "label_details.detail should be the compact `(args): Ret` form"
    );
    assert!(
        !compact.contains('@'),
        "compact label must not include annotations, got {compact:?}"
    );
    // `detail` mirrors the compact form for clients (like Zed) that
    // render `detail` in the popup row and ignore
    // `label_details.detail`. Hover provides the full source-form
    // signature, so the duplication doesn't lose information.
    let detail = push_item
        .detail
        .as_deref()
        .expect("method completion items must carry `detail`");
    assert_eq!(
        detail, "(value: int): int",
        "`detail` should mirror the compact form so Zed's popup row reads cleanly"
    );
    assert!(
        !detail.contains('@'),
        "completion `detail` must not include annotations, got {detail:?}"
    );
    assert!(
        !detail.contains("fn "),
        "completion `detail` must not include the `fn` keyword or modifiers, got {detail:?}"
    );
}

#[test]
fn hover_on_unknown_object_field_skips_object_field_path() {
    let src = "\
type Reader {
    path: String;
}
fn main() {
    var r = Reader { nope: \"\" };
}
";
    let test_project = TestProject::single_file(src);
    let cursor = position_of(src, "nope:");
    // Unknown field — no binding recorded, the object-field hover
    // branch must not synthesise a fake signature for `nope`. The
    // generic ancestor-expression hover may still fire (and that's
    // fine), but the object-field markdown shape — modifier prefix +
    // provenance footer pointing at a non-existent attr — must not
    // appear.
    let h = test_project.hover(cursor);
    if let Some(Hover {
        contents: HoverContents::Markup(MarkupContent { value, .. }),
        ..
    }) = h
    {
        assert!(
            !value.contains("nope:"),
            "unknown field must not produce object-field hover markdown, got {value}"
        );
        assert!(
            !value.contains("*defined in"),
            "unknown field must not produce provenance footer, got {value}"
        );
    }
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
    let sh = capabilities::signature_help(src, "project", r, pos(line, col), SourceEncoding::UTF8)
        .expect("signature help");
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
    let resp = capabilities::goto_definition(
        src,
        "project",
        r,
        &uri,
        pos(line, col),
        SourceEncoding::UTF8,
    )
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
    let syms = capabilities::document_symbols(src, "project", r, SourceEncoding::UTF8);
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

/// Regression: VSCode's LSP client throws `name must not be falsy`
/// when any `DocumentSymbol.name` is the empty string. Tree-sitter's
/// recovery synthesizes empty-range name nodes mid-edit (for example
/// while the user is typing a new method declaration —
/// `static<CURSOR>` inside a `type` body), and the HIR lowering
/// faithfully interns that as `Ident { text: "" }`. The
/// `document_symbols` capability must filter those out at every
/// nesting level (top-level decl, type attrs, type methods);
/// otherwise one in-flight keystroke poisons the whole symbol
/// outline.
#[test]
fn document_symbols_skips_empty_name_recovered_from_partial_edit() {
    // Reproduces the exact shape the user reported: a complete static
    // method, then a partial `static` keyword with no name yet.
    let src = "type Foo {\n    static fn whatever() {};\n\n    static\n}\n";
    let mut t = None;
    let r = root(src, &mut t);
    let syms = capabilities::document_symbols(src, "project", r, SourceEncoding::UTF8);

    // Walk every level of the symbol tree and assert no name is empty.
    fn walk(symbols: &[DocumentSymbol]) {
        for s in symbols {
            assert!(
                !s.name.is_empty(),
                "DocumentSymbol with empty name leaked through (kind={:?})",
                s.kind,
            );
            if let Some(children) = &s.children {
                walk(children);
            }
        }
    }
    walk(&syms);
}

// =============================================================================
// references + rename
// =============================================================================

#[test]
fn references_finds_every_same_name_occurrence() {
    let src = "fn id(x: int): int { return x; }\nfn main(): int { return id(42); }\n";
    let test_project = TestProject::single_file(src);
    // Cursor on the `id` declaration on line 0.
    let locs = test_project.references(pos(0, 3));
    // Two idents named `id`: the decl and the use site in main.
    assert!(
        locs.len() >= 2,
        "expected at least 2 references, got {}",
        locs.len()
    );
}

#[test]
fn rename_emits_one_textedit_per_occurrence() {
    let src = "fn id(x: int): int { return x; }\nfn main(): int { return id(42); }\n";
    let test_project = TestProject::single_file(src);
    let edit = test_project
        .rename(pos(0, 3), "named")
        .expect("rename produced an edit");
    #[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice
    let changes = edit.changes.expect("changes map");
    let edits = changes.get(&test_project.uri).expect("uri in changes");
    assert!(edits.len() >= 2);
    assert!(edits.iter().all(|e| e.new_text == "named"));
}

#[test]
fn prepare_rename_advertises_current_name() {
    let src = "fn helper(): int { return 1; }\n";
    let mut t = None;
    let r = root(src, &mut t);
    let resp =
        capabilities::prepare_rename(src, r, pos(0, 5), SourceEncoding::UTF8).expect("renamable");
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
    let folds = capabilities::folding_ranges(src, r, SourceEncoding::UTF8);
    assert!(!folds.is_empty(), "expected at least one fold range");
    assert!(folds.iter().all(|f| f.end_line > f.start_line));
}

#[test]
fn document_highlights_match_same_text_idents() {
    let src = "fn f(x: int): int { return x + x; }\n";
    let mut t = None;
    let r = root(src, &mut t);
    // Cursor on the parameter `x`.
    let hs = capabilities::document_highlights(src, r, pos(0, 5), SourceEncoding::UTF8);
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
    let ranges = capabilities::selection_ranges(src, r, &[pos(line, col)], SourceEncoding::UTF8);
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
    let test_project = TestProject::single_file(src);
    let range = lsp_types::Range {
        start: pos(0, 0),
        end: pos(99, 0),
    };
    let hints = test_project.inlay_hints(&range);
    assert_eq!(hints.len(), 1, "expected 1 inlay hint, got {}", hints.len());
    let hint = &hints[0];
    let InlayHintLabel::String(s) = &hint.label else {
        panic!("expected string label")
    };
    assert!(s.contains("int"), "expected int in hint, got `{s}`");
}

/// `_` in a for-in head is the runtime's no-binding slot — there's
/// no variable to annotate, so no type hint should appear. The
/// surrounding non-`_` binder still gets its hint, anchored at its
/// own ident end, not at `_`'s.
#[test]
fn inlay_hints_skip_underscore_for_in_params() {
    let src = "fn f(arr: Array<int>) {\n    for (k, _ in arr) {}\n}\n";
    let test_project = TestProject::single_file(src);
    let range = lsp_types::Range {
        start: pos(0, 0),
        end: pos(99, 0),
    };
    let hints = test_project.inlay_hints(&range);
    assert_eq!(
        hints.len(),
        1,
        "expected 1 hint (on `k`, not `_`), got {}: {hints:?}",
        hints.len()
    );
    // The hint should anchor right after `k` on line 1: `for (k`
    // is offset 9 in the line, so the anchor is column 10.
    assert_eq!(hints[0].position.line, 1);
    assert_eq!(hints[0].position.character, 10);
}

/// Both for-in binders named `_` — the runtime treats both as
/// no-binding slots, so no hints should appear at all.
#[test]
fn inlay_hints_skip_double_underscore_for_in_params() {
    let src = "fn f(arr: Array<int>) {\n    for (_, _ in arr) {}\n}\n";
    let test_project = TestProject::single_file(src);
    let range = lsp_types::Range {
        start: pos(0, 0),
        end: pos(99, 0),
    };
    let hints = test_project.inlay_hints(&range);
    assert!(
        hints.is_empty(),
        "expected no hints, got {}: {hints:?}",
        hints.len()
    );
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
    let hints = capabilities::inlay_hints_with_project(
        module,
        &pa,
        &user_doc.text,
        &range,
        SourceEncoding::UTF8,
    );
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

/// Inlay hints render qualified type names when the bare name is
/// ambiguous across modules. `var f = b::Foo {};` against a project
/// where three modules each declare a `Foo` must surface `: b::Foo`,
/// not the misleading `: Foo`.
#[test]
fn inlay_hints_qualify_ambiguous_type_names() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    use std::str::FromStr;

    let a_uri = Uri::from_str("file:///a.gcl").unwrap();
    let b_uri = Uri::from_str("file:///b.gcl").unwrap();
    let c_uri = Uri::from_str("file:///c.gcl").unwrap();
    let main_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(a_uri, "type Foo { fn a() {} }\n", "p", false);
    mgr.add_simple(b_uri, "type Foo { fn b() {} }\n", "p", false);
    mgr.add_simple(c_uri, "type Foo { fn c() {} }\n", "p", false);
    mgr.add_simple(
        main_uri.clone(),
        "fn main() {\n    var f = b::Foo {};\n}\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let main_cell = mgr.get(&main_uri).expect("main doc");
    let main_doc = main_cell.borrow();
    let module = pa.module(&main_uri).expect("main module cached");

    let range = lsp_types::Range {
        start: pos(0, 0),
        end: pos(99, 0),
    };
    let hints = capabilities::inlay_hints_with_project(
        module,
        &pa,
        &main_doc.text,
        &range,
        SourceEncoding::UTF8,
    );
    assert_eq!(
        hints.len(),
        1,
        "expected 1 inlay hint for `var f`, got {}: {hints:?}",
        hints.len()
    );
    let InlayHintLabel::String(s) = &hints[0].label else {
        panic!("expected string label, got {:?}", hints[0].label);
    };
    assert_eq!(
        s, ": b::Foo",
        "ambiguous `Foo` should render with module qualifier, got `{s}`"
    );
}

/// Anchors enum-variant access typing across every valid form.
/// Variants can be declared with either an ident name (`a`) or a
/// quoted-string name (`"str field"`); access goes through either
/// `Static` (`Foo::a`, `Foo::"str field"`) or `QualifiedStatic`
/// (`project::Foo::a`, `project::Foo::"str field"`). Each form must
/// type as `Foo` so passing it to a `_: Foo` parameter doesn't trip
/// the call-arg validator's "value of type `any`" false-positive.
#[test]
fn enum_variant_access_types_as_enum_in_every_form() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    use std::str::FromStr;

    let uri = Uri::from_str("file:///project.gcl").unwrap();
    // 6 static-access call sites: 4 ident-named forms + 2
    // string-named forms (in-module + qualified). The string-named
    // variant `"str field"` exercises both `enum_field` lowering for
    // string names and the qualified-chain matching against
    // multi-word names.
    let src = "enum Foo { a, b, \"str field\" }\n\
        fn test() {\n\
        \x20   take(Foo::a);\n\
        \x20   take(Foo::\"a\");\n\
        \x20   take(project::Foo::a);\n\
        \x20   take(project::Foo::\"a\");\n\
        \x20   take(Foo::\"str field\");\n\
        \x20   take(project::Foo::\"str field\");\n\
        }\n\
        fn take(_: Foo) {}\n";
    let mut mgr = SourceManager::new();
    mgr.add_simple(uri.clone(), src, "project", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    let module = pa.module(&uri).expect("module cached");

    use greycat_analyzer_hir::hir::Expr;
    let mut static_count = 0usize;
    for (idx, expr) in module.hir.exprs.iter() {
        let is_static = matches!(expr, Expr::Static(_) | Expr::QualifiedStatic { .. });
        if !is_static {
            continue;
        }
        static_count += 1;
        let ty = module
            .analysis
            .expr_types
            .get(&idx)
            .copied()
            .unwrap_or_else(|| panic!("static expr at idx {idx:?} has no expr_types entry"));
        let display = pa.display_type(ty).to_string();
        assert_eq!(
            display, "Foo",
            "static expression should type as `Foo` (enum), got `{display}`"
        );
    }
    assert_eq!(
        static_count, 6,
        "expected 6 static expressions in the fixture (4 ident + 2 string), got {static_count}"
    );

    // The call-arg validator must accept every call site — no
    // semantic diagnostics should fire on this module.
    let diag_msgs: Vec<_> = module
        .analysis
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect();
    assert!(
        diag_msgs.is_empty(),
        "expected no semantic diagnostics on enum-variant calls, got {diag_msgs:?}"
    );
}

/// Anchors completion for enum variants. Three scenarios:
///
/// 1. `Foo::|` → list every variant. Ident-shaped names (`alpha`)
///    appear bare; non-ident names (`"Africa/Abidjan"`) come with
///    their quotes so accepting the completion produces valid syntax.
/// 2. `Foo::"Afr|"` → cursor inside a quoted property — list every
///    variant, filter by prefix, emit bare text (the opening quote
///    is already in the buffer, so re-quoting would double-up).
/// 3. `Foo::"a|"` → string-mode prefix filter; only variants whose
///    HIR name starts with `a` show up (`alpha`, `America/...`).
///
/// Mirrors the real-world `core::TimeZone` shape (600+ IANA-spelled
/// variants in stdlib). Reproduces the user's reports that (a) no
/// completion fired after `Foo::`, and (b) typing inside the quotes
/// failed to surface variants whose names start with the typed
/// prefix.
#[test]
fn completion_after_enum_double_colon_lists_variants() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    use std::str::FromStr;

    fn complete_items(
        mgr: &SourceManager,
        uri: &Uri,
        pa: &ProjectAnalysis,
        cursor_byte: usize,
    ) -> Vec<CompletionItem> {
        let cell = mgr.get(uri).unwrap();
        let doc = cell.borrow();
        let line = doc.text[..cursor_byte].matches('\n').count() as u32;
        let col = (cursor_byte
            - doc.text[..cursor_byte]
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0)) as u32;
        let list = capabilities::completion_with_project(
            &doc.text,
            doc.root_node(),
            pos(line, col),
            uri,
            pa,
            None,
            SourceEncoding::UTF8,
        )
        .unwrap_or_else(|| panic!("no completion at byte {cursor_byte}"));
        list.items
    }

    fn complete_at(
        mgr: &SourceManager,
        uri: &Uri,
        pa: &ProjectAnalysis,
        cursor_byte: usize,
    ) -> Vec<String> {
        complete_items(mgr, uri, pa, cursor_byte)
            .into_iter()
            .map(|c| c.label)
            .collect()
    }

    /// Apply a completion item's `text_edit` to `text`, returning the
    /// resulting buffer. Anchors the architectural rule: every
    /// completion item must use `text_edit` (not `insert_text`) so
    /// asking for completion mid-ident replaces the surrounding word
    /// instead of doubling it.
    fn apply_edit(text: &str, item: &CompletionItem) -> String {
        let edit = match item
            .text_edit
            .as_ref()
            .unwrap_or_else(|| panic!("completion item `{}` is missing text_edit", item.label))
        {
            CompletionTextEdit::Edit(e) => e,
            CompletionTextEdit::InsertAndReplace(_) => {
                panic!("test only handles CompletionTextEdit::Edit")
            }
        };
        let start = position_to_byte(text, edit.range.start);
        let end = position_to_byte(text, edit.range.end);
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..start]);
        out.push_str(&edit.new_text);
        out.push_str(&text[end..]);
        out
    }

    fn position_to_byte(text: &str, p: Position) -> usize {
        let mut line = 0u32;
        let mut byte = 0usize;
        for ch in text.chars() {
            if line == p.line
                && (byte - text[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32
                    == p.character
            {
                return byte;
            }
            byte += ch.len_utf8();
            if ch == '\n' {
                if line == p.line {
                    return byte - 1;
                }
                line += 1;
            }
        }
        byte
    }

    let uri = Uri::from_str("file:///project.gcl").unwrap();
    // Mirrors `core::TimeZone`'s shape: an enum with IANA-style
    // string variants alongside ident-shaped ones. A real-world
    // `core::TimeZone` ships 600+ such names (`"Africa/Abidjan"`,
    // `"America/New_York"`, …); the per-variant completion path
    // must stay allocation-light.
    let src = concat!(
        "enum Foo { alpha, beta, \"Africa/Abidjan\", \"America/New_York\", \"str field\" }\n",
        "fn test() {\n",
        "    var a = Foo::\n",
        "    var b = Foo::\"Afr\";\n",
        "    var c = Foo::\"a\";\n",
        "}\n",
    );
    let mut mgr = SourceManager::new();
    mgr.add_simple(uri.clone(), src, "project", false);
    let pa = ProjectAnalysis::analyze(&mgr);

    // 1. `Foo::|` — every variant. Ident-shaped names appear bare;
    //    non-ident names (slash, space) are wrapped in quotes so
    //    accepting the completion produces valid syntax.
    let labels = complete_at(
        &mgr,
        &uri,
        &pa,
        src.find("Foo::\n").unwrap() + "Foo::".len(),
    );
    assert!(
        labels.iter().any(|l| l == "alpha")
            && labels.iter().any(|l| l == "beta")
            && labels.iter().any(|l| l == "\"Africa/Abidjan\"")
            && labels.iter().any(|l| l == "\"America/New_York\"")
            && labels.iter().any(|l| l == "\"str field\""),
        "expected every variant (with string-named ones quoted) at `Foo::`, got {labels:?}"
    );

    // 2. `Foo::"Afr|"` — string-mode, prefix filter on `Afr`. The
    //    opening quote is in the buffer so the inserted text is
    //    bare (no leading `"`).
    let cursor = src.find("\"Afr\"").unwrap() + "\"Afr".len();
    let labels = complete_at(&mgr, &uri, &pa, cursor);
    assert!(
        labels.iter().any(|l| l == "Africa/Abidjan"),
        "expected `Africa/Abidjan` (bare) in `Foo::\"Afr|\"` completion, got {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l == "\"Africa/Abidjan\""),
        "string-mode completion should not re-quote variants (opening `\"` is already typed), got {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l == "alpha"),
        "string-mode prefix filter should drop non-matching variants, got {labels:?}"
    );

    // 3. `Foo::"a|"` — string-mode, prefix `a`. Matches `alpha`
    //    (case-insensitive) and `"America/New_York"` (which surfaces
    //    bare since we're inside the quotes).
    let cursor = src.find("\"a\"").unwrap() + "\"a".len();
    let labels = complete_at(&mgr, &uri, &pa, cursor);
    assert!(
        labels.iter().any(|l| l == "alpha") && labels.iter().any(|l| l == "America/New_York"),
        "expected both `alpha` and `America/New_York` inside `Foo::\"a|\"`, got {labels:?}"
    );

    // 4. Mid-ident invocation. The user has typed `Foo::TimeZone`
    //    and re-invokes completion with the cursor in the middle
    //    (`Foo::Tim|eZone`). Accepting `TimeStamp` (which matches
    //    the `Tim` prefix) must REPLACE the whole `TimeZone` token,
    //    not insert at the cursor — the previous `insert_text`-only
    //    shape produced `Foo::TimTimeStampeZone`.
    let mid_src = concat!(
        "enum E { alpha, TimeStamp, TimeZone }\n",
        "fn t() {\n",
        "    var x = E::TimeZone;\n",
        "}\n",
    );
    let mid_uri = Uri::from_str("file:///mid.gcl").unwrap();
    let mut mid_mgr = SourceManager::new();
    mid_mgr.add_simple(mid_uri.clone(), mid_src, "project", false);
    let mid_pa = ProjectAnalysis::analyze(&mid_mgr);
    // Cursor right after the `Tim` prefix on line 3 (the use site,
    // not the decl).
    let use_offset = mid_src.find("E::TimeZone").unwrap() + "E::Tim".len();
    let items = complete_items(&mid_mgr, &mid_uri, &mid_pa, use_offset);
    let timestamp = items
        .iter()
        .find(|c| c.label == "TimeStamp")
        .unwrap_or_else(|| panic!("expected `TimeStamp` at mid-ident cursor, got {items:?}"));
    let after = apply_edit(mid_src, timestamp);
    assert!(
        after.contains("E::TimeStamp;"),
        "mid-ident completion must replace the whole token; got `{after}`"
    );
    assert!(
        !after.contains("TimTimeStampeZone") && !after.contains("TimeStampeZone"),
        "mid-ident completion must not double the surrounding ident; got `{after}`"
    );

    // 5. Mid-string-property invocation. The user has typed
    //    `F::"TimeZone"` and re-invokes completion with the cursor
    //    after the `Tim` prefix. Accepting `TimeStamp` must replace
    //    the whole inner string content (the closing `"` stays put).
    let mid_str_src = concat!(
        "enum F { \"TimeStamp\", \"TimeZone\" }\n",
        "fn t() {\n",
        "    var x = F::\"TimeZone\";\n",
        "}\n",
    );
    let mid_str_uri = Uri::from_str("file:///midstr.gcl").unwrap();
    let mut mid_str_mgr = SourceManager::new();
    mid_str_mgr.add_simple(mid_str_uri.clone(), mid_str_src, "project", false);
    let mid_str_pa = ProjectAnalysis::analyze(&mid_str_mgr);
    let use_offset = mid_str_src.find("F::\"TimeZone\"").unwrap() + "F::\"Tim".len();
    let items = complete_items(&mid_str_mgr, &mid_str_uri, &mid_str_pa, use_offset);
    let timestamp = items
        .iter()
        .find(|c| c.label == "TimeStamp")
        .unwrap_or_else(|| panic!("expected `TimeStamp` inside the string, got {items:?}"));
    let after = apply_edit(mid_str_src, timestamp);
    assert!(
        after.contains("F::\"TimeStamp\";"),
        "mid-string completion must replace the whole inner content; got `{after}`"
    );
    assert!(
        !after.contains("TimTimeStampeZone") && !after.contains("TimeStampeZone"),
        "mid-string completion must not leak the original suffix; got `{after}`"
    );
}

/// Mirrors the user's project.gcl: a bare-ident call `foo()` to a
/// fn declared in the same module with a `String?` return type. The
/// analyzer's first pass returns `any` for non-generic Ident-callee
/// calls; the cross-module post-pass closes the gap by reading the
/// fn's declared return type. Without this, var-init typing
/// (`var s = foo();`) and inlay hints fall back to `any` even when
/// the return type is right there in the source.
#[test]
fn inlay_hints_with_project_use_bare_ident_call_return_types() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    use std::str::FromStr;

    let main_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        main_uri.clone(),
        "native fn foo(): String?;\n\nfn main() {\n    var s = foo();\n}\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&main_uri).expect("doc");
    let doc = cell.borrow();
    let module = pa.module(&main_uri).expect("module cached");

    let range = lsp_types::Range {
        start: pos(0, 0),
        end: pos(99, 0),
    };
    let hints = capabilities::inlay_hints_with_project(
        module,
        &pa,
        &doc.text,
        &range,
        SourceEncoding::UTF8,
    );
    let var_hint = hints
        .iter()
        .find(|h| {
            matches!(
                &h.label,
                InlayHintLabel::String(s) if s.contains("String")
            )
        })
        .unwrap_or_else(|| panic!("expected `: String?` inlay hint on `var s`, got {hints:?}"));
    let InlayHintLabel::String(label) = &var_hint.label else {
        unreachable!()
    };
    assert_eq!(
        label, ": String?",
        "bare-ident fn call return type should propagate, got `{label}`"
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
    let edits = capabilities::formatting(&src, r, SourceEncoding::UTF8).expect("Some(edits)");
    assert!(edits.is_empty(), "expected no edits, got {edits:?}");
}

#[test]
fn formatting_returns_a_single_full_replacement_on_drift() {
    let src = "fn   sloppy   (   ){}\n";
    let mut t = None;
    let r = root(src, &mut t);
    let edits = capabilities::formatting(src, r, SourceEncoding::UTF8).expect("Some(edits)");
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
    let tokens = capabilities::semantic_tokens(src, "project", r, SourceEncoding::UTF8);
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

#[test]
fn semantic_tokens_emits_enum_member_for_variant_decl_and_ref() {
    // Declaration sites (`Red`, `Green`, `Blue` inside `enum Color { ... }`)
    // and the reference site (`Color::Red`) should both emit ENUM_MEMBER
    // (index 3), while the type `Color` itself stays ENUM (index 2).
    let src = "enum Color { Red, Green, Blue }\nfn pick(): Color { return Color::Red; }\n";
    let mut t = None;
    let r = root(src, &mut t);
    let tokens = capabilities::semantic_tokens(src, "project", r, SourceEncoding::UTF8);
    let enum_idx = 2u32;
    let enum_member_idx = 3u32;
    // Three decl-site variants + one variant ref = 4 ENUM_MEMBER tokens.
    let enum_member_count = tokens
        .data
        .iter()
        .filter(|t| t.token_type == enum_member_idx)
        .count();
    assert_eq!(
        enum_member_count, 4,
        "expected 4 ENUM_MEMBER tokens (3 decl-site + 1 ref), got {enum_member_count}"
    );
    // `Color` appears twice as a type: return-type annotation and static
    // receiver. Both should be ENUM.
    let enum_count = tokens
        .data
        .iter()
        .filter(|t| t.token_type == enum_idx)
        .count();
    assert_eq!(
        enum_count, 2,
        "expected 2 ENUM tokens (return-type + static receiver), got {enum_count}"
    );
}

#[test]
fn semantic_tokens_typed_suffix_distinct_from_digits() {
    // Painting the whole `number` node as NUMBER hides the textual
    // suffix in `42_time`, `3day_2hour42s`, `3.14f`. Tokenize each
    // numeric segment as NUMBER and each `number_suffix` as KEYWORD
    // (distinct theme color) — including the in-between suffixes of a
    // compound duration.
    let src = "fn main() { 42_time; 3day_2hour42s; 3.14f; }\n";
    let mut t = None;
    let r = root(src, &mut t);
    let tokens = capabilities::semantic_tokens(src, "project", r, SourceEncoding::UTF8);

    let number_idx = 7u32; // TOK_NUMBER
    let keyword_idx = 9u32; // TOK_KEYWORD

    // Decode delta-encoded ranges plus token type.
    let mut line = 0u32;
    let mut col = 0u32;
    let mut spans: Vec<(u32, u32, u32, u32)> = Vec::new(); // (line, col, len, ty)
    for tk in &tokens.data {
        if tk.delta_line != 0 {
            line += tk.delta_line;
            col = tk.delta_start;
        } else {
            col += tk.delta_start;
        }
        spans.push((line, col, tk.length, tk.token_type));
    }

    // No overlap on the same line.
    for win in spans.windows(2) {
        let (l0, c0, len0, _) = win[0];
        let (l1, c1, _, _) = win[1];
        if l0 == l1 {
            assert!(
                c0 + len0 <= c1,
                "overlapping semantic tokens on line {l0}: \
                 [{c0}..{}) overlaps [{c1}..)",
                c0 + len0
            );
        }
    }

    // Helper: text at a span.
    let line_text =
        |target_line: u32| -> &str { src.lines().nth(target_line as usize).unwrap_or("") };
    let span_text = |line: u32, col: u32, len: u32| -> &str {
        let lt = line_text(line);
        let start = col as usize;
        let end = start + len as usize;
        &lt[start..end.min(lt.len())]
    };

    let on_body_line: Vec<_> = spans.iter().filter(|(l, _, _, _)| *l == 0).collect();
    let mut texts: Vec<(&str, u32)> = on_body_line
        .iter()
        .map(|(l, c, len, ty)| (span_text(*l, *c, *len), *ty))
        .collect();
    // Drop the `main` ident (TOK_FN). Keep only number-related spans.
    texts.retain(|(_, ty)| *ty == number_idx || *ty == keyword_idx);

    // The body has spans, in order:
    //   42(NUMBER) _time(KEYWORD)
    //   3(NUMBER) day(KEYWORD) _2(?...) hour(KEYWORD) 42(NUMBER) s(KEYWORD)
    //   3.14(NUMBER) f(KEYWORD)
    //
    // The grammar puts trailing `_` either with `number_int` or
    // `number_suffix` depending on context — assert behavior, not
    // exact byte boundaries.
    let n_number = texts.iter().filter(|(_, ty)| *ty == number_idx).count();
    let n_keyword = texts.iter().filter(|(_, ty)| *ty == keyword_idx).count();
    assert!(
        n_number >= 5,
        "expected >=5 NUMBER spans (42, 3, 2, 42, 3.14); got {n_number}: {texts:?}"
    );
    assert!(
        n_keyword >= 5,
        "expected >=5 KEYWORD spans (time, day, hour, s, f); got {n_keyword}: {texts:?}"
    );

    // The textual suffixes must NOT contain digits — otherwise we'd be
    // back to painting digit-runs as suffix.
    for (txt, ty) in &texts {
        if *ty == keyword_idx {
            assert!(
                !txt.chars().any(|c| c.is_ascii_digit()),
                "suffix span `{txt}` contains digits — painting at the wrong granularity"
            );
        }
    }
}

#[test]
fn semantic_tokens_string_interpolation_no_overlap() {
    // The whole-`string` node previously got a STRING token spanning the
    // `${world}` substitution, which then overlapped the VARIABLE token
    // emitted for the inner `world` ident. LSP forbids overlapping
    // semantic tokens; VSCode reacted by losing the string color for the
    // interpolated section.
    let src = "fn main() { var world = 0; var s = \"hello ${world}\"; }\n";
    let mut t = None;
    let r = root(src, &mut t);
    let tokens = capabilities::semantic_tokens(src, "project", r, SourceEncoding::UTF8);

    // STRING token type is index 6.
    let string_type_idx = 6u32;
    let var_type_idx = 4u32;
    assert!(
        tokens.data.iter().any(|t| t.token_type == string_type_idx),
        "expected at least one STRING-typed token (the literal fragment)"
    );
    assert!(
        tokens.data.iter().any(|t| t.token_type == var_type_idx),
        "expected at least one VARIABLE-typed token (the interpolated `world`)"
    );

    // Decode delta-encoded ranges and assert no two intervals overlap.
    let mut line = 0u32;
    let mut col = 0u32;
    let mut ranges: Vec<(u32, u32, u32)> = Vec::new();
    for tk in &tokens.data {
        if tk.delta_line != 0 {
            line += tk.delta_line;
            col = tk.delta_start;
        } else {
            col += tk.delta_start;
        }
        ranges.push((line, col, tk.length));
    }
    for win in ranges.windows(2) {
        let (l0, c0, len0) = win[0];
        let (l1, c1, _) = win[1];
        if l0 == l1 {
            assert!(
                c0 + len0 <= c1,
                "overlapping semantic tokens on line {l0}: \
                 [{c0}..{}) overlaps [{c1}..)",
                c0 + len0
            );
        }
    }
}

#[test]
fn semantic_tokens_paints_lambda_return_type_as_type() {
    // The return-type annotation on a lambda (`fn (...): T { ... }`) is a
    // `type_ident`; its `name` ident must paint as TYPE (index 1) just
    // like `fn_decl`'s return type does — not fall through to "no token".
    let src = "fn main() { var f = fn (a: int): int { return a; }; }\n";
    let mut t = None;
    let r = root(src, &mut t);
    let tokens = capabilities::semantic_tokens(src, "project", r, SourceEncoding::UTF8);

    let type_idx = 1u32;
    let mut line = 0u32;
    let mut col = 0u32;
    let mut spans: Vec<(u32, u32, u32, u32)> = Vec::new();
    for tk in &tokens.data {
        if tk.delta_line != 0 {
            line += tk.delta_line;
            col = tk.delta_start;
        } else {
            col += tk.delta_start;
        }
        spans.push((line, col, tk.length, tk.token_type));
    }

    // Two `int` occurrences on line 0: the param type at col 28 and the
    // lambda return type at col 33. Both must paint as TYPE.
    let int_type_spans: Vec<_> = spans
        .iter()
        .filter(|(_, c, len, ty)| {
            *ty == type_idx
                && src
                    .lines()
                    .next()
                    .and_then(|l| l.get(*c as usize..(*c + *len) as usize))
                    == Some("int")
        })
        .collect();
    assert_eq!(
        int_type_spans.len(),
        2,
        "expected 2 TYPE spans covering `int` (param type + lambda return type); \
         got spans: {spans:?}",
    );
}

// =============================================================================
// P24.5 — DiagnosticTag::UNNECESSARY plumbing
// =============================================================================

#[test]
fn dead_code_lint_carries_unnecessary_tag() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    use std::str::FromStr;
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///mod.gcl").unwrap();
    mgr.add_simple(
        uri.clone(),
        "fn f(): int { return 1; var _ = 0; }\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let module = pa.module(&uri).unwrap();
    let cell = mgr.get(&uri).unwrap();
    let doc = cell.borrow();
    let diags =
        capabilities::diagnostics_from_module(&doc.text, module, false, SourceEncoding::UTF8);
    let unreachable = diags
        .iter()
        .find(|d| {
            matches!(
                &d.code,
                Some(NumberOrString::String(s)) if s == "unreachable"
            )
        })
        .expect("expected an `unreachable` diagnostic");
    let tags = unreachable
        .tags
        .as_ref()
        .expect("expected `tags` to be set on `unreachable`");
    assert!(
        tags.contains(&DiagnosticTag::UNNECESSARY),
        "expected UNNECESSARY tag, got {tags:?}"
    );
}

#[test]
fn unused_local_carries_unnecessary_tag() {
    // P24.5 retroactively — `unused-local` is one of the rules that
    // should have been carrying UNNECESSARY all along.
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    use std::str::FromStr;
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///mod.gcl").unwrap();
    mgr.add_simple(
        uri.clone(),
        "fn f(): int { var unused = 0; return 1; }\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let module = pa.module(&uri).unwrap();
    let cell = mgr.get(&uri).unwrap();
    let doc = cell.borrow();
    let diags =
        capabilities::diagnostics_from_module(&doc.text, module, false, SourceEncoding::UTF8);
    let unused = diags
        .iter()
        .find(|d| {
            matches!(
                &d.code,
                Some(NumberOrString::String(s)) if s == "unused-local"
            )
        })
        .expect("expected `unused-local`");
    let tags = unused.tags.as_ref().expect("expected tags on unused-local");
    assert!(tags.contains(&DiagnosticTag::UNNECESSARY));
}

// =============================================================================
// P23.5 — directive completion (`// gcl-…`)
// =============================================================================

#[test]
fn completion_inside_gcl_directive_comment_lists_directives() {
    let src = "// gcl-\nfn f() {}\n";
    let test_project = TestProject::single_file(src);
    // Cursor right after `// gcl-` (line 0, character 7).
    let list = test_project
        .completion(pos(0, 7))
        .expect("expected completion items inside `// gcl-`");
    let labels: Vec<_> = list.items.into_iter().map(|c| c.label).collect();
    assert!(
        labels.iter().any(|l| l == "gcl-lint-off"),
        "expected `gcl-lint-off` in directive completion, got {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "gcl-fmt-file-off"),
        "expected `gcl-fmt-file-off` in directive completion, got {labels:?}"
    );
}

#[test]
fn completion_inside_lint_off_rule_list_lists_known_rules() {
    let src = "// gcl-lint-off \nfn f() {}\n";
    let test_project = TestProject::single_file(src);
    // Cursor at the rule-list slot (right after the trailing space).
    let list = test_project
        .completion(pos(0, 16))
        .expect("expected rule-list completion inside `gcl-lint-off `");
    let labels: Vec<_> = list.items.into_iter().map(|c| c.label).collect();
    assert!(
        labels.iter().any(|l| l == "unused-decl"),
        "expected `unused-decl` rule in completion, got {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "possibly-null"),
        "expected `possibly-null` rule in completion, got {labels:?}"
    );
}

// P37.5 — `breakpoint`, `break`, and `continue` are all listed in
// `ALL_KEYWORDS` so the keyword completer surfaces them at statement
// positions. Regression test against the gap where the slice was
// originally seeded without `break` / `continue` / `breakpoint`.
#[test]
fn completion_at_stmt_position_includes_breakpoint() {
    // `br;` parses as `expr_stmt (ident)`; cursor at the end of `br`
    // (before `;`) lands inside the ident and the keyword completer's
    // prefix filter narrows `ALL_KEYWORDS` to entries starting with
    // `br` (`break` and `breakpoint`).
    let src = "fn f() {\n    br;\n}\n";
    let test_project = TestProject::single_file(src);
    let cursor = src.find("br;").unwrap() + 2;
    let line = src[..cursor].matches('\n').count() as u32;
    let col = (cursor - src[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    let list = test_project
        .completion(pos(line, col))
        .expect("expected keyword completion at statement position");
    let labels: Vec<_> = list.items.into_iter().map(|c| c.label).collect();
    assert!(
        labels.iter().any(|l| l == "breakpoint"),
        "expected `breakpoint` in stmt-position keyword completion, got {labels:?}"
    );
    // Also verify the previously-missing siblings landed in the same pass.
    assert!(
        labels.iter().any(|l| l == "break"),
        "expected `break` in stmt-position keyword completion, got {labels:?}"
    );
}
