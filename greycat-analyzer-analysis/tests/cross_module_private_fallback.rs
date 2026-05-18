//! Bare-name resolution order: module-local decls (PUBLIC or PRIVATE)
//! always shadow cross-module hits. Matches the GreyCat runtime —
//! re-verified against `greycat run` after the earlier 8.0.291-dev
//! "p9" probe turned out to disagree with the runtime in the wild
//! (a `private type Load` inside an `@include`d module was correctly
//! preferred over a public `type Load` in `project.gcl`).
//!
//! Probe outcomes encoded by the tests below:
//!
//! - p3 (local private, no global same-name) → bare ref binds to local.
//! - p8 (local public + remote public) → local public wins.
//! - p9 (local private + remote public) → LOCAL private wins.
//!
//! See the commentary in `resolver::Cx::record_use` for the lookup
//! ladder itself.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_analysis::resolver::Definition;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_hir::types::{Decl, Expr};
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

/// For each call-expression `name()` in `uri`'s module, return what
/// the resolver bound the callee to.
fn callee_bindings(pa: &ProjectAnalysis, uri: &Uri, name: &str) -> Vec<Definition> {
    let m = pa.module(uri).expect("module");
    let mut hits = Vec::new();
    for (_, expr) in m.hir.exprs.iter() {
        if let Expr::Call(c) = expr
            && let Expr::Ident { name: name_idx, .. } = m.hir.exprs[c.callee].clone()
            && pa.symbol(&m.hir.idents[name_idx].symbol) == name
            && let Some(def) = m.resolutions.lookup(name_idx)
        {
            hits.push(def);
        }
    }
    hits
}

#[test]
fn local_private_resolves_when_no_global_clash() {
    // p3 mirror: a private fn inside its own module is reachable by
    // bare name. The only candidate is the local private; step 3
    // (last-resort) fires.
    let mut mgr = SourceManager::new();
    let module_uri = add(
        &mut mgr,
        "/proj/src/a.gcl",
        "private fn local_only() {}
fn caller() {
    local_only();
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let bindings = callee_bindings(&pa, &module_uri, "local_only");
    assert_eq!(bindings.len(), 1, "expected one binding: {:#?}", bindings);
    match &bindings[0] {
        Definition::Decl(decl_id) => {
            let m = pa.module(&module_uri).unwrap();
            let decl = &m.hir.decls[*decl_id];
            assert!(
                matches!(decl, Decl::Fn(_)),
                "expected fn decl, got {:?}",
                decl
            );
        }
        other => panic!("expected Decl, got {:?}", other),
    }
}

#[test]
fn local_public_shadows_remote_public() {
    // p8 mirror: local public wins over remote public.
    let mut mgr = SourceManager::new();
    let local_uri = add(
        &mut mgr,
        "/proj/src/b.gcl",
        "fn greeting(): String { return \"from-b\"; }
fn caller(): String {
    return greeting();
}
",
    );
    add(
        &mut mgr,
        "/proj/src/a.gcl",
        "fn greeting(): String { return \"from-a\"; }
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let bindings = callee_bindings(&pa, &local_uri, "greeting");
    assert_eq!(bindings.len(), 1, "expected one binding: {:#?}", bindings);
    // The local public binds to `Definition::Decl` (in-module decl
    // index), NOT to `ProjectDecl` (cross-module pointer).
    assert!(
        matches!(bindings[0], Definition::Decl(_)),
        "expected Decl (local public), got {:?}",
        bindings[0]
    );
}

#[test]
fn local_private_shadows_remote_public() {
    // p9 mirror: bare `greeting()` inside `b.gcl` binds to b.gcl's
    // local PRIVATE, not to a.gcl's remote PUBLIC. Module-local
    // always wins over cross-module regardless of visibility.
    let mut mgr = SourceManager::new();
    let local_uri = add(
        &mut mgr,
        "/proj/src/b.gcl",
        "private fn greeting(): String { return \"from-b-private\"; }
fn caller(): String {
    return greeting();
}
",
    );
    add(
        &mut mgr,
        "/proj/src/a.gcl",
        "fn greeting(): String { return \"from-a-public\"; }
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let bindings = callee_bindings(&pa, &local_uri, "greeting");
    assert_eq!(bindings.len(), 1, "expected one binding: {:#?}", bindings);
    // Crucial assertion: the bare `greeting()` inside `b.gcl` binds
    // to b.gcl's local private (`Definition::Decl`), not to the
    // remote public (`Definition::ProjectDecl`).
    assert!(
        matches!(bindings[0], Definition::Decl(_)),
        "expected Decl (local private), got {:?}",
        bindings[0]
    );
}

#[test]
fn two_modules_each_with_same_named_private_each_resolve_locally() {
    // Kopr regression: maps.gcl has `private fn processIndex(...)` and
    // single_line_diagrams.gcl has `private fn processIndex(...)` with
    // a different signature. Each module's bare-name call must bind to
    // its own local private, not to the other module's.
    //
    // Step 3 of `record_use` filters cross-module private candidates,
    // so neither module sees the other's private — the step-4 module-
    // private fallback should answer with the local decl.
    let mut mgr = SourceManager::new();
    let a_uri = add(
        &mut mgr,
        "/proj/src/a.gcl",
        "private fn process(): int { return 1; }\n\
         fn run_a(): int { return process(); }\n",
    );
    let b_uri = add(
        &mut mgr,
        "/proj/src/b.gcl",
        "private fn process(): String { return \"b\"; }\n\
         fn run_b(): String { return process(); }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let a_bindings = callee_bindings(&pa, &a_uri, "process");
    let b_bindings = callee_bindings(&pa, &b_uri, "process");
    assert_eq!(a_bindings.len(), 1, "a: {:#?}", a_bindings);
    assert_eq!(b_bindings.len(), 1, "b: {:#?}", b_bindings);
    assert!(
        matches!(a_bindings[0], Definition::Decl(_)),
        "a's call should resolve to its own local private (Decl), got {:?}",
        a_bindings[0]
    );
    assert!(
        matches!(b_bindings[0], Definition::Decl(_)),
        "b's call should resolve to its own local private (Decl), got {:?}",
        b_bindings[0]
    );
}
