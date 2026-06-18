use greycat_analyzer_hir::hir::Expr;

use super::{LintCx, LintDiagnostic, LintRule, LintSeverity};

/// Warn when a numeric literal exceeded its representable range at
/// HIR lowering time. Covers `int` / `float` overflow, `float`
/// precision loss, and `duration` / `time` µs saturation — every
/// case that previously surfaced as a built-in semantic warning.
/// Routing through the lint registry lets users `// gcl-lint-off
/// literal-overflow` site-by-site (which the analyzer's diagnostic
/// path couldn't honour).
///
/// Malformed-shape issues (bad char escape, bad ISO-8601) stay on
/// the semantic path — those are hard errors, not warnings, and
/// suppressing them via a lint directive would hide genuine parse
/// failures.
pub struct LiteralOverflow;

impl LintRule for LiteralOverflow {
    fn name(&self) -> &'static str {
        "literal-overflow"
    }

    fn check(&self, cx: &mut LintCx<'_>) {
        use greycat_analyzer_hir::hir::{LiteralExpr, LiteralKind, ParseIssue};
        let mut diags: Vec<LintDiagnostic> = Vec::new();
        for (_, expr) in cx.hir.exprs.iter() {
            let Expr::Literal(LiteralExpr {
                kind,
                parse_issue: Some(issue),
                byte_range,
            }) = expr
            else {
                continue;
            };
            let message = match (kind, issue) {
                (LiteralKind::Int(_), ParseIssue::Overflow) => {
                    "integer literal exceeds `int` range: overflow"
                }
                (LiteralKind::Float(_), ParseIssue::PrecisionLoss) => {
                    "float literal has more significant digits than `float` can represent: \
                     precision lost"
                }
                (LiteralKind::Float(_), ParseIssue::Overflow) => {
                    "float literal exceeds `float` range: value rounded to infinity"
                }
                (LiteralKind::Duration(_), ParseIssue::Overflow) => {
                    "duration literal exceeds the representable `duration` range (µs): \
                     value saturated"
                }
                (LiteralKind::Time(_), ParseIssue::Overflow) => {
                    "time literal exceeds the representable `time` range (µs): value saturated"
                }
                _ => continue,
            };
            diags.push(LintDiagnostic {
                rule: "literal-overflow",
                severity: LintSeverity::Warning,
                message: message.to_string(),
                byte_range: byte_range.clone(),
                tag: None,
            });
        }
        for d in diags {
            cx.emit(d);
        }
    }
}
