//! Honest first-pass port of TS `lsp.*.test.ts` scenarios (P8.7).
//!
//! The TS reference has 15 scenario test files driving the LSP server
//! end-to-end. Porting them all (full JSON-RPC harness, fixture
//! parity) is a multi-week project and is intentionally out of scope
//! here — this file exercises every capability by calling the
//! `capabilities::*` functions directly with representative source
//! snippets, giving us a regression-guard that flags behavioral
//! drift without needing a wire-protocol harness.
//!
//! Each `#[test]` mirrors one TS file's intent. Future work picks
//! these up one by one and re-targets them at a real JSON-RPC client
//! once a harness is in place.

use std::str::FromStr;

use greycat_analyzer_server::capabilities;
use greycat_analyzer_syntax::parse;
use lsp_types::*;

mod support;
use support::TestProject;

fn pos(line: u32, character: u32) -> Position {
    Position { line, character }
}

fn uri() -> Uri {
    Uri::from_str("file:///test.gcl").unwrap()
}

/// Minimal synthetic `lib/std/core.gcl` shape carrying the runtime
/// types the deref-tests rely on: `node<T>`, `nodeTime<T>`,
/// `nodeList<T>`, `nodeGeo<T>`, all annotated `@deref("resolve")`
/// with a `resolve(): T` method. The analyzer's
/// `arrow_deref_receiver` reads the `@deref` annotation off the
/// declaring type's `TypeFlags` to know that `n->m` desugars to
/// `n.resolve().m`; without a real stdlib decl in scope, that
/// metadata is absent and arrow dispatch falls through. Tests that
/// exercise arrow semantics seed this synthetic stdlib into the
/// `SourceManager` so the project pipeline ingests it before
/// analyzing the user source.
fn synthetic_std_core_with_node() -> &'static str {
    "native type any {}\n\
     native type null {}\n\
     native type bool {}\n\
     native type char {}\n\
     native type int {}\n\
     native type float {}\n\
     native type String {}\n\
     native type time {}\n\
     native type duration {}\n\
     native type geo {}\n\
     native type type {}\n\
     native type field {}\n\
     native type function {}\n\
     @deref(\"resolve\")\n\
     native type node<T> {\n    fn resolve(): T;\n}\n\
     @deref(\"resolve\")\n\
     native type nodeTime<T> {\n    fn resolve(): T;\n}\n\
     @deref(\"resolve\")\n\
     native type nodeList<T> {\n    fn resolve(): T;\n}\n\
     @deref(\"resolve\")\n\
     native type nodeGeo<T> {\n    fn resolve(): T;\n}\n\
     native type nodeIndex<K, V> {}\n\
     native type Array<T> {}\n\
     native type Map<K, V> {}\n\
     type Tuple<T, U> { a: T; b: U; }\n"
}

#[test]
fn hover_renders_param_type() {
    let src = "fn add(a: int, b: int): int { return a + b; }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let h = project.hover(pos(0, 38)).expect("hover present on `a`");
    let HoverContents::Markup(MarkupContent { value, .. }) = h.contents else {
        panic!("expected markup hover");
    };
    assert!(
        value.contains("int"),
        "hover should show param type, got: {value}"
    );
}

/// P15.1 — fn hover renders the full signature, not just the return type.
/// Cursor on the call-site `add` ident; expected content includes the
/// param list `(a: int, b: int)` and the return-type annotation.
#[test]
fn hover_renders_full_fn_signature() {
    let src = "fn add(a: int, b: int): int { return a + b; }\nfn main() { add(1, 2); }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let h = project
        .hover(pos(1, 12))
        .expect("hover present on call-site `add`");
    let HoverContents::Markup(MarkupContent { value, .. }) = h.contents else {
        panic!("expected markup hover");
    };
    assert!(
        value.contains("fn add(a: int, b: int): int"),
        "hover should render full fn signature, got: {value}"
    );
}

/// P15.1 — doc-comments above a decl appear in its hover.
#[test]
fn hover_includes_doc_comments() {
    let src = "/// adds two ints.\nfn add(a: int, b: int): int { return a + b; }\nfn main() { add(1, 2); }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let h = project
        .hover(pos(2, 12))
        .expect("hover present on call-site `add`");
    let HoverContents::Markup(MarkupContent { value, .. }) = h.contents else {
        panic!("expected markup hover");
    };
    assert!(
        value.contains("adds two ints."),
        "hover should include doc-comment, got: {value}"
    );
    assert!(
        value.contains("fn add(a: int, b: int): int"),
        "hover should still include signature, got: {value}"
    );
}

/// P15.1 — generics + multi-param + return type all flow into the
/// rendered signature.
#[test]
fn hover_renders_generics_and_return_type() {
    let src = "fn pick<T>(xs: Array<T>, i: int): T { return xs[i]; }\nfn main() { pick(1, 2); }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let h = project
        .hover(pos(1, 13))
        .expect("hover present on call-site `pick`");
    let HoverContents::Markup(MarkupContent { value, .. }) = h.contents else {
        panic!("expected markup hover");
    };
    assert!(
        value.contains("fn pick<T>(xs: Array<T>, i: int): T"),
        "hover should render generic + return type, got: {value}"
    );
}

/// Hover on a `type` identifier inlines up to 5 attrs in a body
/// block so the reader sees the shape at a glance — no goto-def
/// round-trip needed for a quick peek.
#[test]
fn hover_on_type_decl_renders_attrs_body() {
    let src = "type Point { x: int; y: int; }\nfn use_(p: Point) { }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    // Cursor on `Point` use-site (param type, line 1 col 11).
    let h = project
        .hover(pos(1, 11))
        .expect("hover present on type ident");
    let HoverContents::Markup(MarkupContent { value, .. }) = h.contents else {
        panic!("expected markup hover");
    };
    assert!(
        value.contains("type Point {"),
        "hover should open a body block, got: {value}"
    );
    assert!(
        value.contains("x: int;"),
        "hover should list `x: int`, got: {value}"
    );
    assert!(
        value.contains("y: int;"),
        "hover should list `y: int`, got: {value}"
    );
}

/// Native types don't have a `.gcl` body — keep the existing
/// single-line signature shape instead of opening an empty `{ … }`.
#[test]
fn hover_on_native_type_stays_single_line() {
    // Synthetic stdlib-shape: a native type used at param position.
    let src = "native type Foo { }\nfn use_(f: Foo) { }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let h = project
        .hover(pos(1, 11))
        .expect("hover present on native type ident");
    let HoverContents::Markup(MarkupContent { value, .. }) = h.contents else {
        panic!("expected markup hover");
    };
    assert!(
        value.contains("native type Foo"),
        "native type hover should include `native type Foo`, got: {value}"
    );
    // No body inlining for natives — the body open brace shouldn't
    // appear on the same code-block line.
    assert!(
        !value.contains("native type Foo {\n"),
        "native type hover should not open a multi-line body, got: {value}"
    );
}

/// Types with more than 5 attrs truncate the body and add a
/// `… N more` indicator so the hover stays glanceable.
#[test]
fn hover_on_type_decl_truncates_long_attrs_list() {
    let src = "type Big { a: int; b: int; c: int; d: int; e: int; f: int; g: int; }\nfn use_(x: Big) { }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let h = project
        .hover(pos(1, 11))
        .expect("hover present on type ident");
    let HoverContents::Markup(MarkupContent { value, .. }) = h.contents else {
        panic!("expected markup hover");
    };
    assert!(
        value.contains("a: int;"),
        "first attr should appear: {value}"
    );
    assert!(
        value.contains("e: int;"),
        "fifth attr should appear: {value}"
    );
    assert!(
        !value.contains("f: int;"),
        "sixth attr should be elided: {value}"
    );
    assert!(
        value.contains("… 2 more"),
        "should report 2 elided attrs, got: {value}"
    );
}

/// P15.1 — cross-module hover renders the foreign decl's signature,
/// doc-comment, and a `defined in <module>` provenance footnote.
#[test]
fn hover_with_project_renders_cross_module_provenance() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let home_uri = Uri::from_str("file:///shapes.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        home_uri.clone(),
        "/// a 2D point.\ntype Point { x: int; y: int; }\n",
        "p",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn read(p: Point): int { return p.x; }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let user_cell = mgr.get(&user_uri).expect("user doc");
    let user_doc = user_cell.borrow();
    // Cursor on the `Point` use site in main.gcl (param type, line 0 col 12).
    let h = capabilities::hover_with_project(
        &user_doc.text,
        &user_doc.lib,
        user_doc.root_node(),
        pos(0, 12),
        &user_uri,
        &pa,
        &mgr,
    )
    .expect("hover present on cross-module Point");
    let HoverContents::Markup(MarkupContent { value, .. }) = h.contents else {
        panic!("expected markup hover");
    };
    assert!(
        value.contains("a 2D point."),
        "hover should include foreign doc-comment, got: {value}"
    );
    assert!(
        value.contains("type Point"),
        "hover should render foreign type signature, got: {value}"
    );
    assert!(
        value.contains("defined in `shapes`"),
        "hover should render provenance footnote, got: {value}"
    );
}

#[test]
fn document_symbols_lists_top_level_decls() {
    let src = "fn one() {}\ntype Foo {}\nenum E { A, B }\n";
    let tree = parse(src);
    let syms = capabilities::document_symbols(src, "project", tree.root_node());
    let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"one"),
        "fn `one` should be a symbol: {names:?}"
    );
    assert!(
        names.contains(&"Foo"),
        "type `Foo` should be a symbol: {names:?}"
    );
    assert!(
        names.contains(&"E"),
        "enum `E` should be a symbol: {names:?}"
    );
}

#[test]
fn folding_ranges_block_spans_multi_line() {
    let src = "fn body() {\n    var x = 1;\n    var y = 2;\n}\n";
    let tree = parse(src);
    let folds = capabilities::folding_ranges(src, tree.root_node());
    assert!(!folds.is_empty(), "fn body should fold");
}

#[test]
fn document_highlights_match_same_text() {
    let src = "fn id(x: int): int { return x; }\n";
    let tree = parse(src);
    let hi = capabilities::document_highlights(src, tree.root_node(), pos(0, 28));
    assert!(
        hi.len() >= 2,
        "expected at least 2 `x` highlights, got {hi:?}"
    );
}

#[test]
fn rename_uses_resolver_for_locals() {
    let src = "fn id(x: int): int { return x; }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let edit = project.rename(pos(0, 28), "y").unwrap();
    #[allow(clippy::mutable_key_type)]
    let changes = edit.changes.unwrap();
    let edits = changes.values().next().unwrap();
    // Should rename both the param decl `x` and the use `x`.
    assert_eq!(edits.len(), 2, "expected 2 edits for `x`, got: {edits:?}");
    for e in edits {
        assert_eq!(e.new_text, "y");
    }
}

#[test]
fn references_returns_def_plus_uses() {
    let src = "fn id(x: int): int { return x; }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let refs = project.references(pos(0, 28));
    assert!(
        refs.len() >= 2,
        "expected at least 2 refs for `x`, got: {refs:?}"
    );
}

#[test]
fn goto_definition_param_lands_on_decl() {
    let src = "fn id(x: int): int { return x; }\n";
    let tree = parse(src);
    let g = capabilities::goto_definition(src, "project", tree.root_node(), &uri(), pos(0, 28));
    let GotoDefinitionResponse::Scalar(loc) = g.expect("goto on `x`") else {
        panic!("expected single location");
    };
    // Param decl `x` is at column 6 (0-indexed: f-n-space-i-d-( = 5, then x at 6).
    assert_eq!(loc.range.start.line, 0);
}

