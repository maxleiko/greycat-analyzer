//! lambda-unify — `fn(...)` lambdas and `function`-typed fn refs are
//! one concept at the type-checker level. Covers the user-supplied
//! spec in `project.gcl` plus the surrounding shapes:
//!
//! - lambda flows into `function` parameter (assignability).
//! - fn-ref (top-level + static method) flows into `function` parameter.
//! - call through a lambda var checks arg types against the lambda's
//!   declared params.
//! - call through a fn-ref var checks arg types against the underlying
//!   fn's signature (since the ref carries the structural Lambda).
//! - lambda body-driven return-type inference renders informative
//!   display shapes (`fn()`, `fn(): int`, `fn(): int?`).
//! - instance-method value reference is a hard error.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

/// Minimal stdlib stub so `well_known.function_decl` etc. populate.
/// Without this, `function`/`int`/`String` resolve to `Unresolved` and
/// the `Lambda → function` assignability rule (which gates on the
/// well-known handle) can't fire.
fn synthetic_std() -> &'static str {
    "native type any {}\n\
     native type null {}\n\
     native type bool {}\n\
     native type char {}\n\
     native type int {}\n\
     native type float {}\n\
     native type String {}\n\
     native type time {}\n\
     native type duration {}\n\
     native type geo {}\n\
     native type type {}\n\
     native type field {}\n\
     native type function {}\n\
     native type Array<T> {}\n\
     native type Map<K, V> {}\n\
     type Tuple<T, U> { a: T; b: U; }\n"
}

fn add_stdlib(mgr: &mut SourceManager) {
    let uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    mgr.add_simple(uri, synthetic_std(), "std", false);
}

fn diagnostics_with_code<'a>(pa: &'a ProjectAnalysis, uri: &Uri, code: &str) -> Vec<&'a str> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.code == code)
        .map(|d| d.message.as_str())
        .collect()
}

fn error_codes<'a>(pa: &'a ProjectAnalysis, uri: &Uri) -> Vec<&'a str> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == greycat_analyzer_analysis::analyzer::Severity::Error)
        .map(|d| d.code)
        .collect()
}

