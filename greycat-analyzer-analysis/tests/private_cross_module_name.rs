//! `private-cross-module-name` — bare reference to a private decl that
//! lives in a foreign module. The runtime rejects the bare form
//! ("unresolved identifier") but the project closure does contain a
//! decl by that name, reachable through its FQN. The analyzer
//! supersedes the generic `unresolved-name` with a richer diagnostic
//! that names the home module and suggests `module::Name`, and an
//! `ide::quickfix::edit_for_diagnostic` rewrites the bare ident to the
//! FQN.
//!
//! Companion to `cross_module_private_visibility.rs` — that file
//! covers same-module access and the strict-invisibility shape; this
//! file covers the richer-diagnostic + quickfix path.

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::ide::quickfix::{QuickfixCx, edit_for_diagnostic};
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
fn bare_private_cross_module_ref_gets_richer_diagnostic() {
    // Reproducer from the bug report (collapsed to user-declared types
    // so the test doesn't depend on `Map` / `Array` from the stdlib):
    //   - foo.gcl declares `private type Element {}` plus a fn taking
    //     a parameter of that type.
    //   - project.gcl writes `foo(Element {})` — bare `Element` is not
    //     reachable because `Element` is private to `foo`.
    //
    // The diagnostic must:
    //   1. Carry code `private-cross-module-name`, not `unresolved-name`.
    //   2. Mention the home module (`foo`) and the FQN (`foo::Element`).
    //   3. Point at the bare `Element` byte range so editors can apply
    //      the quickfix in place.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/foo.gcl",
        "private type Element {}\nfn foo(_: Element) {}\n",
    );
    let project_src = "fn main() {\n    foo(Element {});\n}\n";
    let project_uri = add(&mut mgr, "/proj/project.gcl", project_src);

    let pa = ProjectAnalysis::analyze(&mgr);
    let module = pa.module(&project_uri).expect("project module");
    let diag = module
        .analysis
        .diagnostics
        .iter()
        .find(|d| d.code == "private-cross-module-name")
        .unwrap_or_else(|| {
            panic!(
                "expected one `private-cross-module-name` diagnostic. Got: {:#?}",
                module.analysis.diagnostics
            );
        });
    assert_eq!(diag.severity, Severity::Error);
    assert!(
        diag.message.contains("`foo::Element`"),
        "diagnostic message must contain the FQN `foo::Element` (the \
         quickfix scrapes it from there). Got: {}",
        diag.message
    );
    assert!(
        diag.message.contains("`foo`"),
        "diagnostic message must name the home module `foo`. Got: {}",
        diag.message
    );

    // Mutual exclusion with `unresolved-name`: the supersession in
    // `analyzer.rs` skips the generic diagnostic when the ident
    // appears in `private_cross_module`.
    let unresolved_at_same_range = module
        .analysis
        .diagnostics
        .iter()
        .any(|d| d.code == "unresolved-name" && d.byte_range == diag.byte_range);
    assert!(
        !unresolved_at_same_range,
        "must not also emit `unresolved-name` on the same ident — \
         the richer diagnostic supersedes it."
    );
}

#[test]
fn quickfix_rewrites_bare_ident_to_fqn() {
    // The `edit_for_diagnostic` dispatch arm for
    // `private-cross-module-name` scrapes the FQN from the last
    // backtick pair in the message and replaces the bare ident's byte
    // range with it. Both the LSP `code_actions` handler and the CLI's
    // `lint --fix` go through the same dispatch, so this single test
    // covers both surfaces.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/foo.gcl",
        "private type Element {}\nfn foo(_: Element) {}\n",
    );
    let project_src = "fn main() {\n    foo(Element {});\n}\n";
    let project_uri = add(&mut mgr, "/proj/project.gcl", project_src);

    let pa = ProjectAnalysis::analyze(&mgr);
    let module = pa.module(&project_uri).expect("project module");
    let diag = module
        .analysis
        .diagnostics
        .iter()
        .find(|d| d.code == "private-cross-module-name")
        .expect("diagnostic present");

    // Drive the dispatch the same way `code_actions.rs` and `cmd/lint.rs`
    // do — through `edit_for_diagnostic`.
    let doc = mgr.get(&project_uri).unwrap();
    let doc_b = doc.borrow();
    let tree = doc_b.tree.clone();
    let root = tree.root_node();
    let cx = QuickfixCx {
        root,
        text: project_src,
        hir: Some(&module.hir),
        symbols: Some(&pa.index.symbols),
    };
    let edits = edit_for_diagnostic(&cx, diag.code, &diag.byte_range, &diag.message);
    assert_eq!(
        edits.len(),
        1,
        "expected exactly one TextEdit, got {edits:?}"
    );
    let edit = &edits[0];
    assert_eq!(
        edit.byte_range, diag.byte_range,
        "the edit must replace the bare-ident span pinpointed by the diagnostic",
    );
    assert_eq!(
        edit.new_text, "foo::Element",
        "the edit's replacement text must be the FQN scraped from the message",
    );
}
