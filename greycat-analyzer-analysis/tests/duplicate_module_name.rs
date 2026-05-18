//! GreyCat requires module names to be unique within a project (every
//! `.gcl` file's filename stem becomes its module symbol, and the
//! analyzer keys per-item project maps on `(module, item)` pairs).
//! When two files happen to share a stem, the first ingested wins and
//! the rest are flagged with `duplicate-module-name` AND excluded from
//! the project closure (no decl ingest, no cross-module exposure).

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

#[test]
fn duplicate_module_is_recorded_for_diagnostic_overlay() {
    let mut mgr = SourceManager::new();
    let first = add(&mut mgr, "/proj/foo.gcl", "type A {}\n");
    let dup = add(&mut mgr, "/proj/sub/foo.gcl", "type B {}\n");
    let pa = ProjectAnalysis::analyze(&mgr);

    let dup_info = pa
        .index
        .duplicate_modules
        .get(&dup)
        .expect("duplicate module recorded");
    let foo_sym = pa.index.symbols.lookup("foo").expect("foo interned");
    assert_eq!(dup_info.0, foo_sym, "module name Symbol captured");
    assert_eq!(
        dup_info.1, first,
        "winner URI recorded for the diagnostic message"
    );
    assert!(
        !pa.index.duplicate_modules.contains_key(&first),
        "first-found is NOT a duplicate"
    );
}

#[test]
fn duplicate_module_decls_are_excluded_from_project_closure() {
    // The duplicate's decls must not leak into the project's per-item
    // maps. If they did, cross-module references would silently resolve
    // through whichever was ingested second.
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/foo.gcl", "type A {}\n");
    add(&mut mgr, "/proj/sub/foo.gcl", "type DupB {}\n");
    let pa = ProjectAnalysis::analyze(&mgr);

    let a_sym = pa.index.symbols.lookup("A").expect("A interned");
    let a_id = pa
        .index
        .item_id_for(&Uri::from_str("file:///proj/foo.gcl").unwrap(), a_sym)
        .expect("A item id");
    assert!(
        pa.index.type_members.contains_key(&a_id),
        "first file's decls land in type_members"
    );

    // The duplicate's type name MUST NOT appear in the project name set
    // (otherwise other files could resolve `DupB` and get something
    // that isn't really in any project module).
    assert!(
        pa.index.symbols.lookup("DupB").is_none()
            || !pa
                .index
                .type_names
                .contains(&pa.index.symbols.lookup("DupB").unwrap()),
        "duplicate file's decls are excluded from project name set"
    );
}

#[test]
fn reingesting_same_uri_for_same_module_name_is_idempotent_not_a_duplicate() {
    // The LSP invalidate cycle re-ingests the same URI on every
    // did_change. That must NOT flag the file as a duplicate of itself.
    let mut mgr = SourceManager::new();
    let uri = add(&mut mgr, "/proj/foo.gcl", "type A {}\n");
    let pa = ProjectAnalysis::analyze(&mgr);
    assert!(!pa.index.duplicate_modules.contains_key(&uri));
}
