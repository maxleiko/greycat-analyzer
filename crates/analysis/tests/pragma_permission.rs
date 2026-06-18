//! `@permission` contract validation (see `analysis::pragmas`).
//!
//! A `@permission("name")` usage on a fn / method must name a
//! permission declared somewhere in the project closure via a
//! top-level `@permission("name", "desc");` pragma — otherwise
//! `greycat build` fails. The analyzer surfaces this as a hard
//! `unknown-permission` error. A permission is also meaningless on a
//! non-`@expose`d function (advisory `permission-without-expose`).

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

fn codes(pa: &ProjectAnalysis, uri: &Uri, sev: Severity) -> Vec<String> {
    pa.module(uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == sev)
        .map(|d| d.code.to_string())
        .collect()
}

#[test]
fn undeclared_permission_is_flagged_declared_is_clean() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/project.gcl",
        "@permission(\"p.read\", \"read things\");\n\
         @expose\n@permission(\"p.read\")\nfn ok() {}\n\
         @expose\n@permission(\"p.write\")\nfn bad() {}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let errors = codes(&pa, &uri, Severity::Error);
    assert_eq!(
        errors.iter().filter(|c| *c == "unknown-permission").count(),
        1,
        "only the undeclared `p.write` should be flagged, not the declared `p.read`. \
         Got: {:#?}",
        pa.module(&uri).unwrap().analysis.diagnostics
    );
}

#[test]
fn declaration_in_another_module_satisfies_a_usage() {
    // Declaration lives in one module, usage in another — the declared
    // set is project-wide, so this is clean.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/perms.gcl",
        "@permission(\"p.admin\", \"admin\");\n",
    );
    let user = add(
        &mut mgr,
        "/proj/project.gcl",
        "@expose\n@permission(\"p.admin\")\nfn run() {}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert!(
        !codes(&pa, &user, Severity::Error).contains(&"unknown-permission".to_string()),
        "a declaration in perms.gcl must satisfy the usage in project.gcl. Got: {:#?}",
        pa.module(&user).unwrap().analysis.diagnostics
    );
}

#[test]
fn permission_without_expose_warns_and_expose_clears_it() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/project.gcl",
        "@permission(\"p.x\", \"x\");\n\
         @permission(\"p.x\")\nfn lonely() {}\n\
         @expose\n@permission(\"p.x\")\nfn exposed() {}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let warnings = codes(&pa, &uri, Severity::Warning);
    assert_eq!(
        warnings
            .iter()
            .filter(|c| *c == "permission-without-expose")
            .count(),
        1,
        "exactly one warning — for `lonely` (no @expose), not `exposed`. Got: {:#?}",
        pa.module(&uri).unwrap().analysis.diagnostics
    );
    // The declared permission itself must not error in either case.
    assert!(!codes(&pa, &uri, Severity::Error).contains(&"unknown-permission".to_string()));
}

#[test]
fn non_string_permission_arg_is_a_type_error() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/project.gcl",
        "@expose\n@permission(42)\nfn run() {}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert!(
        codes(&pa, &uri, Severity::Error).contains(&"pragma-arg-type".to_string()),
        "a non-string `@permission(42)` arg is rejected. Got: {:#?}",
        pa.module(&uri).unwrap().analysis.diagnostics
    );
}

#[test]
fn bare_permission_no_args_warns_no_effect() {
    // `@permission()` builds at the runtime, but names no permission —
    // a no-op. Flag it (`pragma-missing-args`, warning), and don't pile
    // on the expose-warning (there's no permission to be meaningless).
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/project.gcl",
        "@permission()\nfn run() {}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let d = &pa.module(&uri).unwrap().analysis.diagnostics;
    assert!(
        d.iter().any(|d| d.code == "pragma-missing-args"),
        "`@permission()` should warn that it has no effect. Got: {d:#?}"
    );
    assert!(
        !d.iter()
            .any(|d| d.code == "unknown-permission" || d.code == "permission-without-expose"),
        "no unknown-permission / expose-warning for the empty form. Got: {d:#?}"
    );
}
