//! Cross-module + inheritance scenarios for
//! `textDocument/definition`. Anchors P31.1: when the receiver type
//! lives in another module and inherits a method from a third
//! module's abstract type, goto-def must land on the abstract
//! declaration site.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_server::capabilities;
use lsp_types::*;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn position_of(src: &str, needle: &str) -> Position {
    let off = src.find(needle).expect("needle present");
    let line = src[..off].matches('\n').count() as u32;
    let col = (off - src[..off].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    Position {
        line,
        character: col,
    }
}

#[test]
fn goto_def_cross_module_inherited_method_lands_on_base_decl() {
    // base.gcl declares the abstract method on `Base`.
    // sub.gcl extends Base; main.gcl calls `s.greet()` on a Sub.
    // Cursor on `greet` in main must jump to base.gcl's `greet`
    // declaration ident.
    let mut mgr = SourceManager::new();
    let base_src = "\
abstract type Base {
    fn greet(): String { return \"hi\"; }
}
";
    let sub_src = "\
type Sub extends Base {}
";
    let main_src = "\
fn use_sub(s: Sub): String {
    return s.greet();
}
";
    let base_uri = add(&mut mgr, "/proj/src/base.gcl", base_src);
    let _sub_uri = add(&mut mgr, "/proj/src/sub.gcl", sub_src);
    let main_uri = add(&mut mgr, "/proj/src/main.gcl", main_src);

    let pa = ProjectAnalysis::analyze(&mgr);
    let cursor_pos = position_of(main_src, "greet()");
    let resp = capabilities::goto_definition_across_project(
        &pa,
        &mgr,
        &main_uri,
        cursor_pos,
        SourceEncoding::UTF8,
    )
    .expect("goto produced a location");
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location, got {resp:?}");
    };
    assert_eq!(loc.uri, base_uri, "expected jump into base.gcl");
    // Line 1 of base.gcl is `    fn greet(): String { ... }`.
    assert_eq!(
        loc.range.start.line, 1,
        "expected `greet` decl on base.gcl line 1, got {:?}",
        loc.range
    );
}

#[test]
fn goto_def_cross_module_inherited_attr_lands_on_base_decl() {
    // Same shape but for an attr: Base has `name: String`, Sub
    // extends Base, main reads `s.name`. The cross-module
    // `foreign_member_uses` path must route to base.gcl's attr.
    let mut mgr = SourceManager::new();
    let base_src = "\
abstract type Base {
    name: String;
}
";
    let sub_src = "\
type Sub extends Base {}
";
    let main_src = "\
fn use_sub(s: Sub): String {
    return s.name;
}
";
    let base_uri = add(&mut mgr, "/proj/src/base.gcl", base_src);
    let _sub_uri = add(&mut mgr, "/proj/src/sub.gcl", sub_src);
    let main_uri = add(&mut mgr, "/proj/src/main.gcl", main_src);

    let pa = ProjectAnalysis::analyze(&mgr);
    let cursor_pos = position_of(main_src, "name;");
    let resp = capabilities::goto_definition_across_project(
        &pa,
        &mgr,
        &main_uri,
        cursor_pos,
        SourceEncoding::UTF8,
    )
    .expect("goto produced a location");
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location, got {resp:?}");
    };
    assert_eq!(loc.uri, base_uri);
    // `name` is on line 1 of base.gcl.
    assert_eq!(loc.range.start.line, 1);
}

#[test]
fn goto_def_in_module_method_still_works() {
    // Regression guard: the cross-module changes must not break
    // the in-module path. Cursor on a same-module helper call
    // jumps to the local decl.
    let mut mgr = SourceManager::new();
    let src = "fn helper(): int { return 1; }\nfn caller(): int { return helper(); }\n";
    let uri = add(&mut mgr, "/proj/src/mod.gcl", src);

    let pa = ProjectAnalysis::analyze(&mgr);
    let cursor_pos = position_of(src, "helper();");
    let resp = capabilities::goto_definition_across_project(
        &pa,
        &mgr,
        &uri,
        cursor_pos,
        SourceEncoding::UTF8,
    )
    .expect("goto produced a location");
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location, got {resp:?}");
    };
    assert_eq!(loc.uri, uri);
    assert_eq!(loc.range.start.line, 0);
}
