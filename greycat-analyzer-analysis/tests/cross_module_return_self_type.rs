//! Regression: a foreign non-generic type used as both the declared
//! return type AND the returned object literal must NOT trip
//! `T is not assignable to T`.
//!
//! Before the fix, `lower_type_ref_id` (used by `validate_decl` to
//! lower the declared return / var-init type at validation time)
//! lacked the `resolve_decl_handle` step that `lower_type_ref` and
//! `lower_type_ref_project` already had — so for a foreign decl it
//! fell through to `arena.named(name)`. Meanwhile the body walker
//! minted `Type(handle)` for the same source token. The asymmetric
//! pair (`Named("Foo")` vs `Type(handle_for_Foo)`) defeats
//! `is_assignable_to_with_index`, and the analyzer surfaces a
//! self-named not-assignable diagnostic where the bare names render
//! identical on both sides of the message.
//!
//! Reproduced in the wild against `greycat/pro/text_search/`:
//! `return Section { ... }` from a fn `: Section` in a sibling
//! module flagged `type Section is not assignable to declared return
//! type Section`.

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
fn cross_module_return_value_matches_declared_return_type() {
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "type Foo {\n    x: int;\n}\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn make(): Foo {\n    return Foo { x: 1 };\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {:#?}",
        diags
    );
}

#[test]
fn cross_module_var_init_matches_declared_var_type() {
    // Same shape, var-init flavor — `validate_decl`'s `Decl::Var` arm
    // walks the same `lower_type_ref_id` path as `Decl::Fn`.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "type Foo {\n    x: int;\n}\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn caller() {\n    var f: Foo = Foo { x: 1 };\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {:#?}",
        diags
    );
}

#[test]
fn cross_module_generic_arg_matches_declared_type() {
    // Generic instantiation on a foreign type-arg: `Array<Foo>` in
    // module B must agree on the inner `Foo` identity with `Foo`
    // declared in module A. The recursive arg-lowering in
    // `lower_type_ref_id` shares the same `resolve_decl_handle`
    // hole — `Array<Foo>` reduces to `Generic("Array", [Named("Foo")])`
    // on the declared side while the body walker produces
    // `Generic("Array", [Type(handle_Foo)])` for `Array<Foo>{}`.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "type Foo {\n    x: int;\n}\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn make(): Array<Foo> {\n    return Array<Foo> {};\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {:#?}",
        diags
    );
}

#[test]
fn cross_module_map_two_generics_matches_declared_type() {
    // `Map<K, V>` where one of the type-args is a foreign decl. The
    // recursive arg-lowering must mint `Type(handle)` on both the
    // declared and the body-walker sides; otherwise
    // `Generic("Map", [Primitive(String), Named("Foo")])` vs
    // `Generic("Map", [Primitive(String), Type(handle_Foo)])` would
    // fail invariant arg-equality.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "type Foo {\n    x: int;\n}\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn make(): Map<String, Foo> {\n    return Map<String, Foo> {};\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {:#?}",
        diags
    );
}

#[test]
fn cross_module_user_generic_with_foreign_arg_matches() {
    // User-defined generic type `Pair<A, B>` declared in module A,
    // instantiated with a foreign-to-the-caller arg in module B.
    // Exercises `Generic { name: "Pair", args: [Type(handle_Foo),
    // Primitive(Int)] }` agreement across the two lowering paths.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "type Foo {\n    x: int;\n}\ntype Pair<A, B> {\n    a: A;\n    b: B;\n}\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn make(): Pair<Foo, int> {\n    return Pair<Foo, int> { a: Foo { x: 1 }, b: 2 };\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {:#?}",
        diags
    );
}

#[test]
fn cross_module_nested_generic_matches_declared_type() {
    // Deeply nested: `Array<Map<String, Foo>>`. Recursive lowering
    // must mint `Type(handle)` at the leaf even two levels deep.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "type Foo {\n    x: int;\n}\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn make(): Array<Map<String, Foo>> {\n    return Array<Map<String, Foo>> {};\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {:#?}",
        diags
    );
}

#[test]
fn cross_module_enum_value_flows_into_call_arg() {
    // Cross-module enum identity must survive a value flowing from
    // one foreign call's return into another foreign call's
    // parameter. Reproduced in the wild on `pro/lib/algebra/kmeans.gcl`:
    // `var t = tensor.type();` (returns `TensorType` from a foreign
    // module) → `Kmeans::configure(..., t, ...)` (param declared as
    // `tensor_type: TensorType` in *another* foreign module) used to
    // surface `value of type \`TensorType\` is not assignable to
    // parameter \`tensor_type: TensorType\``. Root cause: the
    // signature pass for the receiver type was lowered *before*
    // `index.enum_types` saw the enum (the previous lazy-population
    // ordering inside `lower_module_signatures`), so the method's
    // cached return type was a `Type(handle)` shape while the
    // call-arg validation lowered the param to the canonical
    // `Enum{...}` — and the two TypeIds compared unequal.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "enum Mode {\n    fast;\n    slow;\n}\n\
         type Box {\n    fn mode(): Mode { return Mode::fast; }\n}\n\
         type Holder {\n    static fn configure(m: Mode): int { return 0; }\n}\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn caller() {\n    var b = Box {};\n    var m = b.mode();\n    Holder::configure(m);\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "cross-module enum value should flow into a cross-module enum-typed parameter; got: {:#?}",
        diags
    );
}

#[test]
fn cross_module_enum_arg_matches_declared_type() {
    // `Array<SearchMode>` where `SearchMode` is a foreign enum.
    // The body walker hits `enum_type_for(name)` and produces
    // `Generic("Array", [Enum{...}])`; the validation side has to
    // hit the same branch ahead of `resolve_decl_handle`, otherwise
    // it mints `Generic("Array", [Type(handle)])` and the outer
    // shapes diverge. Reproduced in the wild on `pro/text_search/`'s
    // hybrid-search tests, which return `Array<SearchMode>`.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "enum Mode {\n    fast;\n    slow;\n}\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn make(): Array<Mode> {\n    return Array<Mode> {};\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {:#?}",
        diags
    );
}

#[test]
fn cross_module_stupidly_nested_generic_matches_declared_type() {
    // Stress test: every level mixes foreign user-defined types and
    // runtime generics. If any one of the recursive lowering hops
    // forgets to mint `Type(handle)` for a leaf, the outer
    // `Generic` shapes diverge between the body walker and the
    // validation pass — and the diagnostic surfaces with identical
    // bare-name rendering on both sides.
    //
    // Shape under test:
    //   Map<
    //     Bar,
    //     Array<
    //       Map<
    //         String,
    //         Pair<
    //           Array<Foo>,
    //           Map<Bar, Array<Foo>>
    //         >
    //       >
    //     >
    //   >
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "type Foo {\n    x: int;\n}\n\
         type Bar {\n    y: int;\n}\n\
         type Pair<A, B> {\n    a: A;\n    b: B;\n}\n",
    );
    let ty = "Map<Bar, Array<Map<String, Pair<Array<Foo>, Map<Bar, Array<Foo>>>>>>";
    let src = format!("fn make(): {ty} {{\n    return {ty} {{}};\n}}\n");
    let main_uri = add(&mut mgr, "/proj/src/main.gcl", &src);
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {:#?}",
        diags
    );
}
