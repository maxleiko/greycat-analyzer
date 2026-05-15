//! Anchors the `literal-overflow` lint rule: numeric literals that
//! exceed their type's representable range (int / float / duration /
//! time) or lose float precision now surface as a suppressible lint
//! rather than a built-in semantic warning the user can't silence.

use greycat_analyzer_analysis::lint::LintSeverity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze(src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///mod.gcl").unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    (uri, ProjectAnalysis::analyze(&mgr))
}

#[test]
fn int_overflow_surfaces_as_lint_not_semantic() {
    let src = "fn f() { var x = 99999999999999999999; }\n";
    let (uri, pa) = analyze(src);
    let m = pa.module(&uri).unwrap();

    let lint_hits: Vec<_> = m
        .lints
        .iter()
        .filter(|l| l.rule == "literal-overflow")
        .collect();
    assert_eq!(
        lint_hits.len(),
        1,
        "expected one literal-overflow lint, got {:?}",
        m.lints
    );
    assert_eq!(lint_hits[0].severity, LintSeverity::Warning);
    assert!(lint_hits[0].message.contains("integer literal"));

    let semantic_hits: Vec<_> = m
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("integer literal"))
        .collect();
    assert!(
        semantic_hits.is_empty(),
        "integer overflow must not leak into the semantic diagnostic stream — \
         that path can't be suppressed by `// gcl-lint-off`. got: {semantic_hits:?}"
    );
}

#[test]
fn gcl_lint_off_suppresses_literal_overflow() {
    let src = "\
fn f() {
    // gcl-lint-next-off literal-overflow
    var x = 99999999999999999999;
}
";
    let (uri, pa) = analyze(src);
    let m = pa.module(&uri).unwrap();
    let hits: Vec<_> = m
        .lints
        .iter()
        .filter(|l| l.rule == "literal-overflow")
        .collect();
    assert!(
        hits.is_empty(),
        "`// gcl-lint-next-off literal-overflow` should silence the diagnostic; \
         got: {hits:?}"
    );
}

#[test]
fn float_precision_loss_surfaces_as_lint() {
    // 25 significant digits — overflows the u64 mantissa the lowering
    // accumulates, which is the trigger for PrecisionLoss.
    let src = "fn f() { var x = 1.234567890123456789012345; }\n";
    let (uri, pa) = analyze(src);
    let m = pa.module(&uri).unwrap();
    let hits: Vec<_> = m
        .lints
        .iter()
        .filter(|l| l.rule == "literal-overflow")
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected one literal-overflow lint for the float, got {:?}",
        m.lints
    );
    assert!(hits[0].message.contains("float literal"));
}

#[test]
fn malformed_iso8601_stays_a_semantic_error() {
    // Malformed-shape parse failures are hard errors — they must not
    // be routed through the lint surface (where users could
    // accidentally suppress a genuine parse failure).
    let src = "fn f() { var x = 2024-99-99T00:00:00Z; }\n";
    let (uri, pa) = analyze(src);
    let m = pa.module(&uri).unwrap();
    let lint_hits: Vec<_> = m
        .lints
        .iter()
        .filter(|l| l.rule == "literal-overflow")
        .collect();
    assert!(
        lint_hits.is_empty(),
        "malformed-shape parse issues belong on the semantic path, \
         not on `literal-overflow`. got: {lint_hits:?}"
    );
}
