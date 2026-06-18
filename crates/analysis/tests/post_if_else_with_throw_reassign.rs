//! Regression: after an `if (x != null) { throw; } else { x = ...; }`,
//! the post-if-else narrow for `x` should be the join of the two
//! branches — and since the `if` branch throws (bottom), the join is
//! just the else branch's narrow. The else branch reassigns `x` to
//! a non-null value, so post-if-else `x` must be non-null, and
//! `return x!!` must satisfy the declared return type.
//!
//! Kopr smoking gun (paraphrased):
//!
//! ```text
//! static fn createTag(name: String): node<Tag> {
//!     var tag = byName.get(name);            // node<Tag>?
//!     if (tag != null) {
//!         throw "...";
//!     } else {
//!         tag = node<Tag> { Tag { ... } };   // reassign non-null
//!     }
//!     return tag!!;
//! }
//! ```
//!
//! Before the fix, the analyzer flagged `return tag!!;` with
//! "return value of type `null` is not assignable to declared return
//! type `node<Tag>`" — i.e. the post-if-else narrow stayed pinned at
//! the if-branch's `null` shape and the else branch's reassignment
//! never lifted into the join.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn assignability_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("not assignable"))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn else_reassignment_lifts_after_throwing_if_branch() {
    let mut mgr = SourceManager::new();
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "type Tag { name: String; }\n\
         fn lookup(name: String): Tag? { return null; }\n\
         fn make(name: String): Tag {\n\
             var tag = lookup(name);\n\
             if (tag != null) {\n\
                 throw \"exists\";\n\
             } else {\n\
                 tag = Tag { name: name };\n\
             }\n\
             return tag!!;\n\
         }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "post-if-else narrow with throwing then-branch must lift the else-branch's non-null reassignment; got: {:#?}",
        diags
    );
}