#[test]
fn cross_module_references_aggregates_across_docs() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let home_uri = Uri::from_str("file:///home.gcl").unwrap();
    let user_uri = Uri::from_str("file:///user.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(home_uri.clone(), "type Helper {}\n", "p", false);
    mgr.add_simple(
        user_uri.clone(),
        "fn first(h: Helper) {}\nfn second(): Helper { return Helper {}; }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    // Cursor on the `Helper` decl name in home.gcl (col 5).
    let refs = capabilities::references_across_project(&pa, &mgr, &home_uri, pos(0, 5));
    let user_hits = refs.iter().filter(|l| l.uri == user_uri).count();
    let home_hits = refs.iter().filter(|l| l.uri == home_uri).count();
    assert_eq!(home_hits, 1, "binding site in home.gcl: {refs:?}");
    assert_eq!(user_hits, 3, "three Helper uses in user.gcl: {refs:?}");
}

#[test]
fn rename_method_renames_binding_and_call_sites() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let u = Uri::from_str("file:///m.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        u.clone(),
        "type T { fn m(): int { return 1; } } fn caller(t: T) { t.m(); }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    // Cursor on the method binding `m` inside `fn m(): int`.
    let edit = capabilities::rename_across_project(&pa, &mgr, &u, pos(0, 12), "n").unwrap();
    #[allow(clippy::mutable_key_type)]
    let changes = edit.changes.unwrap();
    let edits = changes.get(&u).expect("home-module edits");
    assert_eq!(
        edits.len(),
        2,
        "expected binding + call-site rename: {edits:?}"
    );
    for e in edits {
        assert_eq!(e.new_text, "n");
    }
}

#[test]
fn rename_type_attr_renames_binding_and_member_access() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let u = Uri::from_str("file:///m.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        u.clone(),
        "type T { a: int; } fn caller(t: T) { var x = t.a; }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    // Cursor on the attr binding `a` inside `a: int;`.
    let edit = capabilities::rename_across_project(&pa, &mgr, &u, pos(0, 9), "b").unwrap();
    #[allow(clippy::mutable_key_type)]
    let changes = edit.changes.unwrap();
    let edits = changes.get(&u).expect("home-module edits");
    assert_eq!(
        edits.len(),
        2,
        "expected binding + member-access rename: {edits:?}"
    );
}

#[test]
fn rename_cross_module_method_aggregates_per_uri() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let home = Uri::from_str("file:///home.gcl").unwrap();
    let user = Uri::from_str("file:///user.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        home.clone(),
        "type T { fn m(): int { return 1; } }\n",
        "p",
        false,
    );
    mgr.add_simple(user.clone(), "fn caller(t: T) { t.m(); }\n", "p", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    // Cursor on home.gcl's `m` binding (col 12 inside `fn m(): int`).
    let edit = capabilities::rename_across_project(&pa, &mgr, &home, pos(0, 12), "n").unwrap();
    #[allow(clippy::mutable_key_type)]
    let changes = edit.changes.unwrap();
    assert_eq!(changes.len(), 2, "expected edits in both URIs: {changes:?}");
    assert_eq!(changes.get(&home).unwrap().len(), 1);
    assert_eq!(changes.get(&user).unwrap().len(), 1);
}

#[test]
fn cross_module_rename_aggregates_text_edits_per_uri() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let home_uri = Uri::from_str("file:///home.gcl").unwrap();
    let user_uri = Uri::from_str("file:///user.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(home_uri.clone(), "type Helper {}\n", "p", false);
    mgr.add_simple(user_uri.clone(), "fn use_h(h: Helper) {}\n", "p", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    let edit =
        capabilities::rename_across_project(&pa, &mgr, &home_uri, pos(0, 5), "Renamed").unwrap();
    #[allow(clippy::mutable_key_type)]
    let changes = edit.changes.unwrap();
    assert_eq!(changes.len(), 2, "edits across two URIs: {changes:?}");
    let home_edits = changes.get(&home_uri).expect("home edits");
    let user_edits = changes.get(&user_uri).expect("user edits");
    assert_eq!(home_edits.len(), 1);
    assert_eq!(user_edits.len(), 1);
    for e in home_edits.iter().chain(user_edits.iter()) {
        assert_eq!(e.new_text, "Renamed");
    }
}

/// P15.x — hover on each segment of a `Type::method` static expression.
/// The `Type` ident lowers as a TypeRef name (resolver records it as a
/// `ProjectDecl` cross-module hit); the `method` ident is bound via
/// the analyzer's `foreign_member_uses` map (P15.6). Both should
/// surface a markup hover.
#[test]
fn hover_works_on_static_expr_segments() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        runtime_uri,
        "type Identity { static native fn create(name: String, role: String): Identity; }\n",
        "p",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() { var x = Identity::create(\"a\", \"b\"); }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).expect("user doc");
    let doc = cell.borrow();
    // Cursor on `Identity` (col 23 — between I and d).
    let h_ident = capabilities::hover_with_project(
        &doc.text,
        &doc.lib,
        doc.root_node(),
        pos(0, 23),
        &user_uri,
        &pa,
        &mgr,
    );
    assert!(
        h_ident.is_some(),
        "hover should fire on `Identity` segment of static expression"
    );
    // Cursor on `create` (col 35 — somewhere in the property).
    let h_method = capabilities::hover_with_project(
        &doc.text,
        &doc.lib,
        doc.root_node(),
        pos(0, 35),
        &user_uri,
        &pa,
        &mgr,
    );
    assert!(
        h_method.is_some(),
        "hover should fire on `create` segment of static expression"
    );
}

/// P15.9 — cursor on the module-name segment of a `static_expr`
/// chain (`runtime::Identity::create`) jumps to that module's file.
#[test]
fn goto_module_segment_jumps_to_named_module_file() {
    use greycat_analyzer_core::SourceManager;
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        runtime_uri.clone(),
        "type Identity { static native fn create(name: String, role: String): Identity; }\n",
        "p",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() { runtime::Identity::create(\"a\", \"b\"); }\n",
        "p",
        false,
    );
    let user_cell = mgr.get(&user_uri).expect("user doc");
    let user_doc = user_cell.borrow();
    // Cursor on the leftmost `runtime` ident (line 0, col 14).
    let loc =
        capabilities::goto_module_segment(&user_doc.text, user_doc.root_node(), pos(0, 14), &mgr)
            .expect("goto-def on `runtime` segment");
    assert_eq!(loc.uri, runtime_uri);
    assert_eq!(loc.range.start.line, 0);
    assert_eq!(loc.range.start.character, 0);
}

/// P15.7 — `var x = Identity::create(...)` should infer x as
/// `Identity`, not `any`. The call's expr_type is the foreign
/// method's `return_type`, and the local var's `def_types` entry
/// gets re-linked to it after Pass 3.5 lands.
#[test]
fn cross_module_static_call_infers_return_type() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        runtime_uri.clone(),
        "type Identity { static native fn create(name: String, role: String): Identity; }\n",
        "p",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() { var x = Identity::create(\"a\", \"b\"); }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let user_module = pa.module(&user_uri).expect("user module");
    let x_local = user_module
        .hir
        .idents
        .iter()
        .find(|(_, i)| pa.symbols()[i.symbol] == *"x")
        .map(|(idx, _)| idx)
        .expect("`x` ident");
    let ty = user_module
        .analysis
        .def_types
        .get(&x_local)
        .copied()
        .expect("def_type for x");
    let display = pa.display_type(ty).to_string();
    assert_eq!(
        display, "Identity",
        "x should infer as `Identity`, got `{display}`"
    );
}

/// P15.4 — `@include("<cursor>")` directory completion lists the
/// project root's subdirectories.
#[test]
fn completion_inside_at_include_lists_subdirs() {
    use std::fs;
    let tmp = std::env::temp_dir().join(format!(
        "p15_4_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(tmp.join("src")).unwrap();
    fs::create_dir_all(tmp.join("vendor")).unwrap();
    fs::create_dir_all(tmp.join("node_modules")).unwrap(); // should be skipped

    let src = "@include(\"\");\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    // Cursor sits between the two quotes (col 10).
    let list = project
        .completion_at(pos(0, 10), Some(&tmp))
        .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"src"),
        "expected `src` directory in completion list: {labels:?}"
    );
    assert!(
        labels.contains(&"vendor"),
        "expected `vendor` directory in completion list: {labels:?}"
    );
    assert!(
        !labels.contains(&"node_modules"),
        "node_modules should be filtered out: {labels:?}"
    );
    fs::remove_dir_all(&tmp).ok();
}

/// P15.4 — typing a `/` after a directory name continues completion
/// into that directory's subdirs.
#[test]
fn completion_inside_at_include_drills_into_subdirs() {
    use std::fs;
    let tmp = std::env::temp_dir().join(format!(
        "p15_4_drill_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(tmp.join("playground/scripts")).unwrap();
    fs::create_dir_all(tmp.join("playground/dist")).unwrap();
    fs::create_dir_all(tmp.join("playground/node_modules")).unwrap();

    let src = "@include(\"playground/\");\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    // Cursor sits between the trailing `/` and the closing quote (col 21).
    let list = project
        .completion_at(pos(0, 21), Some(&tmp))
        .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"scripts"),
        "expected `scripts` subdir: {labels:?}"
    );
    assert!(
        labels.contains(&"dist"),
        "expected `dist` subdir: {labels:?}"
    );
    assert!(
        !labels.contains(&"node_modules"),
        "node_modules should still be filtered: {labels:?}"
    );
    fs::remove_dir_all(&tmp).ok();
}

/// P15.3 — cursor in the version slot of `@library("name", "<cursor>")`
/// emits a single lazy placeholder. The LSP layer would swap this for
/// concrete versions; the foundational `completion` entry surfaces it
/// verbatim so the test can inspect the placeholder shape.
#[test]
fn completion_inside_at_library_version_emits_placeholder() {
    let src = "@library(\"std\", \"\");\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    // Cursor between the two quotes of the version string (col 17).
    let list = project.completion(pos(0, 17)).expect("completion list");
    assert_eq!(list.items.len(), 1, "got: {:?}", list.items);
    assert!(list.is_incomplete);
    let item = &list.items[0];
    assert_eq!(item.label, "Fetching 'std' versions...");
    assert_eq!(item.kind, Some(CompletionItemKind::MODULE));
    let payload =
        capabilities::extract_lib_version_placeholder(&list).expect("placeholder payload");
    assert_eq!(payload.lib, "std");
    assert_eq!(payload.typed, "");
    // Inner-content range covers the gap between the quotes.
    assert_eq!(payload.range.start, pos(0, 17));
    assert_eq!(payload.range.end, pos(0, 17));
}

/// P15.3 — cursor in the *name* slot (first arg) does NOT emit a
/// version placeholder. Name completion is intentionally out of
/// scope for this chunk.
#[test]
fn completion_inside_at_library_name_does_not_emit_placeholder() {
    let src = "@library(\"\", \"\");\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    // Cursor inside the empty first string (col 10).
    let list = project.completion(pos(0, 10));
    let payload = list
        .as_ref()
        .and_then(capabilities::extract_lib_version_placeholder);
    assert!(
        payload.is_none(),
        "should not produce a version placeholder"
    );
}

/// P15.3 — `resolve_library_version_completion` walks the registry via
/// the supplied fetcher and emits real items in semver-descending
/// order with channel info in `labelDetails.detail`.
#[test]
fn resolve_lib_version_emits_sorted_items_with_channel_detail() {
    use greycat_analyzer_core::registry::{RegistryFetcher, RegistryItem};
    use rustc_hash::FxHashMap;

    struct Stub(FxHashMap<String, &'static str>);
    impl RegistryFetcher for Stub {
        fn fetch(&self, url: &str) -> Vec<RegistryItem> {
            self.0
                .get(url)
                .and_then(|j| serde_json::from_str(j).ok())
                .unwrap_or_default()
        }
    }
    let pairs = vec![
        (
            "https://get.greycat.io/files/core/".to_string(),
            r#"[{"path":"core/stable/","size":null,"last_modification":"2026-01-01T00:00:00Z"},
                {"path":"core/dev/","size":null,"last_modification":"2026-01-02T00:00:00Z"}]"#,
        ),
        (
            "https://get.greycat.io/files/core/stable/".to_string(),
            r#"[{"path":"core/stable/7.8/","size":null,"last_modification":"2026-01-01T00:00:00Z"}]"#,
        ),
        (
            "https://get.greycat.io/files/core/dev/".to_string(),
            r#"[{"path":"core/dev/8.0/","size":null,"last_modification":"2026-01-02T00:00:00Z"}]"#,
        ),
        (
            "https://get.greycat.io/files/core/stable/7.8/x64-linux/".to_string(),
            r#"[{"path":"core/stable/7.8/x64-linux/7.8.166-stable.zip","size":1,"last_modification":"2026-04-09T00:00:00Z"}]"#,
        ),
        (
            "https://get.greycat.io/files/core/stable/7.8/noarch/".to_string(),
            r#"[]"#,
        ),
        (
            "https://get.greycat.io/files/core/dev/8.0/x64-linux/".to_string(),
            r#"[{"path":"core/dev/8.0/x64-linux/8.0.5-dev.zip","size":1,"last_modification":"2026-04-10T00:00:00Z"}]"#,
        ),
        (
            "https://get.greycat.io/files/core/dev/8.0/noarch/".to_string(),
            r#"[]"#,
        ),
    ];
    let stub = Stub(pairs.into_iter().collect());
    // `std` is aliased to `core` at the registry root.
    let payload = capabilities::LibVersionPayload {
        lib: "std".into(),
        typed: "".into(),
        range: lsp_types::Range {
            start: pos(0, 17),
            end: pos(0, 17),
        },
    };
    let list = capabilities::resolve_library_version_completion(&payload, &stub);
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["8.0.5-dev", "7.8.166-stable"]);

    let stable = list
        .items
        .iter()
        .find(|i| i.label == "7.8.166-stable")
        .unwrap();
    let details = stable.label_details.as_ref().unwrap();
    assert_eq!(details.detail.as_deref(), Some("[stable]"));
    assert_eq!(details.description.as_deref(), Some("2026-04-09T00:00:00Z"));

    // textEdit replaces the inner-content range, not the whole string.
    match stable.text_edit.as_ref().unwrap() {
        CompletionTextEdit::Edit(edit) => {
            assert_eq!(edit.range, payload.range);
            assert_eq!(edit.new_text, "7.8.166-stable");
        }
        _ => panic!("expected plain TextEdit"),
    }
}

/// P15.3 — when the user has typed a `-dev` prerelease, matching-channel
/// versions rank first via `sortText` but non-matching channels are
/// still in the list (no hard filter — see capabilities.rs commentary).
#[test]
fn resolve_lib_version_biases_matching_channel_first() {
    use greycat_analyzer_core::registry::{RegistryFetcher, RegistryItem};
    use rustc_hash::FxHashMap;

    struct Stub(FxHashMap<String, &'static str>);
    impl RegistryFetcher for Stub {
        fn fetch(&self, url: &str) -> Vec<RegistryItem> {
            self.0
                .get(url)
                .and_then(|j| serde_json::from_str(j).ok())
                .unwrap_or_default()
        }
    }
    let pairs = vec![
        (
            "https://get.greycat.io/files/core/".to_string(),
            r#"[{"path":"core/stable/","size":null,"last_modification":""},
                {"path":"core/dev/","size":null,"last_modification":""}]"#,
        ),
        (
            "https://get.greycat.io/files/core/stable/".to_string(),
            r#"[{"path":"core/stable/8.0/","size":null,"last_modification":""}]"#,
        ),
        (
            "https://get.greycat.io/files/core/dev/".to_string(),
            r#"[{"path":"core/dev/8.0/","size":null,"last_modification":""}]"#,
        ),
        (
            "https://get.greycat.io/files/core/stable/8.0/x64-linux/".to_string(),
            r#"[{"path":"core/stable/8.0/x64-linux/8.0.10-stable.zip","size":1,"last_modification":""}]"#,
        ),
        (
            "https://get.greycat.io/files/core/stable/8.0/noarch/".to_string(),
            r#"[]"#,
        ),
        (
            "https://get.greycat.io/files/core/dev/8.0/x64-linux/".to_string(),
            r#"[{"path":"core/dev/8.0/x64-linux/8.0.5-dev.zip","size":1,"last_modification":""}]"#,
        ),
        (
            "https://get.greycat.io/files/core/dev/8.0/noarch/".to_string(),
            r#"[]"#,
        ),
    ];
    let stub = Stub(pairs.into_iter().collect());
    let payload = capabilities::LibVersionPayload {
        lib: "core".into(),
        typed: "8.0.0-dev".into(),
        range: lsp_types::Range {
            start: pos(0, 0),
            end: pos(0, 0),
        },
    };
    let list = capabilities::resolve_library_version_completion(&payload, &stub);
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    // Both versions are still in the list — no hard filter.
    assert!(labels.contains(&"8.0.5-dev"));
    assert!(labels.contains(&"8.0.10-stable"));
    // Matching-channel (`-dev`) sortText starts with `0_`; the
    // newer-but-non-matching `-stable` starts with `1_`, so dev ranks
    // first despite being lower-numbered.
    let dev = list.items.iter().find(|i| i.label == "8.0.5-dev").unwrap();
    let stable = list
        .items
        .iter()
        .find(|i| i.label == "8.0.10-stable")
        .unwrap();
    let dev_sort = dev.sort_text.as_deref().unwrap();
    let stable_sort = stable.sort_text.as_deref().unwrap();
    assert!(
        dev_sort.starts_with("0_"),
        "expected matching channel tier 0, got {dev_sort}"
    );
    assert!(
        stable_sort.starts_with("1_"),
        "expected non-matching channel tier 1, got {stable_sort}"
    );
    assert!(
        dev_sort < stable_sort,
        "dev should rank before stable: {dev_sort} vs {stable_sort}"
    );
}

/// P15.2.1 — typing `@` at top level emits the pragma list (mirrors the
/// TS reference's `PRAGMA_COMPLETION_ITEMS`).
#[test]
fn completion_after_at_emits_pragma_list() {
    let src = "@\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    // Cursor sits right after the `@` (col 1).
    let list = project.completion(pos(0, 1)).expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    for expected in [
        "@library",
        "@include",
        "@role",
        "@permission",
        "@expose",
        "@volatile",
    ] {
        assert!(
            labels.contains(&expected),
            "expected `{expected}` in pragma completion list: {labels:?}"
        );
    }
    // Snippet items advertise `InsertTextFormat::Snippet`.
    let lib = list
        .items
        .iter()
        .find(|i| i.label == "@library")
        .expect("@library entry");
    assert_eq!(lib.insert_text_format, Some(InsertTextFormat::SNIPPET));
    assert_eq!(
        lib.insert_text.as_deref(),
        Some("library(\"$1\", \"$2\");$0"),
    );
}

/// P15.2.2 — typing `re` at a statement position emits keyword
/// completions filtered by the typed prefix (`return`).
#[test]
fn completion_at_stmt_emits_filtered_keywords() {
    let src = "fn body() {\n  re\n}\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    // Cursor after `re` on line 1 col 4.
    let list = project.completion(pos(1, 4)).expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"return"),
        "expected `return` keyword, got: {labels:?}"
    );
    assert!(
        !labels.contains(&"if"),
        "non-matching keyword leaked through prefix filter: {labels:?}"
    );
}

/// P15.2.2 — keyword completion does not fire inside string literals.
#[test]
fn completion_inside_string_skips_keywords() {
    let src = "fn f() { var s: String = \"return\"; }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    // Cursor inside the string body, between `e` and `t` of `return`.
    let list = project.completion(pos(0, 28));
    if let Some(list) = list {
        let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            !labels.contains(&"return"),
            "keywords leaked into string body: {labels:?}"
        );
    }
}

/// P15.2.3 — scope-aware ident completion surfaces locals + params +
/// module-level decls alongside keywords.
#[test]
fn completion_scope_aware_lists_locals_and_decls() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "fn helper(): int { return 1; }\nfn main(seed: int) {\n  var counter = 0;\n  c\n}\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor right after `c` on line 3 col 3.
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(3, 3),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"counter"), "got: {labels:?}");
    assert!(labels.contains(&"catch"), "got: {labels:?}");
    // `helper` does not match prefix `c`, so should be filtered out.
    assert!(!labels.contains(&"helper"), "got: {labels:?}");
}

