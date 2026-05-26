//! A `private` decl in module M is not visible from any other module.
//! In particular, a private type's static attrs / methods must not be
//! reachable cross-module through name-based member lookup. Mirrors
//! the GreyCat runtime: `greycat run` rejects `Load::pv` with
//! "unresolved identifier" when `Load` is the empty public type
//! (declared in `project.gcl`) and `pv` lives on a same-named *private*
//! type in another module.

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn error_codes(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    pa.module(uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.code.to_string())
        .collect()
}

#[test]
fn private_type_static_attrs_are_invisible_cross_module() {
    // Three-module setup mirroring the user-reported scenario:
    //   - project.gcl: public `type Load {}` (no fields).
    //   - foo.gcl   : `private type Load { static pv: String = "pv"; }`.
    //   - load.gcl  : `Importer::foo()` accesses `Load::pv`.
    //
    // `Load` in load.gcl must bind to project.gcl's empty public type
    // (foo.gcl's private is invisible), and `Load::pv` must produce an
    // `unknown-static-member` diagnostic — matching the runtime.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/project.gcl",
        "type Load {}\nfn main() {}\n",
    );
    add(
        &mut mgr,
        "/proj/foo.gcl",
        "private type Load {\n    static pv: String = \"pv\";\n}\n",
    );
    let load_uri = add(
        &mut mgr,
        "/proj/load.gcl",
        "type Importer {\n    static fn foo() {\n        var s = Load::pv;\n    }\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = error_codes(&pa, &load_uri);
    assert!(
        codes.iter().any(|c| c == "unknown-static-member"),
        "expected `unknown-static-member` on `Load::pv` — \
         load.gcl must not be able to see foo.gcl's private static attr. \
         Got diagnostics: {:#?}",
        pa.module(&load_uri).unwrap().analysis.diagnostics
    );
}

#[test]
fn private_type_is_invisible_when_no_public_namesake_exists() {
    // Strict invisibility: if module B has only a private `Helper` and
    // module A asks for `Helper`, the name must be unresolved — not
    // silently bound to B's private. Sister-case of the user's bug:
    // here there is no public namesake to fall back to.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/b.gcl",
        "private type Helper {\n    static tag: String = \"b\";\n}\n",
    );
    let a_uri = add(
        &mut mgr,
        "/proj/a.gcl",
        "fn use_helper(): String {\n    return Helper::tag;\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = error_codes(&pa, &a_uri);
    assert!(
        codes.iter().any(|c| c == "private-cross-module-name"),
        "expected `private-cross-module-name` on `Helper` — \
         b.gcl's private type must not bare-leak into a.gcl, but the \
         resolver should know the FQN `b::Helper` and surface a \
         richer diagnostic than `unresolved-name`. \
         Got diagnostics: {:#?}",
        pa.module(&a_uri).unwrap().analysis.diagnostics
    );
}

#[test]
fn private_type_is_visible_to_its_own_module() {
    // Sanity: the owning module must still see its own private type's
    // members. Same as the user's first example before they split it.
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/lib.gcl",
        "private type Load {\n    static pv: String = \"pv\";\n}\n\
         type Importer {\n    static fn foo() {\n        var s = Load::pv;\n    }\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = error_codes(&pa, &uri);
    assert!(
        codes.is_empty(),
        "same-module private-type access must not error — \
         got diagnostics: {:#?}",
        pa.module(&uri).unwrap().analysis.diagnostics
    );
}