#[test]
fn lambda_is_assignable_to_function_parameter() {
    let mut mgr = SourceManager::new();
    add_stdlib(&mut mgr);
    let main = add(
        &mut mgr,
        "/proj/main.gcl",
        "fn expect_fn(_: function) {}
fn main() {
    var f = fn(a: int, b: int): int { return a + b; };
    expect_fn(f);
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let errs = error_codes(&pa, &main);
    assert!(
        errs.is_empty(),
        "lambda → function should be assignable, got errors: {errs:?}"
    );
}

#[test]
fn call_through_typed_lambda_var_checks_arg_types() {
    let mut mgr = SourceManager::new();
    add_stdlib(&mut mgr);
    let main = add(
        &mut mgr,
        "/proj/main.gcl",
        "fn main() {
    var f = fn(a: int, b: int): int { return a + b; };
    f(3.14, 42);
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let mismatches = diagnostics_with_code(&pa, &main, "argument-type-mismatch");
    assert_eq!(
        mismatches.len(),
        1,
        "expected one argument-type-mismatch for `3.14` not int, got {mismatches:?}"
    );
    assert!(
        mismatches[0].contains("float") && mismatches[0].contains("int"),
        "diagnostic should name float → int mismatch, got: {}",
        mismatches[0]
    );
}

#[test]
fn call_through_static_fn_ref_var_checks_arg_types() {
    // Mirrors the user's `Runtime::on_files_put` example with a
    // local sibling type so the test doesn't depend on stdlib.
    let mut mgr = SourceManager::new();
    add_stdlib(&mut mgr);
    let main = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Runtime { static native fn on_files_put(handler: function?); }
fn main() {
    var x = Runtime::on_files_put;
    x(\"hello\");
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let mismatches = diagnostics_with_code(&pa, &main, "argument-type-mismatch");
    assert_eq!(
        mismatches.len(),
        1,
        "expected one argument-type-mismatch for `\"hello\"` not function?, got {mismatches:?}"
    );
    assert!(
        mismatches[0].contains("String") && mismatches[0].contains("function"),
        "diagnostic should name String → function mismatch, got: {}",
        mismatches[0]
    );
}

#[test]
fn cross_module_fn_ref_still_assignable_to_function() {
    // Existing `cross_module_fn_value` invariant: bare cross-module
    // fn-ref → function param must stay assignable. lambda-unify
    // refactors this from `Type(function_decl)` to a structural
    // Lambda; the new `Lambda → function` assignability rule keeps
    // the test green.
    let mut mgr = SourceManager::new();
    add_stdlib(&mut mgr);
    add(
        &mut mgr,
        "/proj/helper.gcl",
        "fn fetch_stuff(): int { return 0; }\n",
    );
    let main = add(
        &mut mgr,
        "/proj/main.gcl",
        "fn takes_fn(f: function) {}
fn caller() {
    takes_fn(fetch_stuff);
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let errs = error_codes(&pa, &main);
    assert!(
        errs.is_empty(),
        "cross-module fn-ref → function should stay assignable, got: {errs:?}"
    );
}

#[test]
fn lambda_no_return_displays_as_fn_no_ret() {
    let mut mgr = SourceManager::new();
    add_stdlib(&mut mgr);
    let main = add(
        &mut mgr,
        "/proj/main.gcl",
        "fn main() {
    var f = fn() { };
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let m = pa.module(&main).expect("module");
    let f_local = m
        .hir
        .idents
        .iter()
        .find(|(_, i)| pa.symbols()[i.symbol] == *"f")
        .map(|(idx, _)| idx)
        .expect("`f` ident");
    let ty = m
        .analysis
        .def_types
        .get(&f_local)
        .copied()
        .expect("def_type for f");
    assert_eq!(pa.display_type(ty).to_string(), "fn()");
}

#[test]
fn lambda_body_inferred_int_return() {
    let mut mgr = SourceManager::new();
    add_stdlib(&mut mgr);
    let main = add(
        &mut mgr,
        "/proj/main.gcl",
        "fn main() {
    var f = fn() { return 5; };
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let m = pa.module(&main).expect("module");
    let f_local = m
        .hir
        .idents
        .iter()
        .find(|(_, i)| pa.symbols()[i.symbol] == *"f")
        .map(|(idx, _)| idx)
        .expect("`f` ident");
    let ty = m
        .analysis
        .def_types
        .get(&f_local)
        .copied()
        .expect("def_type for f");
    assert_eq!(pa.display_type(ty).to_string(), "fn(): int");
}

#[test]
fn lambda_body_inferred_nullable_int_return() {
    let mut mgr = SourceManager::new();
    add_stdlib(&mut mgr);
    let main = add(
        &mut mgr,
        "/proj/main.gcl",
        "fn main() {
    var f = fn(b: bool): int? {
        if (b) { return 5; }
        return null;
    };
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let m = pa.module(&main).expect("module");
    let f_local = m
        .hir
        .idents
        .iter()
        .find(|(_, i)| pa.symbols()[i.symbol] == *"f")
        .map(|(idx, _)| idx)
        .expect("`f` ident");
    let ty = m
        .analysis
        .def_types
        .get(&f_local)
        .copied()
        .expect("def_type for f");
    assert_eq!(pa.display_type(ty).to_string(), "fn(bool): int?");
}

#[test]
fn lambda_body_inference_bails_on_incompatible_branches() {
    let mut mgr = SourceManager::new();
    add_stdlib(&mut mgr);
    let main = add(
        &mut mgr,
        "/proj/main.gcl",
        "fn main() {
    var f = fn(b: bool) {
        if (b) { return 5; }
        return \"x\";
    };
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let m = pa.module(&main).expect("module");
    let f_local = m
        .hir
        .idents
        .iter()
        .find(|(_, i)| pa.symbols()[i.symbol] == *"f")
        .map(|(idx, _)| idx)
        .expect("`f` ident");
    let ty = m
        .analysis
        .def_types
        .get(&f_local)
        .copied()
        .expect("def_type for f");
    // int + String can't reduce to T or T?, so the lambda's ret
    // stays None — displayed without a `: ret` clause.
    assert_eq!(pa.display_type(ty).to_string(), "fn(bool)");
}

#[test]
fn instance_method_value_ref_is_error() {
    let mut mgr = SourceManager::new();
    add_stdlib(&mut mgr);
    let main = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Foo {
    a: int;
    fn double(): int { return this.a * 2; }
}
fn main() {
    var foo = Foo { a: 21 };
    var f = foo.double;
    var g = Foo::double;
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let errs = diagnostics_with_code(&pa, &main, "instance-method-value-ref");
    // Both `foo.double` and `Foo::double` are non-static method refs
    // outside callee position — each surfaces the error.
    assert_eq!(
        errs.len(),
        2,
        "expected two instance-method-value-ref errors, got {errs:?}"
    );
}

#[test]
fn instance_method_called_directly_does_not_error() {
    let mut mgr = SourceManager::new();
    add_stdlib(&mut mgr);
    let main = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Foo {
    a: int;
    fn double(): int { return this.a * 2; }
}
fn main() {
    var foo = Foo { a: 21 };
    var x = foo.double();
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let errs = diagnostics_with_code(&pa, &main, "instance-method-value-ref");
    assert!(
        errs.is_empty(),
        "calling an instance method (`foo.double()`) is fine — only value refs error. got {errs:?}"
    );
}