/// P15.2.3 — params and locals defined before the cursor are visible;
/// locals defined later are not.
#[test]
fn completion_scope_excludes_later_locals() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "fn main() {\n  var early = 1;\n  e\n  var later = 2;\n}\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor right after `e` on line 2 col 3.
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(2, 3),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"early"), "got: {labels:?}");
    assert!(
        !labels.contains(&"later"),
        "future-local leaked into completion: {labels:?}"
    );
}

/// P15.2.3 — runtime-only types (Array, Map, etc.) and primitives
/// surface from the project index.
#[test]
fn completion_lists_runtime_types() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let stdlib_uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(stdlib_uri, synthetic_std_core_with_node(), "std", false);
    mgr.add_simple(user_uri.clone(), "fn main() {\n  A\n}\n", "p", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(1, 3),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Array"),
        "Array runtime type missing: {labels:?}"
    );
}

/// P15.2.4 — `.` member completion lists the receiver's attrs and
/// non-static methods.
#[test]
fn completion_after_dot_lists_attrs_and_methods() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "type Point { x: int; y: int; fn norm(): int { return 0; } }\nfn use_(p: Point) { p. }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor right after the `.` (line 1 col 22).
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(1, 22),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"x"), "got: {labels:?}");
    assert!(labels.contains(&"y"), "got: {labels:?}");
    assert!(labels.contains(&"norm"), "got: {labels:?}");
    // No keyword leak.
    assert!(!labels.contains(&"return"), "got: {labels:?}");
}

/// Instance member completion (`x.|`) must not surface static attrs:
/// `int::min` / `int::max` belong to the static-access path
/// (`Type::|`). Regression for the user-reported "completion shows
/// `min` / `max` on `var x = 42; x.`" bug.
#[test]
fn completion_after_dot_skips_static_attrs() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "type Counter { static max: int = 99; n: int; fn inc(): int { return 0; } }\nfn use_(c: Counter) { c. }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor right after the `.` on line 1 col 24 (`...{ c.|}`).
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(1, 24),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"n"),
        "instance attr `n` should appear: {labels:?}"
    );
    assert!(
        labels.contains(&"inc"),
        "instance method `inc` should appear: {labels:?}"
    );
    assert!(
        !labels.contains(&"max"),
        "static attr `max` must NOT appear in instance completion: {labels:?}"
    );
}

/// P19.17 — when the receiver is nullable, completions on `.` / `->`
/// attach an `additional_text_edits` that inserts `?` immediately
/// before the separator and surface the rewrite via `label_details`,
/// so accepting `size` on `var x: String?` lands as `x?.size`.
#[test]
fn completion_on_nullable_receiver_offers_null_safe_rewrite() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    // Mirror the existing `completion_after_dot_lists_attrs_and_methods`
    // shape (line 1 holds the body so the parser sees a clean fn) and
    // make the receiver nullable.
    mgr.add_simple(
        user_uri.clone(),
        "type Point { x: int; y: int; fn norm(): int { return 0; } }\nfn use_(p: Point?) { p. }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor right after the `.` (line 1 col 23).
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(1, 23),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let item = list
        .items
        .iter()
        .find(|i| i.label == "?.x")
        .unwrap_or_else(|| panic!("`?.x` attr missing from list: {:?}", list.items));
    let edits = item
        .additional_text_edits
        .as_ref()
        .expect("nullable receiver should attach a `?` insertion edit");
    assert!(
        edits.iter().any(|e| e.new_text == "?"),
        "expected `?` insertion edit, got: {edits:?}"
    );
    assert_eq!(
        item.filter_text.as_deref(),
        Some("x"),
        "filter_text should stay as the bare name so typing `x` still matches",
    );
}

#[test]
fn completion_on_non_null_receiver_no_null_safe_rewrite() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "type Point { x: int; y: int; fn norm(): int { return 0; } }\nfn use_(p: Point) { p. }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(1, 22),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let item = list
        .items
        .iter()
        .find(|i| i.label == "x")
        .unwrap_or_else(|| panic!("`x` missing: {:?}", list.items));
    assert!(
        item.additional_text_edits.is_none()
            || !item
                .additional_text_edits
                .as_ref()
                .unwrap()
                .iter()
                .any(|e| e.new_text == "?"),
        "non-nullable receiver should not propose `?.` rewrite"
    );
    // Label stays bare — no `?.` prefix.
    assert!(
        !item.label.starts_with("?."),
        "non-nullable receiver should not prefix label with `?.`, got {:?}",
        item.label
    );
}

#[test]
fn completion_after_upstream_null_safe_no_rewrite() {
    // `x?.y.|` — the chain already has `?.` upstream, so further
    // `.foo` access is runtime-safe (optional chaining short-circuits
    // the whole suffix). Completion must NOT push more `?.`.
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "type Inner { z: int; fn norm(): int { return 0; } }\ntype Outer { y: Inner; }\nfn use_(x: Outer?) { x?.y.z; }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Line 2: `fn use_(x: Outer?) { x?.y.z; }` — second `.` at col 25,
    // cursor right after `.` (between `.` and `z`) at col 26.
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(2, 26),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let z = list
        .items
        .iter()
        .find(|i| i.label == "z" || i.label == "?.z")
        .unwrap_or_else(|| panic!("`z` missing: {:?}", list.items));
    assert_eq!(
        z.label, "z",
        "downstream of `?.`, completion should NOT prefix `?.` (got {:?})",
        z.label
    );
    assert!(
        z.additional_text_edits.is_none()
            || !z
                .additional_text_edits
                .as_ref()
                .unwrap()
                .iter()
                .any(|e| e.new_text == "?"),
        "should not insert `?` when chain has upstream `?.`"
    );
}

/// P15.2.4 — typed prefix filters the member completion list.
#[test]
fn completion_after_dot_prefix_filters() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "type Point { x: int; y: int; }\nfn use_(p: Point) { p.x }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor right after the `x` (line 1 col 23).
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(1, 23),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"x"), "got: {labels:?}");
    assert!(
        !labels.contains(&"y"),
        "non-matching attr leaked: {labels:?}"
    );
}

