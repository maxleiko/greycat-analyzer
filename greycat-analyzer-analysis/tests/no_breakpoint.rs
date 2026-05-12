// P37.7 — `no-breakpoint` advisory lint. Flags every `breakpoint;` in
// committed source (Warning + UNNECESSARY tag so editors dim it), with
// an auto-fix that deletes the statement plus its trailing newline when
// the line is otherwise blank.

use greycat_analyzer_analysis::lint::{DiagTag, LintSeverity};
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_analysis::quickfix;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze(src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///mod.gcl").unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    // `no-breakpoint` is advisory + default-off (P37.7). Tests flip
    // it on explicitly via `enabled_rules` — same surface the CLI's
    // `--enable=<rule>` flag uses.
    let mut pa = ProjectAnalysis::new();
    pa.enabled_rules.insert("no-breakpoint".to_string());
    pa.analyze_staged(&mgr);
    (uri, pa)
}

fn analyze_without_enable(src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///mod.gcl").unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    (uri, ProjectAnalysis::analyze(&mgr))
}

#[test]
fn flags_breakpoint_with_warning_and_unnecessary_tag() {
    let src = "fn f() {\n    breakpoint;\n}\n";
    let (uri, pa) = analyze(src);
    let m = pa.module(&uri).unwrap();
    let hits: Vec<_> = m
        .lints
        .iter()
        .filter(|l| l.rule == "no-breakpoint")
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected one `no-breakpoint`, got {:?}",
        m.lints
    );
    let d = hits[0];
    assert_eq!(d.severity, LintSeverity::Warning);
    assert_eq!(d.tag, Some(DiagTag::Unnecessary));
    // Range covers exactly `breakpoint;` (11 bytes).
    let span = &src[d.byte_range.clone()];
    assert_eq!(span, "breakpoint;");
}

#[test]
fn quickfix_deletes_breakpoint_stmt_and_trailing_newline() {
    let src = "fn f() {\n    breakpoint;\n    return;\n}\n";
    let (uri, pa) = analyze(src);
    let m = pa.module(&uri).unwrap();
    let d = m
        .lints
        .iter()
        .find(|l| l.rule == "no-breakpoint")
        .expect("expected no-breakpoint diagnostic");
    let edits = quickfix::edit_for_diagnostic(src, d.rule, &d.byte_range, &d.message);
    assert_eq!(edits.len(), 1);
    let mut fixed = src.to_string();
    fixed.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
    // The `breakpoint;` line should be gone entirely — no blank line
    // left behind.
    assert_eq!(fixed, "fn f() {\n    return;\n}\n");

    // Re-analyzing the fixed source should produce no `no-breakpoint`.
    let (uri2, pa2) = analyze(&fixed);
    let m2 = pa2.module(&uri2).unwrap();
    assert!(
        !m2.lints.iter().any(|l| l.rule == "no-breakpoint"),
        "post-fix should be clean: {:?}",
        m2.lints
    );
}

#[test]
fn default_off_does_not_emit_without_explicit_enable() {
    // The critical invariant: a vanilla `lint` run produces no
    // `no-breakpoint` warnings, so `lint --fix` cannot silently delete
    // committed `breakpoint;` debug aids. Users opt in via
    // `lint --enable=no-breakpoint`.
    let src = "fn f() {\n    breakpoint;\n    return;\n}\n";
    let (uri, pa) = analyze_without_enable(src);
    let m = pa.module(&uri).unwrap();
    assert!(
        !m.lints.iter().any(|l| l.rule == "no-breakpoint"),
        "default-off rule must not emit without --enable: {:?}",
        m.lints
    );
}

#[test]
fn suppression_directive_silences_the_warning() {
    let src = "fn f() {\n    // gcl-lint-off no-breakpoint\n    breakpoint;\n}\n";
    let (uri, pa) = analyze(src);
    let m = pa.module(&uri).unwrap();
    assert!(
        !m.lints.iter().any(|l| l.rule == "no-breakpoint"),
        "suppression should silence the warning: {:?}",
        m.lints
    );
}
