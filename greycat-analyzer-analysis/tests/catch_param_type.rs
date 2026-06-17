//! The `catch (err)` parameter is always typed as `Error`.
//!
//! GreyCat binds the catch parameter to a non-null `core::Error` value.
//! The analyzer records that type in `def_types`, so member access on the
//! parameter resolves against `Error` (unknown fields fire `unknown-member`,
//! known fields stay silent). A locally-declared `Error` stands in for the
//! stdlib type here so the fixture stays self-contained.

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze(src: &str) -> (ProjectAnalysis, Uri) {
    let uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(uri.clone(), src, "project", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    (pa, uri)
}

fn error_messages(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    pa.module(uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn catch_param_unknown_member_fires() {
    let src = "type Error { message: String?; }\n\
               fn f() {\n\
                   try {\n\
                       throw \"boom\";\n\
                   } catch (err) {\n\
                       err.nope;\n\
                   }\n\
               }\n";
    let (pa, uri) = analyze(src);
    let msgs = error_messages(&pa, &uri);
    assert!(
        msgs.iter()
            .any(|m| m == "type `Error` has no member `nope`"),
        "catch param should be typed `Error`, got: {msgs:?}"
    );
}

#[test]
fn catch_param_known_member_no_error() {
    let src = "type Error { message: String?; }\n\
               fn f() {\n\
                   try {\n\
                       throw \"boom\";\n\
                   } catch (err) {\n\
                       var _ = err.message;\n\
                   }\n\
               }\n";
    let (pa, uri) = analyze(src);
    let msgs = error_messages(&pa, &uri);
    assert!(
        !msgs.iter().any(|m| m.contains("has no member")),
        "known member on `Error` catch param should resolve, got: {msgs:?}"
    );
}
