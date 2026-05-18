//! `private attr: T` is read-public / write-private — only the type's
//! constructor (`Foo { attr: 1 }`) can write to it. Direct assignment
//! `obj.attr = x` from anywhere else (even same module, even on a
//! locally-declared type) must emit `private-attr-write`.

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

fn codes(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    pa.module(uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.code.to_string())
        .collect()
}

#[test]
fn direct_assignment_to_private_attr_errors_same_module() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Account {\n    private balance: int;\n}\n\
         fn debit(a: Account, n: int) { a.balance = a.balance - n; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        codes.iter().any(|c| c == "private-attr-write"),
        "expected `private-attr-write` on `a.balance = ...`; got: {codes:?}",
    );
}

#[test]
fn read_of_private_attr_is_allowed() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Account {\n    private balance: int;\n}\n\
         fn show(a: Account): int { return a.balance; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        codes.is_empty(),
        "read of private attr must not error; got: {codes:?}",
    );
}

#[test]
fn constructor_write_to_private_attr_is_allowed() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Account {\n    private balance: int;\n}\n\
         fn open(b: int): Account { return Account { balance: b }; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        codes.is_empty(),
        "object-expr write to private attr must not error; got: {codes:?}",
    );
}

#[test]
fn cross_module_direct_assignment_to_private_attr_errors() {
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/account.gcl",
        "type Account {\n    private balance: int;\n}\n",
    );
    let user_uri = add(
        &mut mgr,
        "/proj/user.gcl",
        "fn debit(a: Account, n: int) { a.balance = a.balance - n; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &user_uri);
    assert!(
        codes.iter().any(|c| c == "private-attr-write"),
        "expected `private-attr-write` cross-module too; got: {codes:?}",
    );
}

#[test]
fn assignment_to_public_attr_is_allowed() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type User {\n    name: String;\n}\n\
         fn rename(u: User, n: String) { u.name = n; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        codes.is_empty(),
        "public attr assignment must not error; got: {codes:?}",
    );
}