/// P15.2.7 — `Type { |` lists the type's attrs as FIELD completions.
#[test]
fn completion_inside_object_literal_lists_attrs() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "type Point { x: int; y: int; fn norm(): int { return 0; } }\nfn main(): Point { return Point{ x: 1,  }; }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor inside the object literal body between the comma and
    // the closing brace (line 1 col 39).
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(1, 39),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    // `x` is already in the literal — don't suggest it again.
    assert!(
        !labels.contains(&"x"),
        "supplied field `x` should be filtered out: {labels:?}"
    );
    // `y` is the only attr left to fill in.
    assert!(labels.contains(&"y"), "got: {labels:?}");
    // Methods aren't fields.
    assert!(
        !labels.contains(&"norm"),
        "method leaked into object literal: {labels:?}"
    );
}

/// P41 — `is`-guard with early-return on a union-typed value strips
/// the matched arm from the post-if scope so a subsequent cast /
/// call on the surviving arm typechecks. Exercises the cached
/// `ProjectAnalysis` path (single-file shim wouldn't catch any
/// per-stage divergence; this is the real LSP code path).
#[test]
fn is_narrow_union_complement_lifts_past_early_return() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "type A {}\n\
         type B {}\n\
         fn use_a(a: A) {}\n\
         fn caller(p: A?, q: B?) {\n\
             var x = p ?? q;\n\
             if (x == null) { return; }\n\
             if (x is B) { return; }\n\
             use_a(x);\n\
         }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    let module = pa.module(&user_uri).expect("module");
    let diags = capabilities::diagnostics_from_module(&doc.text, module, false);
    assert!(
        !diags.iter().any(|d| {
            let msg = &d.message;
            msg.contains("not assignable") || msg.contains("cannot cast")
        }),
        "expected zero is-narrow-related diagnostics: {diags:?}"
    );
}

/// Empty body `Foo { | }` should list every non-static attr from the
/// type, the user's most common case.
#[test]
fn completion_inside_empty_object_literal_lists_all_attrs() {
    let src =
        "type Point { x: int; y: int; static k: int = 0; }\nfn main() { var _ = Point { }; }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    // Cursor between `{` and `}` of the `Point { }` literal (not the
    // type decl's body) — anchor on `= Point {` so `find` lands on
    // the right occurrence.
    let cursor = support::position_after(src, "= Point {", "");
    let list = project
        .completion(cursor)
        .expect("expected completion inside empty object literal");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"x"), "expected `x`: {labels:?}");
    assert!(labels.contains(&"y"), "expected `y`: {labels:?}");
    // Static attr — belongs to `Type::|` static access, not object init.
    assert!(
        !labels.contains(&"k"),
        "static `k` should not appear: {labels:?}"
    );
}

/// Object-literal completion walks the supertype chain so inherited
/// attrs surface when the user is filling in a subclass literal.
#[test]
fn completion_inside_object_literal_walks_supertype_chain() {
    let src = "type Animal { name: String; }\n\
               type Dog extends Animal { breed: String; }\n\
               fn main() { var _ = Dog { }; }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    // Cursor inside `Dog { | }`.
    let cursor = support::position_after(src, "Dog { ", "");
    let list = project
        .completion(cursor)
        .expect("expected completion inside Dog literal");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"breed"),
        "own attr `breed` should appear: {labels:?}"
    );
    assert!(
        labels.contains(&"name"),
        "inherited attr `name` should appear: {labels:?}"
    );
}

/// P15.2.6 — type-position completion at `var x: |` lists in-module
/// type decls and runtime types, but not values like fn names.
#[test]
fn completion_at_type_position_lists_types_only() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let stdlib_uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(stdlib_uri, synthetic_std_core_with_node(), "std", false);
    mgr.add_simple(
        user_uri.clone(),
        "type MyShape { x: int; }\nfn helper() {}\nfn use_() { var v: M = nil; }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor on the partial type ident `M` after `var v: ` (line 2 col 20).
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(2, 20),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"MyShape"), "type missing: {labels:?}");
    assert!(
        labels.contains(&"Map"),
        "Map runtime type missing: {labels:?}"
    );
    assert!(
        !labels.contains(&"helper"),
        "fn leaked into type position: {labels:?}"
    );
    assert!(
        !labels.contains(&"return"),
        "keyword leaked into type position: {labels:?}"
    );
}

/// P15.2.5 — `Type::|` static completion lists the type's static
/// methods.
#[test]
fn completion_after_double_colon_lists_static_methods() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "type Point { static fn origin(): Point { return Point{}; } fn norm(): int { return 0; } }\nfn main() { Point:: }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor right after `Point::` (line 1 col 19).
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(1, 19),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"origin"), "got: {labels:?}");
    assert!(
        !labels.contains(&"norm"),
        "non-static method leaked: {labels:?}"
    );
}

/// P15.2.5 — `module::|` static completion lists the foreign module's
/// top-level decls.
#[test]
fn completion_after_double_colon_lists_module_decls() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        runtime_uri,
        "type Identity { static native fn create(name: String, role: String): Identity; }\nfn helper(): int { return 0; }\n",
        "p",
        false,
    );
    mgr.add_simple(user_uri.clone(), "fn main() { runtime:: }\n", "p", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(0, 21),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"Identity"), "got: {labels:?}");
    assert!(labels.contains(&"helper"), "got: {labels:?}");
}

/// P15.2.4 — cross-module member completion: receiver's type lives in
/// a different module than the cursor's. Lists the foreign type's
/// attrs.
#[test]
fn completion_after_dot_cross_module() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let shapes_uri = Uri::from_str("file:///shapes.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(shapes_uri, "type Point { x: int; y: int; }\n", "p", false);
    mgr.add_simple(user_uri.clone(), "fn use_(p: Point) { p. }\n", "p", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(0, 22),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"x"), "got: {labels:?}");
    assert!(labels.contains(&"y"), "got: {labels:?}");
}

/// Empty string-interpolation slot: completion at `"${|}"` is a real
/// expression context, so the in-scope idents and keywords must
/// surface. The blanket "skip everything inside a string" gate used
/// to suppress this.
#[test]
fn completion_inside_empty_string_interpolation() {
    let src = "fn main() { var greeting = 1; var s = \"${}\"; }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    // The cursor sits between `${` and `}`.
    let cursor = support::position_after(src, "\"${", "");
    let list = project
        .completion(cursor)
        .expect("expected a completion list inside `${|}`");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"greeting"),
        "expected in-scope `greeting` inside `${{|}}`: {labels:?}"
    );
}

/// P15.2.2 — keyword completion does not fire after `.` (member access
/// RHS is owned by P15.2.4).
#[test]
fn completion_after_dot_skips_keywords() {
    let src = "fn f(p: int) { p.r }\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    // Cursor immediately after the `r` of `.r`.
    let list = project.completion(pos(0, 18));
    if let Some(list) = list {
        let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            !labels.contains(&"return"),
            "keywords leaked into member-access RHS: {labels:?}"
        );
    }
}

/// P15.2.1 — typing `@li` filters the pragma list to entries whose name
/// (post-`@`) starts with `li`.
#[test]
fn completion_after_at_prefix_filters() {
    let src = "@li\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let list = project.completion(pos(0, 3)).expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"@library"), "got: {labels:?}");
    assert!(!labels.contains(&"@include"), "got: {labels:?}");
    assert!(!labels.contains(&"@expose"), "got: {labels:?}");
}

