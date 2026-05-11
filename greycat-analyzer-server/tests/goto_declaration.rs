//! P31.3 — `textDocument/declaration` jumps from a concrete method
//! override to the abstract ancestor that declares it. Inverse of
//! `textDocument/implementation`.

use greycat_analyzer_analysis::project::ProjectAnalysis;
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

fn first_loc(resp: GotoDefinitionResponse) -> Location {
    match resp {
        GotoDefinitionResponse::Scalar(l) => l,
        GotoDefinitionResponse::Array(mut locs) => locs.remove(0),
        GotoDefinitionResponse::Link(mut links) => {
            let l = links.remove(0);
            Location {
                uri: l.target_uri,
                range: l.target_range,
            }
        }
    }
}

#[test]
fn declaration_jumps_from_concrete_override_to_abstract_parent() {
    // `Child::process` overrides abstract `Base::process`. Cursor on
    // the override's name jumps to `Base::process`'s declaration.
    let mut mgr = SourceManager::new();
    let src = "\
abstract type Base {
    abstract fn process(): int;
}
type Child extends Base {
    fn process(): int { return 1; }
}
fn main() {}
";
    let uri = add(&mut mgr, "/proj/src/mod.gcl", src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor_pos = position_of(src, "process(): int { return 1");
    let resp = capabilities::goto_declaration_across_project(&pa, &mgr, &uri, cursor_pos)
        .expect("goto-decl returned a response");
    let loc = first_loc(resp);
    assert_eq!(loc.uri, uri);
    // `abstract fn process(): int;` is on line 1.
    assert_eq!(loc.range.start.line, 1);
}

#[test]
fn declaration_works_across_modules() {
    // Cross-module shape: base.gcl declares the abstract, child.gcl
    // overrides. Cursor on child.gcl's override jumps into base.gcl.
    let mut mgr = SourceManager::new();
    let base_src = "\
abstract type Base {
    abstract fn run();
}
";
    let child_src = "\
type Child extends Base {
    fn run() {}
}
";
    let base_uri = add(&mut mgr, "/proj/src/base.gcl", base_src);
    let child_uri = add(&mut mgr, "/proj/src/child.gcl", child_src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor_pos = position_of(child_src, "run() {}");
    let resp = capabilities::goto_declaration_across_project(&pa, &mgr, &child_uri, cursor_pos)
        .expect("goto-decl returned a response");
    let loc = first_loc(resp);
    assert_eq!(loc.uri, base_uri);
    assert_eq!(loc.range.start.line, 1);
}

#[test]
fn declaration_walks_multi_level_chain_to_abstract_root() {
    // Base (abstract `tick`) <- Mid (abstract, also `tick` not
    // overridden) <- Leaf (concrete override). Cursor on Leaf's
    // `tick` jumps to Base, not Mid (Mid doesn't declare an abstract
    // `tick` of its own).
    let mut mgr = SourceManager::new();
    let src = "\
abstract type Base {
    abstract fn tick();
}
abstract type Mid extends Base {}
type Leaf extends Mid {
    fn tick() {}
}
fn main() {}
";
    let uri = add(&mut mgr, "/proj/src/mod.gcl", src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor_pos = position_of(src, "tick() {}");
    let resp = capabilities::goto_declaration_across_project(&pa, &mgr, &uri, cursor_pos)
        .expect("goto-decl returned a response");
    let loc = first_loc(resp);
    // `abstract fn tick();` is on line 1.
    assert_eq!(loc.range.start.line, 1);
}

#[test]
fn declaration_returns_none_when_no_abstract_ancestor_and_on_own_decl() {
    // No abstract parent + cursor on the method's own decl-name —
    // there's no separate "declaration" location distinct from the
    // definition. Return None so the LSP client falls through to
    // textDocument/definition.
    let mut mgr = SourceManager::new();
    let src = "\
type Solo {
    fn run() {}
}
fn main() {}
";
    let uri = add(&mut mgr, "/proj/src/mod.gcl", src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor_pos = position_of(src, "run() {}");
    assert!(
        capabilities::goto_declaration_across_project(&pa, &mgr, &uri, cursor_pos).is_none(),
        "expected None when cursor is on the decl with no abstract ancestor"
    );
}

#[test]
fn declaration_falls_back_to_definition_for_call_with_no_abstract_ancestor() {
    // Cursor on a *call* to a method with no abstract parent — the
    // fallback to goto-definition lands on the method's decl.
    let mut mgr = SourceManager::new();
    let src = "\
type Solo {
    fn run() {}
}
fn caller(s: Solo) {
    s.run();
}
";
    let uri = add(&mut mgr, "/proj/src/mod.gcl", src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor_pos = position_of(src, "run();");
    let resp = capabilities::goto_declaration_across_project(&pa, &mgr, &uri, cursor_pos)
        .expect("goto-decl returned a response");
    let loc = first_loc(resp);
    assert_eq!(loc.uri, uri);
    // `fn run() {}` is on line 1.
    assert_eq!(loc.range.start.line, 1);
}

#[test]
fn declaration_on_call_site_walks_to_abstract_parent() {
    // Cursor on a *call* `c.process()` where `c: Child` and Child
    // overrides Base's abstract `process`. Goto-declaration walks
    // from Child up to Base.
    let mut mgr = SourceManager::new();
    let src = "\
abstract type Base {
    abstract fn process(): int;
}
type Child extends Base {
    fn process(): int { return 1; }
}
fn use_child(c: Child): int {
    return c.process();
}
";
    let uri = add(&mut mgr, "/proj/src/mod.gcl", src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor_pos = position_of(src, "process();");
    let resp = capabilities::goto_declaration_across_project(&pa, &mgr, &uri, cursor_pos)
        .expect("goto-decl returned a response");
    let loc = first_loc(resp);
    // Abstract decl on line 1.
    assert_eq!(loc.range.start.line, 1);
}
