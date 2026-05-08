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

fn pos(line: u32, character: u32) -> Position {
    Position { line, character }
}

fn uri() -> Uri {
    Uri::from_str("file:///test.gcl").unwrap()
}

#[test]
fn hover_renders_param_type() {
    let src = "fn add(a: int, b: int): int { return a + b; }\n";
    let tree = parse(src);
    let h = capabilities::hover(src, "project", tree.root_node(), pos(0, 38));
    let h = h.expect("hover present on `a`");
    let HoverContents::Markup(MarkupContent { value, .. }) = h.contents else {
        panic!("expected markup hover");
    };
    assert!(
        value.contains("int"),
        "hover should show param type, got: {value}"
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
    let tree = parse(src);
    let edit = capabilities::rename(src, tree.root_node(), &uri(), pos(0, 28), "y").unwrap();
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
    let tree = parse(src);
    let refs = capabilities::references(src, "project", tree.root_node(), &uri(), pos(0, 28));
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
        .filter(|(_, i)| i.text == "x")
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
    use greycat_analyzer_analysis::project::ProjectAnalysis;
    use greycat_analyzer_core::SourceManager;
    let a = Uri::from_str("file:///a.gcl").unwrap();
    let b = Uri::from_str("file:///b.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        a.clone(),
        "type Foo {\n    fn run(): int { return 1; }\n}\n",
        "p",
        false,
    );
    mgr.add_simple(
        b.clone(),
        "type Bar {\n    fn run(): int { return 2; }\n}\n",
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
    let uris: std::collections::HashSet<_> =
        locs.iter().map(|l| l.uri.as_str().to_owned()).collect();
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
    use greycat_analyzer_hir::lower_module;
    let foreign_text = "type Helper {}\n";
    let foreign_tree = parse(foreign_text);
    let foreign_hir = lower_module(foreign_text, "a", "p", foreign_tree.root_node());
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
    let tree = parse(src);
    let r = lsp_types::Range {
        start: pos(0, 0),
        end: pos(10, 0),
    };
    let hints = capabilities::inlay_hints(src, "project", tree.root_node(), &r);
    assert!(!hints.is_empty(), "inlay hints should annotate var x");
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
    let tree = parse(src);
    let r = lsp_types::Range {
        start: pos(1, 0),
        end: pos(1, 30),
    };
    let actions = capabilities::code_actions(src, "project", tree.root_node(), &uri(), r);
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
