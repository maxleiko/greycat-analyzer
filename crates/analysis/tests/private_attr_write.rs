//! `private attr: T` is read-public / write-private. Writes are
//! allowed from the owning type's constructor (`Foo { attr: 1 }`) and
//! from any *non-static* method declared on the owning type or on a
//! subtype that inherits the attr. Writes from anywhere else — top-
//! level fns, unrelated types' methods, the owning type's *static*
//! methods — must emit `private-attr-write`.

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

#[test]
fn instance_method_write_to_private_attr_is_allowed() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Foo {\n    private x: int;\n    \
         fn inc() { this.x = this.x + 1; }\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        codes.is_empty(),
        "instance method writing its own type's private attr must not error; got: {codes:?}",
    );
}

#[test]
fn instance_method_write_via_other_instance_of_same_type() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Foo {\n    private x: int;\n    \
         fn swap(o: Foo) { var t = this.x; this.x = o.x; o.x = t; }\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        codes.is_empty(),
        "instance method writing private attr through another instance of the same type must not \
         error; got: {codes:?}",
    );
}

#[test]
fn cross_module_instance_method_write_to_private_attr_is_allowed() {
    let mut mgr = SourceManager::new();
    let foo_uri = add(
        &mut mgr,
        "/proj/foo.gcl",
        "type Foo {\n    private x: int;\n    \
         fn touch() { this.x = this.x + 1; }\n}\n",
    );
    let _user_uri = add(
        &mut mgr,
        "/proj/user.gcl",
        "fn poke(f: Foo) { f.touch(); }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &foo_uri);
    assert!(
        codes.is_empty(),
        "method writing its own private attr in a multi-module project must not error; got: \
         {codes:?}",
    );
}

#[test]
fn subtype_instance_method_write_to_inherited_private_attr_is_allowed() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "abstract type Parent {\n    private bar: String?;\n}\n\
         type Foo extends Parent {\n    \
         fn touch() { this.bar = \"hello\"; }\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        codes.is_empty(),
        "subtype method writing inherited private attr must not error; got: {codes:?}",
    );
}

#[test]
fn assignment_from_unrelated_type_method_still_errors() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Foo {\n    private x: int;\n}\n\
         type Bar {\n    fn poke(f: Foo) { f.x = 0; }\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        codes.iter().any(|c| c == "private-attr-write"),
        "method on an unrelated type must not be allowed to write a private attr; got: {codes:?}",
    );
}

#[test]
fn static_method_write_to_private_attr_still_errors() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Foo {\n    private x: int;\n    \
         static fn poke(f: Foo) { f.x = 0; }\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        codes.iter().any(|c| c == "private-attr-write"),
        "static method writing its own type's private attr must error; got: {codes:?}",
    );
}
