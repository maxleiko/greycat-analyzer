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
fn float_with_many_digits_does_not_flag_precision_loss() {
    // High-precision literals (well past f64's ~16 digit limit) are
    // idiomatic in scientific code — `<math.h>` ships `M_E`, `M_PI`,
    // etc. as 20-digit constants for `long double` compatibility, and
    // users routinely paste them into `.gcl`. The lowering used to
    // flag these as precision-lost; the heuristic was implementation-
    // detail noise (tied to the prior in-house parser's u64 overflow
    // point) and didn't match what the runtime / mainstream compilers
    // do. The parsed value is the correctly-rounded f64 the runtime
    // produces, so there's nothing actionable about the extra digits.
    let src = "fn f() {\n    var x = 2.7182818284590452354_f;\n    \
               var y = 3.14159265358979323846_f;\n    \
               var z = 1.234567890123456789012345_f;\n}\n";
    let (uri, pa) = analyze(src);
    let m = pa.module(&uri).unwrap();
    let hits: Vec<_> = m
        .lints
        .iter()
        .filter(|l| l.rule == "literal-overflow")
        .collect();
    assert!(
        hits.is_empty(),
        "high-digit float literals must not flag literal-overflow; got: {hits:?}"
    );
}

#[test]
fn f64_max_canonical_decimal_does_not_overflow() {
    // `f64::MAX` round-trips through Rust's `{:e}` as
    // `1.7976931348623157e308`; the literal MUST parse to exactly
    // `f64::MAX` rather than `+∞`. The prior in-house parser
    // accumulated rounding error in `(mantissa as f64) * 10.powi(exp)`
    // and overshot `f64::MAX`, producing a spurious literal-overflow
    // lint at both bounds.
    let src = "fn f() {\n    -1.7976931348623157e+308_f;\n    1.7976931348623157e+308_f;\n}\n";
    let (uri, pa) = analyze(src);
    let m = pa.module(&uri).unwrap();
    let hits: Vec<_> = m
        .lints
        .iter()
        .filter(|l| l.rule == "literal-overflow")
        .collect();
    assert!(
        hits.is_empty(),
        "f64::MAX's canonical decimal must not flag literal-overflow; got: {hits:?}"
    );
}

#[test]
fn float_just_past_f64_max_still_overflows() {
    // One step past `f64::MAX`'s exponent — must still flag as
    // overflow so the rounding-bound fix doesn't accidentally widen
    // the accepted range.
    let src = "fn f() { var x = 1.0e+309_f; }\n";
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
        "literal beyond f64::MAX must still flag overflow, got: {:?}",
        m.lints
    );
    assert!(hits[0].message.contains("rounded to infinity"));
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
