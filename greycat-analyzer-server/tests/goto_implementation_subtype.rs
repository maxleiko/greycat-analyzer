//! P31.2 — `textDocument/implementation` filters candidates by
//! subtype relationship to the cursor's declaring type. Drops the
//! pre-P31.2 false positives where unrelated types coincidentally
//! shared a method name with the cursor.

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

fn locations(resp: GotoDefinitionResponse) -> Vec<Location> {
    match resp {
        GotoDefinitionResponse::Array(locs) => locs,
        GotoDefinitionResponse::Scalar(l) => vec![l],
        GotoDefinitionResponse::Link(links) => links
            .into_iter()
            .map(|l| Location {
                uri: l.target_uri,
                range: l.target_range,
            })
            .collect(),
    }
}

#[test]
fn goto_impl_on_abstract_method_returns_only_subtype_overrides() {
    // Base has abstract `process()`. Child extends Base, overrides it.
    // Bystander is unrelated but has a same-named method. Cursor on
    // Base's `process` declaration must return Child::process only —
    // NOT Bystander::process.
    let mut mgr = SourceManager::new();
    let src = "\
abstract type Base {
    abstract fn process(): int;
}
type Child extends Base {
    fn process(): int { return 1; }
}
type Bystander {
    fn process(): int { return 99; }
}
fn main() {}
";
    let uri = add(&mut mgr, "/proj/src/mod.gcl", src);
    let pa = ProjectAnalysis::analyze(&mgr);

    // Cursor on `process` in `abstract fn process(): int;` (Base's decl).
    let cursor_pos = position_of(src, "process(): int;");
    let resp = capabilities::goto_implementation_across_project(&pa, &mgr, &uri, cursor_pos)
        .expect("goto-impl returned a response");
    let locs = locations(resp);
    assert_eq!(
        locs.len(),
        1,
        "expected exactly one implementation (Child::process), got {locs:?}"
    );
    // Child::process is on line 4 (0-indexed) of the source.
    assert_eq!(locs[0].range.start.line, 4);
}

#[test]
fn goto_impl_on_call_site_returns_subtype_overrides() {
    // Cursor on `obj.process()` call where `obj: Base`. Returns the
    // single concrete override on Child.
    let mut mgr = SourceManager::new();
    let src = "\
abstract type Base {
    abstract fn process(): int;
}
type Child extends Base {
    fn process(): int { return 1; }
}
type Bystander {
    fn process(): int { return 99; }
}
fn call(b: Base): int {
    return b.process();
}
";
    let uri = add(&mut mgr, "/proj/src/mod.gcl", src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor_pos = position_of(src, "process();");
    let resp = capabilities::goto_implementation_across_project(&pa, &mgr, &uri, cursor_pos)
        .expect("goto-impl returned a response");
    let locs = locations(resp);
    assert_eq!(
        locs.len(),
        1,
        "expected exactly one implementation (Child::process), got {locs:?}"
    );
}

#[test]
fn goto_impl_across_modules_filters_by_subtype() {
    // Three modules: base.gcl declares the abstract; child.gcl
    // implements; bystander.gcl has an unrelated method of the same
    // name. Cursor on the abstract in base.gcl returns only the
    // child's implementation across modules.
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
    let bystander_src = "\
type Bystander {
    fn run() {}
}
";
    let base_uri = add(&mut mgr, "/proj/src/base.gcl", base_src);
    let child_uri = add(&mut mgr, "/proj/src/child.gcl", child_src);
    let _bystander_uri = add(&mut mgr, "/proj/src/bystander.gcl", bystander_src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor_pos = position_of(base_src, "run();");
    let resp = capabilities::goto_implementation_across_project(&pa, &mgr, &base_uri, cursor_pos)
        .expect("goto-impl returned a response");
    let locs = locations(resp);
    assert_eq!(
        locs.len(),
        1,
        "expected only Child::run from child.gcl, got {locs:?}"
    );
    assert_eq!(locs[0].uri, child_uri);
}