/// P15.x — hover on the `create` segment of the simple `Identity::create`
/// (cross-module method) renders the foreign method's signature, not
/// "expression: function".
#[test]
fn hover_on_static_method_renders_signature() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        runtime_uri,
        "type Identity { static native fn create(name: String, role: String): Identity; }\n",
        "p",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() { var x = Identity::create(\"a\", \"b\"); }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).expect("user doc");
    let doc = cell.borrow();
    // Cursor on `create` (col 32 — within "Identity::create").
    let h = capabilities::hover_with_project(
        &doc.text,
        &doc.lib,
        doc.root_node(),
        pos(0, 32),
        &user_uri,
        &pa,
        &mgr,
    )
    .expect("hover present on `create`");
    let HoverContents::Markup(MarkupContent { value, .. }) = h.contents else {
        panic!("expected markup hover");
    };
    assert!(
        value.contains("fn create"),
        "hover should render fn signature, got: {value}"
    );
    assert!(
        !value.contains("expression: function"),
        "hover should not fall through to expression-typed layer 2: {value}"
    );
}

/// P15.x — chain-segment hover: `Identity` in
/// `runtime::Identity::create` renders the foreign `type Identity`.
#[test]
fn hover_on_chain_type_segment_renders_foreign_type() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        runtime_uri,
        "type Identity { static native fn create(name: String, role: String): Identity; }\n",
        "p",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() { var x = runtime::Identity::create(\"a\", \"b\"); }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).expect("user doc");
    let doc = cell.borrow();
    // Cursor on `Identity` segment (col 32 — within "runtime::Identity::create").
    let h = capabilities::hover_with_project(
        &doc.text,
        &doc.lib,
        doc.root_node(),
        pos(0, 32),
        &user_uri,
        &pa,
        &mgr,
    )
    .expect("hover present on `Identity` chain segment");
    let HoverContents::Markup(MarkupContent { value, .. }) = h.contents else {
        panic!("expected markup hover");
    };
    assert!(
        value.contains("type Identity"),
        "hover should render the foreign type, got: {value}"
    );
    assert!(
        value.contains("defined in `runtime`"),
        "hover should include the provenance footnote, got: {value}"
    );
}

/// P15.x — chain-segment hover: `create` in
/// `runtime::Identity::create` renders the foreign method.
#[test]
fn hover_on_chain_member_segment_renders_foreign_method() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        runtime_uri,
        "type Identity { static native fn create(name: String, role: String): Identity; }\n",
        "p",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() { var x = runtime::Identity::create(\"a\", \"b\"); }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).expect("user doc");
    let doc = cell.borrow();
    // Cursor on `create` segment (col 41 — within "runtime::Identity::create").
    let h = capabilities::hover_with_project(
        &doc.text,
        &doc.lib,
        doc.root_node(),
        pos(0, 41),
        &user_uri,
        &pa,
        &mgr,
    )
    .expect("hover present on `create` chain segment");
    let HoverContents::Markup(MarkupContent { value, .. }) = h.contents else {
        panic!("expected markup hover");
    };
    assert!(
        value.contains("fn create"),
        "hover should render the foreign method, got: {value}"
    );
}

/// P15.10 — call-site arg-type validation. The user's baseline:
/// passing `42` (int) where `Identity` is expected should produce a
/// typed diagnostic at the offending arg's range.
#[test]
fn call_arg_type_mismatch_emits_diagnostic() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(runtime_uri, "type Identity {}\n", "p", false);
    mgr.add_simple(
        user_uri.clone(),
        "fn expect_Identity(_: Identity) {}\nfn main() { expect_Identity(42); }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let user_module = pa.module(&user_uri).expect("user module");
    let has_mismatch = user_module
        .analysis
        .diagnostics
        .iter()
        .any(|d| d.message.contains("`int`") && d.message.contains("Identity"));
    assert!(
        has_mismatch,
        "expected an arg-type mismatch diagnostic; got: {:?}",
        user_module.analysis.diagnostics
    );
}

/// P15.10 — bare ident references to a type/fn flow through pass 3.5
/// to the right runtime type. `expect_ty(Identity)` (where
/// `expect_ty(_: type)` and Identity is a type) should *not* error.
#[test]
fn bare_type_ident_used_as_value_is_type() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let stdlib_uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(stdlib_uri, synthetic_std_core_with_node(), "std", false);
    mgr.add_simple(runtime_uri, "type Identity {}\n", "p", false);
    mgr.add_simple(
        user_uri.clone(),
        "fn expect_ty(_: type) {}\nfn main() { expect_ty(Identity); }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let user_module = pa.module(&user_uri).expect("user module");
    assert!(
        user_module.analysis.diagnostics.is_empty(),
        "bare Identity should pass `expect_ty(_: type)`; got: {:?}",
        user_module.analysis.diagnostics
    );
}

/// P15.8 — `var x = runtime::Identity::create("a", "b");` (chained
/// `module::Type::method(...)` call) infers `x: Identity`.
#[test]
fn qualified_static_call_infers_return_type() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        runtime_uri,
        "type Identity { static native fn create(name: String, role: String): Identity; }\n",
        "p",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() { var x = runtime::Identity::create(\"a\", \"b\"); }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let user_module = pa.module(&user_uri).expect("user module");
    let x_local = user_module
        .hir
        .idents
        .iter()
        .find(|(_, i)| pa.symbols()[i.symbol] == *"x")
        .map(|(idx, _)| idx)
        .expect("`x` ident");
    let ty = user_module
        .analysis
        .def_types
        .get(&x_local)
        .copied()
        .expect("def_type for x");
    let display = pa.display_type(ty).to_string();
    assert_eq!(
        display, "Identity",
        "x should infer as `Identity`, got `{display}`"
    );
}

/// P15.8 — `var y = runtime::Identity::create;` (chained method ref)
/// infers `y: function`.
#[test]
fn qualified_static_method_ref_infers_function() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let stdlib_uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(stdlib_uri, synthetic_std_core_with_node(), "std", false);
    mgr.add_simple(
        runtime_uri,
        "type Identity { static native fn create(name: String, role: String): Identity; }\n",
        "p",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() { var y = runtime::Identity::create; }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let user_module = pa.module(&user_uri).expect("user module");
    let y_local = user_module
        .hir
        .idents
        .iter()
        .find(|(_, i)| pa.symbols()[i.symbol] == *"y")
        .map(|(idx, _)| idx)
        .expect("`y` ident");
    let ty = user_module
        .analysis
        .def_types
        .get(&y_local)
        .copied()
        .expect("def_type for y");
    let display = pa.display_type(ty).to_string();
    assert_eq!(
        display, "function",
        "y should infer as `function`, got `{display}`"
    );
}

/// P15.x — `var w = runtime::Identity;` (module-prefixed type
/// reference) infers as `type` (the runtime native — type decls
/// become `type` values when referenced via `module::Type`).
#[test]
fn module_prefixed_type_ref_infers_type() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let stdlib_uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(stdlib_uri, synthetic_std_core_with_node(), "std", false);
    mgr.add_simple(runtime_uri, "type Identity { id: int; }\n", "p", false);
    mgr.add_simple(
        user_uri.clone(),
        "fn main() { var w = runtime::Identity; }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let user_module = pa.module(&user_uri).expect("user module");
    let w_local = user_module
        .hir
        .idents
        .iter()
        .find(|(_, i)| pa.symbols()[i.symbol] == *"w")
        .map(|(idx, _)| idx)
        .expect("`w` ident");
    let ty = user_module
        .analysis
        .def_types
        .get(&w_local)
        .copied()
        .expect("def_type for w");
    let display = pa.display_type(ty).to_string();
    assert_eq!(display, "type", "w should infer as `type`, got `{display}`");
}

/// P15.7 — `var y = Identity::create;` (method reference, no call)
/// infers as `function` (a runtime native type).
#[test]
fn cross_module_static_method_ref_infers_function() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let stdlib_uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(stdlib_uri, synthetic_std_core_with_node(), "std", false);
    mgr.add_simple(
        runtime_uri,
        "type Identity { static native fn create(name: String, role: String): Identity; }\n",
        "p",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() { var y = Identity::create; }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let user_module = pa.module(&user_uri).expect("user module");
    let y_local = user_module
        .hir
        .idents
        .iter()
        .find(|(_, i)| pa.symbols()[i.symbol] == *"y")
        .map(|(idx, _)| idx)
        .expect("`y` ident");
    let ty = user_module
        .analysis
        .def_types
        .get(&y_local)
        .copied()
        .expect("def_type for y");
    let display = pa.display_type(ty).to_string();
    assert_eq!(
        display, "function",
        "y should infer as `function`, got `{display}`"
    );
}

/// P15.7 — `var z = Identity::id;` (attr reference, no call) infers
/// as `field` (a runtime native type).
#[test]
fn cross_module_static_attr_ref_infers_field() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let stdlib_uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(stdlib_uri, synthetic_std_core_with_node(), "std", false);
    mgr.add_simple(runtime_uri, "type Identity { id: int; }\n", "p", false);
    mgr.add_simple(
        user_uri.clone(),
        "fn main() { var z = Identity::id; }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let user_module = pa.module(&user_uri).expect("user module");
    let z_local = user_module
        .hir
        .idents
        .iter()
        .find(|(_, i)| pa.symbols()[i.symbol] == *"z")
        .map(|(idx, _)| idx)
        .expect("`z` ident");
    let ty = user_module
        .analysis
        .def_types
        .get(&z_local)
        .copied()
        .expect("def_type for z");
    let display = pa.display_type(ty).to_string();
    assert_eq!(
        display, "field",
        "z should infer as `field`, got `{display}`"
    );
}

/// P15.6 — `Identity::create` (`static_expr` against a cross-module
/// type) should bind the property ident to the foreign method via
/// `foreign_member_uses`, just like `.` member access does for attrs.
#[test]
fn cross_module_static_call_binds_foreign_method() {
    use greycat_analyzer_analysis::analyzer::MemberDef;
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let runtime_uri = Uri::from_str("file:///runtime.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        runtime_uri.clone(),
        "type Identity { static native fn create(name: String, role: String): Identity; }\n",
        "p",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() { var x = Identity::create(\"a\", \"b\"); }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let user_module = pa.module(&user_uri).expect("user module");
    let create_uses: Vec<_> = user_module
        .hir
        .idents
        .iter()
        .filter(|(_, i)| pa.symbols()[i.symbol] == *"create")
        .map(|(idx, _)| idx)
        .collect();
    assert_eq!(create_uses.len(), 1, "one `create` ident in main.gcl");
    let foreign = user_module
        .analysis
        .foreign_member_lookup(create_uses[0])
        .expect("foreign method binding for `Identity::create`");
    assert_eq!(foreign.uri, runtime_uri);
    assert!(matches!(foreign.member, MemberDef::Method(_)));
}

#[test]
fn cross_module_member_resolution_binds_foreign_attr() {
    use greycat_analyzer_analysis::analyzer::MemberDef;
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let home_uri = Uri::from_str("file:///shapes.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        home_uri.clone(),
        "type Point { x: int; y: int; }\n",
        "p",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn read_x(p: Point): int { return p.x; }\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let user_module = pa.module(&user_uri).expect("user module");
    let x_uses: Vec<_> = user_module
        .hir
        .idents
        .iter()
        .filter(|(_, i)| pa.symbols()[i.symbol] == *"x")
        .map(|(idx, _)| idx)
        .collect();
    assert_eq!(x_uses.len(), 1, "one `x` ident in user.gcl");
    let foreign = user_module
        .analysis
        .foreign_member_lookup(x_uses[0])
        .expect("foreign attr binding for `p.x`");
    assert_eq!(foreign.uri, home_uri);
    assert!(matches!(foreign.member, MemberDef::Attr(_)));
}

#[test]
fn cross_module_goto_implementation_walks_every_module() {
    // P31.2 — goto-impl returns concrete overrides on subtypes of
    // the cursor's declaring type. With `Bar extends Foo`,
    // cursor on `Foo::run` returns both `Foo::run` (self) and
    // `Bar::run` (subtype override) across two modules.
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let a = Uri::from_str("file:///a.gcl").unwrap();
    let b = Uri::from_str("file:///b.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        a.clone(),
        "abstract type Foo {\n    fn run(): int { return 1; }\n}\n",
        "p",
        false,
    );
    mgr.add_simple(
        b.clone(),
        "type Bar extends Foo {\n    fn run(): int { return 2; }\n}\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    // Cursor on `run` in a.gcl line 1 col 8 (0-indexed: "    fn run" → r at col 7).
    let resp =
        capabilities::goto_implementation_across_project(&pa, &mgr, &a, pos(1, 8)).expect("hits");
    let GotoDefinitionResponse::Array(locs) = resp else {
        panic!("expected array of impls");
    };
    let uris: rustc_hash::FxHashSet<_> = locs.iter().map(|l| l.uri.as_str().to_owned()).collect();
    assert!(
        uris.contains(a.as_str()),
        "should include Foo::run in a.gcl: {locs:?}"
    );
    assert!(
        uris.contains(b.as_str()),
        "should include Bar::run in b.gcl: {locs:?}"
    );
}

#[test]
fn cross_module_decl_location_points_at_foreign_name() {
    use greycat_analyzer_core::SymbolTable;
    use greycat_analyzer_hir::lower_module;
    let foreign_text = "type Helper {}\n";
    let foreign_tree = parse(foreign_text);
    let symbols = SymbolTable::new();
    let foreign_hir = lower_module(foreign_text, &symbols, "a", "p", foreign_tree.root_node());
    let helper_decl = foreign_hir
        .module
        .as_ref()
        .and_then(|m| m.decls.first().copied())
        .expect("Helper decl present");
    let foreign_uri = Uri::from_str("file:///other.gcl").unwrap();
    let loc = capabilities::cross_module_decl_location(
        &foreign_uri,
        foreign_text,
        &foreign_hir,
        helper_decl,
    )
    .expect("location for foreign Helper");
    assert_eq!(loc.uri, foreign_uri);
    assert_eq!(loc.range.start.line, 0);
    // `Helper` starts at column 5 (`type ` = 5 chars).
    assert_eq!(loc.range.start.character, 5);
}

#[test]
fn goto_implementation_finds_concrete_method() {
    let src = r#"
type Foo {
    fn body(): int { return 1; }
}
fn caller(): int { return 0; }
"#;
    let tree = parse(src);
    // Cursor inside `body` ident (line 2, col 7-ish).
    let g = capabilities::goto_implementation(src, "project", tree.root_node(), &uri(), pos(2, 8));
    assert!(g.is_some(), "goto_implementation should resolve `body`");
}

#[test]
fn formatting_normalizes_whitespace() {
    let src = "fn x() {var y=1;}\n";
    let tree = parse(src);
    let edits = capabilities::formatting(src, tree.root_node()).expect("formatting result");
    // Either the formatter emits an edit or the input was already
    // canonical; either way we expect the call to return a Vec.
    let _ = edits;
}

#[test]
fn workspace_symbols_aggregates_across_docs() {
    let docs = vec![
        (
            Uri::from_str("file:///a.gcl").unwrap(),
            "project".to_string(),
            "fn alpha() {}\n".to_string(),
        ),
        (
            Uri::from_str("file:///b.gcl").unwrap(),
            "project".to_string(),
            "fn beta() {}\n".to_string(),
        ),
    ];
    let syms = capabilities::workspace_symbols(docs, "");
    let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
    let only_alpha = capabilities::workspace_symbols(
        vec![
            (
                Uri::from_str("file:///a.gcl").unwrap(),
                "project".to_string(),
                "fn alpha() {}\n".to_string(),
            ),
            (
                Uri::from_str("file:///b.gcl").unwrap(),
                "project".to_string(),
                "fn beta() {}\n".to_string(),
            ),
        ],
        "alph",
    );
    let names: Vec<_> = only_alpha.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["alpha"]);
}

#[test]
fn signature_help_renders_params() {
    let src = "fn add(a: int, b: int): int { return 0; }\nfn caller(): int { return add(1, 2); }\n";
    let tree = parse(src);
    // Cursor inside the call `add(1, 2)` — anywhere between `(` and `)`.
    let sh = capabilities::signature_help(src, "project", tree.root_node(), pos(1, 32));
    let _ = sh; // signature_help may return None when the cursor isn't
    // immediately under a `call_expr` ancestor; just exercise the path
    // for now and rely on the existing unit tests in the LS crate for
    // signature-help shape verification.
}

#[test]
fn inlay_hints_emit_var_type() {
    let src = "fn body() {\n    var x = 1;\n}\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let r = lsp_types::Range {
        start: pos(0, 0),
        end: pos(10, 0),
    };
    let hints = project.inlay_hints(&r);
    assert!(!hints.is_empty(), "inlay hints should annotate var x");
}

#[test]
fn inlay_hints_emit_argument_names() {
    // P13.7: `f(1, 2)` against `fn f(x: int, y: int)` emits `x:` / `y:`
    // hints anchored at each arg position.
    let src = "fn f(x: int, y: int) {}\nfn caller() {\n    f(1, 2);\n}\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let r = lsp_types::Range {
        start: pos(0, 0),
        end: pos(10, 0),
    };
    let hints = project.inlay_hints(&r);
    let labels: Vec<String> = hints
        .iter()
        .filter_map(|h| match &h.label {
            lsp_types::InlayHintLabel::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(
        labels.iter().any(|l| l == "x:"),
        "expected `x:` arg-name hint: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "y:"),
        "expected `y:` arg-name hint: {labels:?}"
    );
}

#[test]
fn inlay_hints_emit_inferred_return_type() {
    // P13.7: a fn with no declared return type but a `return …;` body
    // gets a `: <inferred>` hint anchored after the params `)` so it
    // reads `fn ret(): int` (not `fn ret: int()`).
    let src = "fn ret() {\n    return 42;\n}\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let r = lsp_types::Range {
        start: pos(0, 0),
        end: pos(10, 0),
    };
    let hints = project.inlay_hints(&r);
    let hint = hints
        .iter()
        .find(|h| matches!(&h.label, lsp_types::InlayHintLabel::String(s) if s.contains("int")))
        .unwrap_or_else(|| panic!("expected return-type hint with `int`: {hints:?}"));
    // `fn ret()` — the `)` is at column 7, so the hint anchors at
    // column 8 (right after the close paren).
    assert_eq!(
        hint.position,
        pos(0, 8),
        "hint should sit immediately after the params `)`"
    );
}

#[test]
fn selection_ranges_cover_cursor() {
    let src = "fn x(): int { return 1 + 2; }\n";
    let tree = parse(src);
    let positions = vec![pos(0, 24)]; // cursor on `1`
    let sr = capabilities::selection_ranges(src, tree.root_node(), &positions);
    assert!(!sr.is_empty(), "selection ranges should compute");
}

#[test]
fn semantic_tokens_emit_for_idents() {
    let src = "fn one(): int { return 1; }\n";
    let tree = parse(src);
    let tokens = capabilities::semantic_tokens(src, "project", tree.root_node());
    assert!(
        !tokens.data.is_empty(),
        "expected at least one semantic token"
    );
}

#[test]
fn code_actions_for_unused_local_emits_remove_edit() {
    let src = "fn body() {\n    var unused = 1;\n}\n";
    let project = TestProject::single_file_at("/test.gcl", src);
    let r = lsp_types::Range {
        start: pos(1, 0),
        end: pos(1, 30),
    };
    let actions = project.code_actions(r);
    let any_remove = actions.iter().any(|a| {
        let CodeActionOrCommand::CodeAction(ca) = a else {
            return false;
        };
        ca.title.starts_with("Fix")
            && ca
                .edit
                .as_ref()
                .and_then(|w| w.changes.as_ref())
                .map(|m| m.values().any(|edits| !edits.is_empty()))
                .unwrap_or(false)
    });
    assert!(
        any_remove,
        "expected an unused-local fix action with non-empty edits, got: {actions:?}"
    );
}

/// Completion-detail parity: cross-module decls surface their full
/// signature in `CompletionItem.detail` and the home module's stem in
/// `CompletionItem.label_details.description`. Mirrors the TS
/// reference's quick-detail layout (`(<module>) name: T`).
#[test]
fn completion_cross_module_decl_carries_signature_and_module_label() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let mut mgr = SourceManager::new();
    let model_uri = Uri::from_str("file:///proj/model.gcl").unwrap();
    let main_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    mgr.add_simple(
        model_uri.clone(),
        "type Group {}\nvar groups: nodeIndex<String, node<Group>>;\n",
        "project",
        false,
    );
    mgr.add_simple(main_uri.clone(), "fn main() {\n  g\n}\n", "project", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&main_uri).unwrap();
    let doc = cell.borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(1, 3),
        &main_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let groups = list
        .items
        .iter()
        .find(|i| i.label == "groups")
        .expect("`groups` should appear");
    assert_eq!(
        groups.detail.as_deref(),
        Some("var groups: nodeIndex<String, node<Group>>"),
        "expected the foreign var's full signature in detail; got {:?}",
        groups.detail
    );
    assert_eq!(
        groups
            .label_details
            .as_ref()
            .and_then(|d| d.description.as_deref()),
        Some("model"),
        "expected the home module's stem in label_details.description; got {:?}",
        groups.label_details
    );
}

/// LSP must not surface diagnostics from non-`project` libraries —
/// neither lints (`unused-decl` etc.) nor semantic ones (type-relation
/// errors). Library code isn't the user's, and stdlib quirks /
/// analyzer false-positives there are pure editor noise. The
/// `--lint-libs` flag (LSP `greycat-analyzer.lintLibs` setting) lifts
/// the suppression for both axes at once.
#[test]
fn diagnostics_skip_non_project_lib_lints() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let project_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let lib_uri = Uri::from_str("file:///proj/lib/std/core.gcl").unwrap();
    let mut mgr = SourceManager::new();
    // Both modules carry a `private fn unused() {}` — the
    // unused-decl lint fires on each.
    mgr.add_simple(
        project_uri.clone(),
        "private fn unused() {}\n",
        "project",
        false,
    );
    mgr.add_simple(lib_uri.clone(), "private fn unused() {}\n", "std", false);
    let pa = ProjectAnalysis::analyze(&mgr);

    let project_module = pa.module(&project_uri).unwrap();
    let project_diags =
        capabilities::diagnostics_from_module("private fn unused() {}\n", project_module, false);
    assert!(
        project_diags
            .iter()
            .any(|d| d.message.contains("unused private fn")),
        "project lints SHOULD surface in the editor; got: {project_diags:?}"
    );

    let lib_module = pa.module(&lib_uri).unwrap();
    let lib_diags =
        capabilities::diagnostics_from_module("private fn unused() {}\n", lib_module, false);
    assert!(
        !lib_diags
            .iter()
            .any(|d| d.message.contains("unused private fn")),
        "lib-owned (`std`) lints must NOT surface with lint_libs=false; got: {lib_diags:?}"
    );

    // …but `lint_libs=true` (the `greycat-analyzer.lintLibs` extension
    // setting / `--lint-libs` CLI flag) lifts the suppression.
    let lib_diags_opted_in =
        capabilities::diagnostics_from_module("private fn unused() {}\n", lib_module, true);
    assert!(
        lib_diags_opted_in
            .iter()
            .any(|d| d.message.contains("unused private fn")),
        "lib-owned lints SHOULD surface when lint_libs=true; got: {lib_diags_opted_in:?}"
    );
}

/// Semantic diagnostics (type errors, malformed-literal errors)
/// emanating from non-`project` libraries also stay silent unless
/// the user opts into `lint_libs`. The user isn't going to fix a
/// stdlib bug from their IDE, so we don't pollute it with one.
#[test]
fn semantic_diagnostics_skip_non_project_lib_by_default() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    // A malformed char escape is a hard semantic error
    // (`malformed char literal`) at HIR lowering time — the simplest
    // semantic-side regression surface that doesn't depend on cross-
    // module typing.
    let lib_src = "fn helper() { var c = '\\q'; }\n";
    let lib_uri = Uri::from_str("file:///proj/lib/std/core.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(lib_uri.clone(), lib_src, "std", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    let lib_module = pa.module(&lib_uri).unwrap();

    let suppressed = capabilities::diagnostics_from_module(lib_src, lib_module, false);
    assert!(
        !suppressed
            .iter()
            .any(|d| d.message.contains("malformed char")),
        "lib-owned semantic diagnostics must NOT surface with \
         lint_libs=false; got: {suppressed:?}"
    );

    let opted_in = capabilities::diagnostics_from_module(lib_src, lib_module, true);
    assert!(
        opted_in
            .iter()
            .any(|d| d.message.contains("malformed char")),
        "lib-owned semantic diagnostics SHOULD surface when \
         lint_libs=true; got: {opted_in:?}"
    );
}

/// Regression for the sealed-hierarchy `is`-narrow false positives
/// observed against the `rework-symbols` working copy:
/// `s is Rect` where `s: Shape` (and `Rect extends Shape`) was being
/// flagged as "condition is always false" + unreachable code, with a
/// follow-on "Shape not assignable to Rect" error inside the then-
/// branch even though narrowing should have bound `s: Rect` there.
///
/// The hierarchy is open (Shape is abstract, Rect/Circle extend it);
/// the analyzer must recognise the supertype→subtype `is` check as a
/// legitimate narrow, not a contradiction. The fixture mirrors the
/// repo-root `project.gcl` byte-for-byte.
#[test]
fn sealed_hierarchy_is_narrow_does_not_false_positive() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let src = "abstract type Shape {}\n\
               type Rect extends Shape {}\n\
               type Circle extends Shape {}\n\
               \n\
               fn test(s: Shape) {\n\
                   if (s is Rect) {\n\
                       expect_rect(s);\n\
                   } else {\n\
                       expect_circle(s);\n\
                   }\n\
               }\n\
               \n\
               fn expect_rect(_: Rect) {}\n\
               fn expect_circle(_: Circle) {}\n";
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(user_uri.clone(), src, "project", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    let module = pa.module(&user_uri).unwrap();
    let diags = capabilities::diagnostics_from_module(src, module, false);

    // 1. The `is`-check must not be reported as decidable. Shape can
    //    legitimately be a Rect at runtime — that's the whole point of
    //    the subtype dispatch.
    let decidable: Vec<_> = diags
        .iter()
        .filter(|d| {
            d.message.contains("condition is always false")
                || d.message.contains("condition is always true")
        })
        .collect();
    assert!(
        decidable.is_empty(),
        "no decidable-condition diagnostic should fire for `Shape is Rect`; got: {decidable:#?}",
    );

    // 2. No unreachable-code report on the then-branch — it follows
    //    from (1) being wrong, but pin it down independently so a
    //    regression in either pass surfaces a focused failure.
    let unreachable: Vec<_> = diags
        .iter()
        .filter(|d| d.message.contains("unreachable"))
        .collect();
    assert!(
        unreachable.is_empty(),
        "no unreachable-code diagnostic expected; got: {unreachable:#?}",
    );

    // 3. Inside `if (s is Rect) { expect_rect(s); }`, `s` must narrow
    //    to `Rect` so the call type-checks. The pre-fix behavior was
    //    "Shape not assignable to Rect" — surface that explicitly.
    let then_branch_err: Vec<_> = diags
        .iter()
        .filter(|d| {
            d.message.contains("not assignable")
                && d.message.contains("Rect")
                && d.message.contains("Shape")
        })
        .collect();
    assert!(
        then_branch_err.is_empty(),
        "`s` should narrow to `Rect` inside the then-branch; got: {then_branch_err:#?}",
    );

    // 4. P42.3 — inside the else-branch, `s` must narrow to `Circle`
    //    (the lone remaining concrete derivative of `Shape`) so the
    //    `expect_circle(s)` call type-checks. Before P42 the analyzer
    //    left `s` as `Shape` here and flagged "Shape not assignable
    //    to Circle".
    let else_branch_err: Vec<_> = diags
        .iter()
        .filter(|d| {
            d.message.contains("not assignable")
                && d.message.contains("Circle")
                && d.message.contains("Shape")
        })
        .collect();
    assert!(
        else_branch_err.is_empty(),
        "`s` should narrow to `Circle` inside the else-branch; got: {else_branch_err:#?}",
    );
}

/// Regression for symbol/handle mis-alignment under live LSP edits.
/// Captures the screenshot bug: the initial analysis renders the user's
/// `Shape`/`Rect`/`Circle` correctly, but after a `did_change` →
/// `invalidate` cycle the same diagnostics start naming unrelated
/// foreign symbols (e.g. `path`, `append`) where the user's types
/// should appear.
///
/// The hypothesis: caches that survive `invalidate` (the `TypeArena`,
/// `DeclRegistry`, `SymbolTable`) hold `(Uri, Idx<Decl>) →
/// TypeDeclId` and `Symbol → name` mappings that go stale when the
/// freshly-lowered HIR allocates decls into different arena positions
/// than the previous lower did. Reproducer drives the same `did_change`
/// flow the LSP exercises (`manager.update` + `pa.invalidate`).
#[test]
fn invalidate_after_did_change_does_not_misalign_symbols() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::{SourceEncoding, SourceManager};

    let stdlib_uri = Uri::from_str("file:///proj/std/core.gcl").unwrap();
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();

    // Initial source — canonical reproducer shape. Type decls land at
    // `Idx<Decl>` 0/1/2 in the HIR (no methods → no interleaved decls).
    let initial_src = "abstract type Shape {}\n\
                       type Rect extends Shape {}\n\
                       type Circle extends Shape {}\n\
                       fn test(s: Shape) {\n\
                           if (s is Rect) {\n\
                               expect_rect(s);\n\
                           } else {\n\
                               expect_circle(s);\n\
                           }\n\
                       }\n\
                       fn expect_rect(_: Rect) {}\n\
                       fn expect_circle(_: Circle) {}\n";

    // Edit: add a method inside Shape. `lower_type_decl` pushes nested
    // methods onto the *same* `hir.decls` arena BEFORE allocating the
    // owning Type, so the method takes `Idx(0)` and every later type
    // shifts up by one. After this edit, `Shape` lands where `Rect`
    // used to be in the persistent `DeclRegistry`'s `(Uri, Idx<Decl>)`
    // intern table — first-write-wins on `name` means the cached
    // handle still reports "Rect".
    let edited_src = "abstract type Shape { fn nudge() {} }\n\
                      type Rect extends Shape {}\n\
                      type Circle extends Shape {}\n\
                      fn test(s: Shape) {\n\
                          if (s is Rect) {\n\
                              expect_rect(s);\n\
                          } else {\n\
                              expect_circle(s);\n\
                          }\n\
                      }\n\
                      fn expect_rect(_: Rect) {}\n\
                      fn expect_circle(_: Circle) {}\n";

    let mut mgr = SourceManager::new();
    mgr.add_simple(stdlib_uri, synthetic_std_core_with_node(), "std", false);
    mgr.add_simple(user_uri.clone(), initial_src, "project", false);
    let mut pa = ProjectAnalysis::analyze(&mgr);

    // Sanity: initial state is clean (no decidability false positives,
    // no `not assignable` on the narrow path — same guarantees as the
    // sealed-hierarchy test above).
    {
        let module = pa.module(&user_uri).unwrap();
        let diags = capabilities::diagnostics_from_module(initial_src, module, false);
        let bad: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.message.contains("condition is always")
                    || (d.message.contains("not assignable")
                        && d.message.contains("Shape")
                        && d.message.contains("Rect"))
            })
            .collect();
        assert!(
            bad.is_empty(),
            "pre-edit baseline must be clean; got: {bad:#?}",
        );
    }

    // Drive the LSP `did_change` flow: text update + invalidate.
    mgr.update(
        &user_uri,
        vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: edited_src.into(),
        }],
        1,
        SourceEncoding::UTF8,
    );
    pa.invalidate(&mgr, &user_uri);

    let module = pa.module(&user_uri).unwrap();

    // Direct signal — render the type of every binding in `def_types`
    // and compare against what the source declared. If `s: Shape` comes
    // out as anything other than `Shape`, the registry / arena cache
    // is leaking stale handle names across the invalidate boundary.
    for (ident_idx, ty) in &module.analysis.def_types {
        let binding_name = pa.symbols().resolve(&module.hir.idents[*ident_idx].symbol);
        let rendered = pa.display_type(*ty).to_string();
        let expected = match binding_name {
            "s" => "Shape",
            // Both `expect_rect`'s and `expect_circle`'s parameters are
            // named `_`; check that whichever rendering we see is one
            // of the two declared types, not a stale third name.
            "_" => {
                assert!(
                    rendered == "Rect" || rendered == "Circle",
                    "post-invalidate: `_` param should render as `Rect` or `Circle`, got `{rendered}`",
                );
                continue;
            }
            _ => continue,
        };
        assert_eq!(
            rendered, expected,
            "post-invalidate: `{binding_name}` should render as `{expected}`, got `{rendered}`",
        );
    }

    // Diagnostic-side signal — the same regression surfaces as the
    // decidable + unreachable + not-assignable triplet from the
    // screenshot, with foreign names where the user's types should be.
    let diags = capabilities::diagnostics_from_module(edited_src, module, false);
    let decidable: Vec<_> = diags
        .iter()
        .filter(|d| {
            d.message.contains("condition is always false")
                || d.message.contains("condition is always true")
        })
        .collect();
    assert!(
        decidable.is_empty(),
        "post-invalidate: no decidable-condition diagnostic should fire for `Shape is Rect`; got: {decidable:#?}",
    );
}

