//! Inheritance-aware completion regressions.
//!
//! B1 — object-field completion must offer inherited fields even when
//! the supertype lives in another module (`@library` / `@include`) or
//! is written with a qualifier; the walk keys off the project
//! `type_members` supertype chain, not a module-local name re-resolve.
//!
//! B2 — member completion (`.` / `->`) must offer inherited members,
//! including for a same-module `extends` (previously there was no
//! supertype walk at all).

mod support;

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_server::capabilities;
use lsp_types::*;
use std::str::FromStr;

use support::{TEST_ENCODING, TestProject};

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn position_of(src: &str, needle: &str, after: usize) -> Position {
    let off = src.find(needle).expect("needle present") + after;
    let line = src[..off].matches('\n').count() as u32;
    let col = (off - src[..off].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    Position {
        line,
        character: col,
    }
}

// ---- B2: same-module inheritance, member completion ------------------------

#[test]
fn member_completion_lists_same_module_inherited_members() {
    let src = "\
type Base {
    base_attr: String;
    fn base_method(): int { return 1; }
}
type Sub extends Base {
    sub_attr: int;
}
fn use_it(s: Sub): int {
    return s.;
}
";
    let cursor = position_of(src, "return s.", "return s.".len());
    let p = TestProject::single_file(src);
    let list = p.completion(cursor).expect("member completion at `s.`");
    let labels: Vec<&str> = list.items.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"sub_attr"),
        "own attr `sub_attr` missing, got {labels:?}"
    );
    assert!(
        labels.contains(&"base_attr"),
        "inherited attr `base_attr` missing, got {labels:?}"
    );
    assert!(
        labels.contains(&"base_method"),
        "inherited method `base_method` missing, got {labels:?}"
    );
}

#[test]
fn member_completion_walks_multi_level_chain_and_dedups_overrides() {
    // Grandparent → Parent → Child, with `Child` overriding `tag()`.
    // The walk must reach the grandparent (`gp_attr`) and emit the
    // overridden `tag` exactly once (the child's signature wins).
    let src = "\
type Gp {
    gp_attr: String;
    fn tag(): String { return \"gp\"; }
}
type Parent extends Gp {
    p_attr: int;
}
type Child extends Parent {
    c_attr: bool;
    fn tag(): String { return \"child\"; }
}
fn use_it(c: Child): String {
    return c.;
}
";
    let cursor = position_of(src, "return c.", "return c.".len());
    let p = TestProject::single_file(src);
    let list = p.completion(cursor).expect("member completion at `c.`");
    let labels: Vec<&str> = list.items.iter().map(|c| c.label.as_str()).collect();
    for want in ["c_attr", "p_attr", "gp_attr", "tag"] {
        assert!(labels.contains(&want), "`{want}` missing, got {labels:?}");
    }
    let tag_count = labels.iter().filter(|l| **l == "tag").count();
    assert_eq!(
        tag_count, 1,
        "overridden `tag` must appear once, got {labels:?}"
    );
}

#[test]
fn member_completion_lists_foreign_inherited_members() {
    // B1 ∩ B2: the inherited member lives in another module *and* is
    // reached through `extends`. Member completion must surface it.
    let mut mgr = SourceManager::new();
    let base_src = "\
type Base {
    base_attr: String;
    fn base_method(): int { return 1; }
}
";
    let main_src = "\
type Sub extends Base {
    sub_attr: int;
}
fn use_it(s: Sub): int {
    return s.;
}
";
    let _base_uri = add(&mut mgr, "/proj/src/base.gcl", base_src);
    let main_uri = add(&mut mgr, "/proj/src/main.gcl", main_src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor = position_of(main_src, "return s.", "return s.".len());
    let doc = mgr.get(&main_uri).unwrap().borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        cursor,
        &main_uri,
        &pa,
        None,
        SourceEncoding::UTF8,
    )
    .expect("member completion at `s.`");
    let labels: Vec<&str> = list.items.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"sub_attr"),
        "own attr `sub_attr` missing, got {labels:?}"
    );
    assert!(
        labels.contains(&"base_attr") && labels.contains(&"base_method"),
        "foreign inherited members missing, got {labels:?}"
    );
}

// ---- B1: foreign supertype, object-field completion ------------------------

#[test]
fn object_field_completion_lists_foreign_inherited_fields() {
    let mut mgr = SourceManager::new();
    let base_src = "\
type Base {
    base_field: String;
}
";
    let main_src = "\
type Sub extends Base {
    sub_field: int;
}
fn make(): Sub {
    return Sub { };
}
";
    let _base_uri = add(&mut mgr, "/proj/src/base.gcl", base_src);
    let main_uri = add(&mut mgr, "/proj/src/main.gcl", main_src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor = position_of(main_src, "return Sub { ", "return Sub { ".len());
    let doc = mgr.get(&main_uri).unwrap().borrow();
    let list = capabilities::completion_with_project(
        &doc.text,
        doc.root_node(),
        cursor,
        &main_uri,
        &pa,
        None,
        SourceEncoding::UTF8,
    )
    .expect("object-field completion inside `Sub { }`");
    let labels: Vec<&str> = list.items.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"sub_field"),
        "own field `sub_field` missing, got {labels:?}"
    );
    assert!(
        labels.contains(&"base_field"),
        "foreign inherited field `base_field` missing, got {labels:?}"
    );
}

// Sanity: same-module object-field inheritance already worked; keep it
// green so the B1 rework doesn't regress the local path.
#[test]
fn object_field_completion_lists_same_module_inherited_fields() {
    let src = "\
type Base {
    base_field: String;
}
type Sub extends Base {
    sub_field: int;
}
fn make(): Sub {
    return Sub { };
}
";
    let cursor = position_of(src, "return Sub { ", "return Sub { ".len());
    let p = TestProject::single_file(src);
    let list = p
        .completion(cursor)
        .expect("object-field completion inside `Sub { }`");
    let labels: Vec<&str> = list.items.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"sub_field") && labels.contains(&"base_field"),
        "expected own + inherited fields, got {labels:?}"
    );
    let _ = TEST_ENCODING;
}
