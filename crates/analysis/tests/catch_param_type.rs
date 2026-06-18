//! The `catch (err)` parameter is always typed as `core::Error`.
//!
//! GreyCat binds the catch parameter to a non-null `core::Error` value.
//! The analyzer records that type in `def_types`, so member access on the
//! parameter resolves against `Error`. The type is anchored by identity
//! (`arena.builtins.error`), so a user-declared local `type Error` cannot
//! shadow it — the catch param stays `core::Error` regardless.

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

/// Synthetic `core.gcl`: the module symbol derives from the file name
/// (`core`), not the `"std"` library arg, so `Error` interns as the
/// `(core, "Error")` identity that `arena.builtins.error` points at.
const STD_CORE: &str = "native type any {}\n\
                        native type String {}\n\
                        type Error { message: String?; }\n";

fn analyze(user_src: &str) -> (ProjectAnalysis, Uri) {
    let mut mgr = SourceManager::new();
    let std_uri = Uri::from_str("file:///std/core.gcl").unwrap();
    mgr.add_simple(std_uri, STD_CORE, "std", false);
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    mgr.add_simple(user_uri.clone(), user_src, "project", false);
    (ProjectAnalysis::analyze(&mgr), user_uri)
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
    let src = "fn f() {\n\
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
    let src = "fn f() {\n\
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

#[test]
fn catch_param_uses_core_error_not_local_shadow() {
    // A local `type Error { foo: int; }` must NOT hijack the catch param:
    // it stays `core::Error` (which has `message`, not `foo`).
    let src = "type Error { foo: int; }\n\
               fn f() {\n\
                   try {\n\
                       throw \"boom\";\n\
                   } catch (err) {\n\
                       var _a = err.foo;\n\
                       var _b = err.message;\n\
                   }\n\
               }\n";
    let (pa, uri) = analyze(src);
    let msgs = error_messages(&pa, &uri);
    assert!(
        msgs.iter().any(|m| m == "type `Error` has no member `foo`"),
        "catch param must be core::Error (no `foo`), got: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("has no member `message`")),
        "core::Error.message must resolve, got: {msgs:?}"
    );
}
