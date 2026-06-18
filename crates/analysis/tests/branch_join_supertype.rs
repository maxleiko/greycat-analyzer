//! Regression: an if / else-if chain (no final else) assigning three
//! different concrete subtypes to a binding declared at the supertype
//! must join the branches to the declared supertype, not collapse to
//! one branch's type.
//!
//! ```text
//! var a: Animal?;
//! if (k == 0) { a = Dog {...}; }
//! else if (k == 1) { a = Cat {...}; }
//! else if (k == 2) { a = Fish {...}; }
//! if (a != null) {
//!     if (a is Dog) {} else if (a is Cat) {} else if (a is Fish) {}
//! }
//! ```
//!
//! The post-if join previously carried only the first reaching branch
//! (`Dog?`), so `a` looked like `Dog` after `a != null`: the `is Dog`
//! check read "always true" and the `is Cat` / `is Fish` arms read
//! "always false" + unreachable. The join now widens disagreeing
//! branches to the binding's declared type (`Animal?`), so all three
//! subtypes stay live. (The final `is Fish` is still flagged
//! "always true" by sealed-hierarchy exhaustiveness once Dog and Cat
//! are ruled out — a distinct, sound mechanism.)

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn all_messages(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn if_else_if_chain_joins_to_declared_supertype() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "abstract type Animal {}\n\
         type Dog extends Animal { legs: int; }\n\
         type Cat extends Animal { lives: int; }\n\
         type Fish extends Animal { fins: int; }\n\
         fn classify(kind: int) {\n\
             var a: Animal?;\n\
             if (kind == 0) { a = Dog { legs: 4 }; }\n\
             else if (kind == 1) { a = Cat { lives: 9 }; }\n\
             else if (kind == 2) { a = Fish { fins: 2 }; }\n\
             if (a != null) {\n\
                 if (a is Dog) {}\n\
                 else if (a is Cat) {}\n\
                 else if (a is Fish) {}\n\
             }\n\
         }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let msgs = all_messages(&pa, &uri);

    // The first-branch collapse used to surface all of these.
    assert!(
        !msgs.iter().any(|m| m.contains("unreachable")),
        "no is-arm should be unreachable; got: {msgs:#?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("can never be")),
        "no is-arm should be statically false; got: {msgs:#?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("already of type `Dog`")),
        "`a` must not collapse to the first branch's `Dog`; got: {msgs:#?}"
    );
}