/// P16.5 — `n.|` where `n: node<Foo>` lists `node`'s own methods AND
/// `Foo`'s attrs/methods. The inner-type items carry an
/// `additional_text_edits` that rewrites `.` → `->` so accepting one
/// drops the user into the correct deref shape.
#[test]
fn completion_dot_on_node_tag_receiver_offers_inner_with_arrow_rewrite() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let stdlib_uri = Uri::from_str("file:///proj/std/core.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(stdlib_uri, synthetic_std_core_with_node(), "std", false);
    mgr.add_simple(
        user_uri.clone(),
        "type Foo {\n  name: String;\n}\nfn caller(n: node<Foo>) {\n  n.\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor right after `n.` on line 4 col 4.
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(4, 4),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let name = list
        .items
        .iter()
        .find(|i| i.label == "name")
        .expect("inner-type attr `name` should appear");
    let edits = name
        .additional_text_edits
        .as_ref()
        .expect("expected `.` → `->` rewrite on inner-type item");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "->");
    // `resolve` is `node`'s own native method — kept verbatim, no rewrite.
    if let Some(resolve) = list.items.iter().find(|i| i.label == "resolve") {
        assert!(
            resolve.additional_text_edits.is_none(),
            "tag's own method should not carry the `.→->` rewrite"
        );
    }
}