#[test]
fn goto_impl_on_concrete_override_includes_self() {
    // Cursor on `Child::process` (concrete override). The declaring
    // type is Child. Subtypes of Child (just Child itself here) with
    // concrete `process` get returned. Bystander is excluded.
    let mut mgr = SourceManager::new();
    let src = "\
abstract type Base { abstract fn process(): int; }
type Child extends Base {
    fn process(): int { return 1; }
}
type Bystander { fn process(): int { return 99; } }
fn main() {}
";
    let uri = add(&mut mgr, "/proj/src/mod.gcl", src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor_pos = position_of(src, "process(): int { return 1");
    let resp = capabilities::goto_implementation_across_project(&pa, &mgr, &uri, cursor_pos)
        .expect("goto-impl returned a response");
    let locs = locations(resp);
    assert_eq!(locs.len(), 1, "expected Child::process only, got {locs:?}");
}

#[test]
fn goto_impl_on_abstract_type_name_returns_concrete_subtypes() {
    // Cursor on the abstract type's binding-site name (`Shape` in
    // `abstract type Shape`). Returns every concrete subtype in the
    // project — the type-cursor counterpart of the abstract-method
    // case above.
    let mut mgr = SourceManager::new();
    let src = "\
abstract type Shape {}
type Square extends Shape {}
type Circle extends Shape {}
type Bystander {}
";
    let uri = add(&mut mgr, "/proj/src/mod.gcl", src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor_pos = position_of(src, "Shape {}");
    let resp = capabilities::goto_implementation_across_project(&pa, &mgr, &uri, cursor_pos)
        .expect("goto-impl returned a response");
    let locs = locations(resp);
    assert_eq!(
        locs.len(),
        2,
        "expected Square + Circle (Bystander excluded), got {locs:?}"
    );
}

#[test]
fn goto_impl_on_type_use_site_returns_concrete_subtypes() {
    // Cursor on a *use* of the abstract type (`var x: Shape;`).
    // Same answer as cursor-on-binding: concrete subtypes.
    let mut mgr = SourceManager::new();
    let src = "\
abstract type Shape {}
type Square extends Shape {}
type Circle extends Shape {}
fn use_it(s: Shape) {}
";
    let uri = add(&mut mgr, "/proj/src/mod.gcl", src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor_pos = position_of(src, "Shape) {}");
    let resp = capabilities::goto_implementation_across_project(&pa, &mgr, &uri, cursor_pos)
        .expect("goto-impl returned a response");
    let locs = locations(resp);
    assert_eq!(
        locs.len(),
        2,
        "expected Square + Circle from use-site cursor, got {locs:?}"
    );
}

#[test]
fn goto_impl_on_type_name_across_modules() {
    let mut mgr = SourceManager::new();
    let base_src = "abstract type Shape {}\n";
    let sq_src = "type Square extends Shape {}\n";
    let ci_src = "type Circle extends Shape {}\n";
    let base_uri = add(&mut mgr, "/proj/src/base.gcl", base_src);
    let sq_uri = add(&mut mgr, "/proj/src/square.gcl", sq_src);
    let ci_uri = add(&mut mgr, "/proj/src/circle.gcl", ci_src);
    let pa = ProjectAnalysis::analyze(&mgr);

    let cursor_pos = position_of(base_src, "Shape {}");
    let resp = capabilities::goto_implementation_across_project(&pa, &mgr, &base_uri, cursor_pos)
        .expect("goto-impl returned a response");
    let locs = locations(resp);
    assert_eq!(locs.len(), 2, "expected 2 subtype locations: {locs:?}");
    let mut uris: Vec<_> = locs.iter().map(|l| l.uri.clone()).collect();
    uris.sort_by(|a, b| a.path().as_str().cmp(b.path().as_str()));
    assert_eq!(uris, vec![ci_uri, sq_uri]);
}

#[test]
fn goto_impl_no_inheritance_returns_only_self() {
    // Sanity: a standalone type with a method and no children should
    // return just its own definition (cursor on its decl line).
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
    let resp = capabilities::goto_implementation_across_project(&pa, &mgr, &uri, cursor_pos)
        .expect("goto-impl returned a response");
    let locs = locations(resp);
    assert_eq!(locs.len(), 1, "expected Solo::run only, got {locs:?}");
}