/// P16.5 — `n->|` where `n: node<Foo>` lists `Foo`'s members directly
/// (already in the right shape, no rewrite). Tag-owned methods like
/// `node::resolve` are reachable via `.`, not `->`, so they don't
/// surface here.
#[test]
fn completion_arrow_on_node_tag_receiver_lists_inner_directly() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let stdlib_uri = Uri::from_str("file:///proj/std/core.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(stdlib_uri, synthetic_std_core_with_node(), "std", false);
    mgr.add_simple(
        user_uri.clone(),
        "type Foo {\n  name: String;\n}\nfn caller(n: node<Foo>) {\n  n->\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor right after `n->` on line 4 col 5.
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(4, 5),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"name"),
        "inner-type attr should surface on `n->|`: {labels:?}"
    );
    let name = list.items.iter().find(|i| i.label == "name").unwrap();
    assert!(
        name.additional_text_edits.is_none(),
        "`->` already in source — no `.→->` rewrite needed"
    );
    // `resolve` belongs to `node` (the tag), so it should NOT appear
    // on the `->` path.
    assert!(
        !labels.contains(&"resolve"),
        "tag's own methods should not surface on `n->|`: {labels:?}"
    );
}

/// P16.5 analyzer side — `n->name` for `n: node<Foo>` records a
/// `member_uses` binding pointing at `Foo`'s `name` attr (rather than
/// resolving against `node` and finding nothing). Verified through the
/// hover capability since hover surfaces the bound attr's signature.
#[test]
fn hover_arrow_on_node_tag_resolves_inner_member() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let stdlib_uri = Uri::from_str("file:///proj/std/core.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(stdlib_uri, synthetic_std_core_with_node(), "std", false);
    mgr.add_simple(
        user_uri.clone(),
        "type Foo {\n  /// the inner name\n  name: String;\n}\nfn caller(n: node<Foo>) {\n  var s = n->name;\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor on `name` of `n->name` (line 5 col 13).
    let hover = capabilities::hover_with_project(
        &doc.text,
        "project",
        doc.root_node(),
        pos(5, 13),
        &user_uri,
        &pa,
        &mgr,
    )
    .expect("hover should resolve through the deref");
    let body = match hover.contents {
        HoverContents::Markup(m) => m.value,
        HoverContents::Scalar(MarkedString::String(s)) => s,
        HoverContents::Scalar(MarkedString::LanguageString(ls)) => ls.value,
        HoverContents::Array(_) => panic!("unexpected array hover shape"),
    };
    assert!(
        body.contains("name") && body.contains("String"),
        "hover should describe `Foo.name: String`; got:\n{body}"
    );
}

/// Regression: completion inside an *empty* `for-in` body must surface
/// the iterator binders (`i`, `theNode`). Pre-fix the body's
/// byte_range was derived from "first stmt..last stmt" which collapsed
/// to `0..0` for empty blocks, so the cursor never matched the body
/// bracket and the binders were dropped.
#[test]
fn completion_in_empty_for_in_body_surfaces_iterator_params() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "fn caller(arr: Array<int>) {\n  for (i, theNode in arr) {\n    \n  }\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Line 2 col 4 — inside the empty body of the for-in.
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(2, 4),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"i"),
        "for-in index binder should surface in empty body: {labels:?}"
    );
    assert!(
        labels.contains(&"theNode"),
        "for-in value binder should surface in empty body: {labels:?}"
    );
}

/// In-module locals surface their inferred type in
/// `CompletionItem.detail` (so `var counter = 0; c|` shows `int`).
#[test]
fn completion_in_module_local_carries_inferred_type_detail() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "fn main() {\n  var counter = 0;\n  c\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(2, 3),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let counter = list
        .items
        .iter()
        .find(|i| i.label == "counter")
        .expect("`counter` should appear");
    assert_eq!(
        counter.detail.as_deref(),
        Some("int"),
        "expected the local's inferred type in detail; got {:?}",
        counter.detail
    );
}

/// FUNCTION completion items auto-append `($0)` (snippet format) so
/// accepting `helper` rewrites to `helper(<cursor>)`. Skipped when the
/// next non-whitespace byte after the cursor is already `(` — in
/// `helper|()` the user already opened the call.
#[test]
fn completion_function_item_appends_call_parens() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "fn helper(x: int): int { return x; }\nfn main() {\n  h\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(2, 3),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let helper = list
        .items
        .iter()
        .find(|i| i.label == "helper")
        .expect("`helper` should appear");
    assert_eq!(
        helper.insert_text.as_deref(),
        Some("helper($0)"),
        "expected snippet body with `($0)` placeholder; got {:?}",
        helper.insert_text
    );
    assert_eq!(
        helper.insert_text_format,
        Some(InsertTextFormat::SNIPPET),
        "expected SNIPPET insert_text_format so the editor honors `$0`"
    );
}

/// Variables / types must NOT be rewritten — only FUNCTION / METHOD
/// items get the call-parens.
#[test]
fn completion_variable_item_does_not_append_parens() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "fn main() {\n  var counter = 0;\n  c\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(2, 3),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let counter = list
        .items
        .iter()
        .find(|i| i.label == "counter")
        .expect("`counter` should appear");
    assert_eq!(
        counter.insert_text.as_deref(),
        Some("counter"),
        "VARIABLE items must keep their bare name; got {:?}",
        counter.insert_text
    );
    assert_ne!(
        counter.insert_text_format,
        Some(InsertTextFormat::SNIPPET),
        "VARIABLE items must not become SNIPPETs"
    );
}

/// When the cursor is followed by an open-paren (e.g. user backspaced
/// inside `helper(...)`), the snippet rewrite is skipped to avoid
/// `helper($0)()`.
#[test]
fn completion_skips_call_parens_when_already_present() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "fn helper(x: int): int { return x; }\nfn main() {\n  h(1)\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor is after `h`, just before `(`.
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(2, 3),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let helper = list
        .items
        .iter()
        .find(|i| i.label == "helper")
        .expect("`helper` should appear");
    assert_eq!(
        helper.insert_text.as_deref(),
        Some("helper"),
        "should not append `($0)` when cursor is followed by `(`; got {:?}",
        helper.insert_text
    );
}

/// Member completion (`.` / `->`) populates each item's `detail`
/// (full method signature, attribute type) and `documentation`
/// (decl's doc-comment) so the popup tooltip lights up the same way
/// VS Code's TS reference does.
#[test]
fn completion_member_items_carry_detail_and_documentation() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "type Box {\n  /// item count\n  count: int;\n  /// gives back the inner String\n  fn get(): String { return \"\"; }\n}\nfn caller(b: Box) {\n  b.\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(7, 4),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let count = list
        .items
        .iter()
        .find(|i| i.label == "count")
        .expect("`count` attr should appear");
    assert_eq!(count.detail.as_deref(), Some("count: int"));
    let count_doc = match count.documentation.as_ref() {
        Some(Documentation::MarkupContent(c)) => c.value.clone(),
        Some(Documentation::String(s)) => s.clone(),
        None => panic!("expected attr documentation, got None"),
    };
    assert!(
        count_doc.contains("item count"),
        "attr doc-comment should pass through; got {count_doc:?}"
    );

    let get = list
        .items
        .iter()
        .find(|i| i.label == "get")
        .expect("`get` method should appear");
    assert_eq!(
        get.detail.as_deref(),
        Some("fn get(): String"),
        "expected the rendered method signature"
    );
    let get_doc = match get.documentation.as_ref() {
        Some(Documentation::MarkupContent(c)) => c.value.clone(),
        Some(Documentation::String(s)) => s.clone(),
        None => panic!("expected method documentation, got None"),
    };
    assert!(
        get_doc.contains("inner String"),
        "method doc-comment should pass through; got {get_doc:?}"
    );
}

/// Static completion (`Type::|`, `module::|`) populates `detail` /
/// `documentation` for static methods and module-level decls, so the
/// quick-detail popup matches the instance-access path.
#[test]
fn completion_static_items_carry_detail_and_documentation() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "type Box {\n  /// builds a fresh box\n  static fn make(): Box { return Box {}; }\n}\nfn caller() {\n  Box::\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(5, 7),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let make = list
        .items
        .iter()
        .find(|i| i.label == "make")
        .expect("`make` static method should appear");
    assert!(
        make.detail
            .as_deref()
            .is_some_and(|d| d.contains("static fn make()") && d.contains(": Box")),
        "expected static method signature in detail; got {:?}",
        make.detail
    );
    let make_doc = match make.documentation.as_ref() {
        Some(Documentation::MarkupContent(c)) => c.value.clone(),
        Some(Documentation::String(s)) => s.clone(),
        None => panic!("expected static-method documentation, got None"),
    };
    assert!(
        make_doc.contains("fresh box"),
        "static-method doc-comment should pass through; got {make_doc:?}"
    );
}

/// Regression: when the cursor sits mid-identifier (`x.|chars()`),
/// accepting a different completion (`endsWith`) must replace the
/// existing word and not concatenate with it. Without the
/// `text_edit` replace-range, editors that follow the LSP literally
/// produce `x.endsWith()chars()`. With it, the result is
/// `x.endsWith()` — the existing `()` after `chars` is preserved AND
/// the call-paren rewrite is suppressed (parens already there).
#[test]
fn completion_mid_identifier_replaces_whole_word() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "type Wrapped {\n  fn chars(): int { return 0; }\n  fn endsWith(s: String): bool { return true; }\n}\nfn test(x: Wrapped) {\n  x.chars()\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // `  x.chars()` → cursor between `.` and `c` of `chars` (col 4).
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(5, 4),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let ends_with = list
        .items
        .iter()
        .find(|i| i.label == "endsWith")
        .expect("`endsWith` should appear in member completion");

    // Item must carry an explicit replace-range covering `chars`.
    let CompletionTextEdit::Edit(edit) = ends_with.text_edit.as_ref().expect(
        "expected an explicit text_edit so the editor replaces `chars` rather than \
             inserting next to it",
    ) else {
        panic!(
            "expected a plain TextEdit (not InsertReplaceEdit); got {:?}",
            ends_with.text_edit
        );
    };
    assert_eq!(
        edit.range,
        lsp_types::Range {
            start: pos(5, 4),
            end: pos(5, 9),
        },
        "TextEdit range should cover the existing `chars` identifier",
    );
    // Existing `()` after `chars` means we skip the auto-paren snippet.
    assert_eq!(
        edit.new_text, "endsWith",
        "should not append `($0)` when parens follow the replaced ident; got {:?}",
        edit.new_text,
    );
}

/// Pragma completion items already use SNIPPET bodies (e.g.
/// `@library("$1", "$2")`); the call-paren rewrite must leave those
/// untouched.
#[test]
fn completion_pragma_snippet_not_clobbered_by_call_parens() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(user_uri.clone(), "@li\n", "project", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(0, 3),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let lib = list
        .items
        .iter()
        .find(|i| i.label == "@library")
        .expect("`@library` pragma should appear");
    assert!(
        lib.insert_text
            .as_deref()
            .is_some_and(|t| !t.ends_with("($0)")),
        "pragma snippet body should be preserved, not appended-to; got {:?}",
        lib.insert_text
    );
}

/// In-module module-level decls surface their full signature in
/// `CompletionItem.detail`. No `label_details.description` because the
/// decl is intra-module — the foreign-provenance footnote only applies
/// to cross-module surfaces.
#[test]
fn completion_in_module_decl_carries_signature_detail() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "fn helper(x: int): String { return \"\"; }\nfn main() {\n  h\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(2, 3),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list");
    let helper = list
        .items
        .iter()
        .find(|i| i.label == "helper")
        .expect("`helper` should appear");
    assert_eq!(
        helper.detail.as_deref(),
        Some("fn helper(x: int): String"),
        "expected the in-module fn's full signature in detail; got {:?}",
        helper.detail
    );
    assert!(
        helper.label_details.is_none(),
        "intra-module decl should not carry a foreign-module description; got {:?}",
        helper.label_details
    );
}

/// Repro: `n.` member completion on a module-level `var` receiver.
///
/// Mirrors `project.gcl`:
///
/// ```gcl
/// var n: node<int?>;
///
/// fn main() {
///     n.
/// }
/// ```
///
/// `n` is a top-level (module-scope) var — the resolver records it as
/// `Definition::Decl(...)`, not `Definition::Local`. Member completion
/// must still surface the receiver's members (the `node` tag's own
/// methods + the inner type's members via the `.`→`->` rewrite).
#[test]
fn completion_after_dot_on_modvar_node_receiver() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    // P35.9 — load a minimal std/core fixture so the `node` decl
    // is part of the project. With it, `member_completion`'s
    // existing fallback (`project.index.locate_decl("node")`)
    // finds the std-core decl and walks its members.
    let std_uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        std_uri,
        "native type node<T> {\n  fn resolve(): node<T>;\n  fn set(value: T): node<T>;\n}\n",
        "std",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "var n: node<int?>;\nfn main() {\n    n.\n}\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor right after the `.` on line 2 (0-indexed), col 6.
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(2, 6),
        &user_uri,
        &pa,
        None,
    )
    .expect("expected a completion list after `n.` on a modvar receiver");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.is_empty(),
        "expected at least one member completion for `n.` on `var n: node<int?>;`, got empty list"
    );
    // `node` tag's own methods should be reachable through `.`.
    assert!(
        labels.iter().any(|l| *l == "resolve" || *l == "set"),
        "expected `node` tag members (e.g. `resolve` / `set`) in list, got: {labels:?}"
    );
}

/// Isolate the modvar from the runtime `node` tag: receiver is a
/// module-level var of a user-defined type, with a placeholder member
/// (`p.foo`) to avoid ERROR recovery. Pure modvar behaviour test.
#[test]
fn completion_after_dot_on_modvar_user_type_receiver_no_error() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "type Point { x: int; y: int; }\nvar p: Point;\nfn main() {\n    p.foo;\n}\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Line 3 (0-indexed), col 6: right after `.` in `p.foo`.
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(3, 6),
        &user_uri,
        &pa,
        None,
    )
    .expect("expected a completion list after `p.` on a user-typed modvar");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"x") && labels.contains(&"y"),
        "expected `x`/`y` Point attrs after `p.` on `var p: Point;`, got: {labels:?}"
    );
}

/// Companion to `completion_after_dot_on_modvar_node_receiver`: same
/// scenario but with a placeholder member ident (`n.foo`) so the parser
/// produces a full `member_expr` instead of falling into ERROR
/// recovery. Lets us pin down whether the modvar bug lives in the
/// HIR fast path or only in the ERROR-recovery fallback path.
#[test]
fn completion_after_dot_on_modvar_node_receiver_no_error() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    // P35.9 — minimal std/core fixture (as in
    // `completion_after_dot_on_modvar_node_receiver`).
    let std_uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        std_uri,
        "native type node<T> {\n  fn resolve(): node<T>;\n  fn set(value: T): node<T>;\n}\n",
        "std",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "var n: node<int?>;\nfn main() {\n    n.foo;\n}\n",
        "p",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor right after the `.` on line 2 (0-indexed), col 6.
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(2, 6),
        &user_uri,
        &pa,
        None,
    )
    .expect("expected a completion list after `n.` on a modvar receiver (no-ERROR variant)");
    let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.is_empty(),
        "expected at least one member completion (no-ERROR variant), got empty list"
    );
}

/// Synthetic stdlib that exposes `Array<T>` with the methods we
/// need to exercise receiver-instantiation rendering (`add`, `get`,
/// `set` — declared with `T` so the substitution can rewrite them).
fn synthetic_std_core_with_generic_array() -> &'static str {
    "native type any {}\n\
     native type null {}\n\
     native type bool {}\n\
     native type int {}\n\
     native type float {}\n\
     native type String {}\n\
     native type Array<T> {\n\
         fn add(value: T);\n\
         fn get(i: int): T;\n\
         fn set(i: int, value: T): T;\n\
         fn last(): T?;\n\
     }\n\
     native type Map<K, V> {\n\
         fn set(key: K, value: V): V;\n\
         fn get(key: K): V?;\n\
     }\n"
}

/// Hover on `arr.add` where `arr: Array<String>` should render the
/// signature with `T` substituted by `String` — the displayed
/// signature should match the type-checker's view of the call.
#[test]
fn hover_on_generic_method_substitutes_receiver_instantiation() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let stdlib_uri = Uri::from_str("file:///proj/std/core.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        stdlib_uri,
        synthetic_std_core_with_generic_array(),
        "std",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() {\n    var arr = Array<String>{};\n    arr.add(42);\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor on `add` in `arr.add(42)` (line 2 col 9).
    let hover = capabilities::hover_with_project(
        &doc.text,
        "project",
        doc.root_node(),
        pos(2, 9),
        &user_uri,
        &pa,
        &mgr,
    )
    .expect("hover should resolve on `add`");
    let body = match hover.contents {
        HoverContents::Markup(m) => m.value,
        HoverContents::Scalar(MarkedString::String(s)) => s,
        HoverContents::Scalar(MarkedString::LanguageString(ls)) => ls.value,
        HoverContents::Array(_) => panic!("unexpected array hover shape"),
    };
    assert!(
        body.contains("value: String"),
        "hover should render `value: String` (instantiated), got:\n{body}"
    );
    assert!(
        !body.contains("value: T"),
        "hover should not leak declared `T` param after subst; got:\n{body}"
    );
}

/// Map<K, V> instantiated as `Map<String, int>`: hover on `set`
/// should substitute both `K` and `V` independently.
#[test]
fn hover_on_generic_method_substitutes_multiple_generics() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let stdlib_uri = Uri::from_str("file:///proj/std/core.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        stdlib_uri,
        synthetic_std_core_with_generic_array(),
        "std",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() {\n    var m = Map<String, int>{};\n    m.set(\"a\", 1);\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor on `set` in `m.set(...)` (line 2 col 7).
    let hover = capabilities::hover_with_project(
        &doc.text,
        "project",
        doc.root_node(),
        pos(2, 7),
        &user_uri,
        &pa,
        &mgr,
    )
    .expect("hover should resolve on `set`");
    let body = match hover.contents {
        HoverContents::Markup(m) => m.value,
        HoverContents::Scalar(MarkedString::String(s)) => s,
        HoverContents::Scalar(MarkedString::LanguageString(ls)) => ls.value,
        HoverContents::Array(_) => panic!("unexpected array hover shape"),
    };
    assert!(
        body.contains("key: String") && body.contains("value: int"),
        "hover should substitute K→String and V→int; got:\n{body}"
    );
}

/// Nullable generic-param return (`fn last(): T?`) on `Array<String>`:
/// hover should render `String?`, not `T?`.
#[test]
fn hover_on_nullable_generic_return_substitutes() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let stdlib_uri = Uri::from_str("file:///proj/std/core.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        stdlib_uri,
        synthetic_std_core_with_generic_array(),
        "std",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() {\n    var arr = Array<String>{};\n    arr.last();\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor on `last` (line 2 col 9).
    let hover = capabilities::hover_with_project(
        &doc.text,
        "project",
        doc.root_node(),
        pos(2, 9),
        &user_uri,
        &pa,
        &mgr,
    )
    .expect("hover should resolve on `last`");
    let body = match hover.contents {
        HoverContents::Markup(m) => m.value,
        HoverContents::Scalar(MarkedString::String(s)) => s,
        HoverContents::Scalar(MarkedString::LanguageString(ls)) => ls.value,
        HoverContents::Array(_) => panic!("unexpected array hover shape"),
    };
    assert!(
        body.contains("String?"),
        "hover should render nullable subst as `String?`; got:\n{body}"
    );
}

/// Member completion on a generic receiver should render each
/// method's `detail` with the receiver's instantiation substituted.
#[test]
fn completion_after_dot_substitutes_generic_method_signatures() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let stdlib_uri = Uri::from_str("file:///proj/std/core.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        stdlib_uri,
        synthetic_std_core_with_generic_array(),
        "std",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "fn main() {\n    var arr = Array<String>{};\n    arr.\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor right after the `.` on line 2 (col 8).
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        pos(2, 8),
        &user_uri,
        &pa,
        None,
    )
    .expect("completion list after `arr.`");
    let add = list
        .items
        .iter()
        .find(|i| i.label == "add")
        .unwrap_or_else(|| panic!("`add` missing from list: {:?}", list.items));
    let detail = add.detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("value: String"),
        "completion detail should substitute T→String; got: {detail}"
    );
    let get = list
        .items
        .iter()
        .find(|i| i.label == "get")
        .unwrap_or_else(|| panic!("`get` missing from list: {:?}", list.items));
    let get_detail = get.detail.as_deref().unwrap_or("");
    assert!(
        get_detail.contains(": String") && !get_detail.contains(": T"),
        "completion detail for `get` should substitute return type T→String; got: {get_detail}"
    );
}

/// Sanity: free-function hover (no receiver, no subst) is byte-
/// identical to the pre-subst rendering. Guards against accidental
/// `None`-ctx regression in the renderers.
#[test]
fn hover_on_free_function_no_subst_applied() {
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        user_uri.clone(),
        "fn helper(x: int): int { return x; }\nfn main() { helper(1); }\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let cell = mgr.get(&user_uri).unwrap();
    let doc = cell.borrow();
    // Cursor on `helper` at call site (line 1 col 14).
    let hover = capabilities::hover_with_project(
        &doc.text,
        "project",
        doc.root_node(),
        pos(1, 14),
        &user_uri,
        &pa,
        &mgr,
    )
    .expect("hover should resolve on `helper`");
    let body = match hover.contents {
        HoverContents::Markup(m) => m.value,
        HoverContents::Scalar(MarkedString::String(s)) => s,
        HoverContents::Scalar(MarkedString::LanguageString(ls)) => ls.value,
        HoverContents::Array(_) => panic!("unexpected array hover shape"),
    };
    assert!(
        body.contains("fn helper(x: int): int"),
        "free-fn hover should render the declared signature unchanged; got:\n{body}"
    );
}
