# FOLLOWUP — work the original ROADMAP postponed

## Purpose

[ROADMAP.md](ROADMAP.md) ticked every chunk of the M1–M5 plan, but several of those ticks landed as *foundational passes* — the structure shipped, but specific behaviors were explicitly deferred so the rest of the plan could keep moving. This document is the catalog of those deferrals: what's missing, why, where the foundation already lives, and a rough priority.

It's a follow-up roadmap, not a wish list — every item below has a concrete deliverable and an acceptance bar. Items that turn into substantial chunks of their own should graduate into ROADMAP.md once they're picked up.

Sequencing intuition: **F1–F2 unblock real `.gcl` analysis on the corpus** (control-flow narrowing + grammar-gap drain). **F3–F5 close LSP UX gaps** (scope-aware rename, real quickfixes, multi-module resolution). **F6–F8 are quality / perf / distribution work** that doesn't gate further analyzer development.

---

## F1. Analyzer — control-flow narrowing & friends

The analyzer's foundational pass (P2.5) infers a single `TypeId` per expression and treats type information as flow-insensitive. Every concrete narrowing pattern below must be added.

### F1.1 Null-flow narrowing
- **Acceptance:** in the `then` branch of `if (x != null) { … }`, the inferred type of `x` strips the `nullable` flag. Symmetric in the `else` branch when the comparison is `==`.
- **Foundation:** `analyzer::AnalysisResult::def_types` already keys per-binding types. The narrowing pass needs a per-block override layer that the inference walker consults before falling back to `def_types`.
- **TS reference:** `packages/lang/src/analysis/analyzer.ts` (search for `isPossiblyNull`, `narrow`).

### F1.2 Type guards via `is`
- **Acceptance:** `if (x is Foo) { … }` narrows `x` to `Foo` in the then-branch. `is` returns `bool`.
- Foundation: `Expr::Unsupported { kind: "is_expr" }` in HIR — needs a real lowering plus the analyzer rule.

### F1.3 Non-null assertion (`!!`)
- **Foundation:** already lowered as `Expr::Unary(NonNullAssert)` and infers as `inner` minus `nullable` (see `analyzer::infer_expr`). What's missing is *flow propagation* — after `var y = x!!;`, `x` is also non-null in the rest of the block.

### F1.4 Exhaustiveness
- **Acceptance:** an `if`/`else if` chain over an enum that doesn't cover every variant produces a `non-exhaustive match over Color (missing: Red)` diagnostic. Same for nullable types missing a `null` arm.
- **Foundation:** `TypeKind::Enum { variants }` already carries the variant list (P2.4); enum decl ingestion records it (P2.6). Need an analyzer pass that walks `if`/`else` chains and tracks which constants were tested.

### F1.5 Unused-decl warnings
- **Acceptance:** top-level `fn` / `type` / `enum` / `var` decls that are never referenced and never `@expose`'d produce a `unused <kind> '<name>'` warning. Currently only locals/params are reported.
- **Foundation:** the resolver's reverse index isn't built yet; we'd add it as a `references_to: HashMap<Idx<Decl>, usize>` in `Resolutions`.

### F1.6 Declarator / hinter / actions ports
- **What's missing:** `analysis/declarator.ts`, `analysis/hinter.ts`, `analysis/actions.ts` (~700 LoC TS combined) drive richer per-decl validation, hover hints, and code-action proposals. Foundational P2.5 doesn't touch these.
- **Acceptance:** parity tests against the same source files used in the TS analyzer suite.

---

## F2. Tree-sitter grammar gaps & HIR `Unsupported`

### F2.1 Drain `KNOWN_GRAMMAR_GAPS`
The coverage gauntlet's `KNOWN_GRAMMAR_GAPS` list in [greycat-analyzer-syntax/tests/coverage.rs](greycat-analyzer-syntax/tests/coverage.rs) is a temporary buffer between *gap discovered* and *grammar fixed*. Each entry is a one-line grammar relaxation in [vendor/tree-sitter-greycat/grammar.js](vendor/tree-sitter-greycat/grammar.js).
- **Current backlog (1 entry):** `tests/corpus/parser_fixtures/inline_type/in.gcl` — last `type_attr` should not require trailing `;`.
- **Acceptance:** list empty.

### F2.2 Drain `Expr::Unsupported`
The HIR lowering walker emits `Expr::Unsupported { kind, byte_range }` for every CST shape that doesn't yet have a concrete HIR variant. Each one is dead code from the analyzer's perspective.
- **Procedure:** for each kind that shows up in a corpus run, decide whether it's worth a dedicated variant or merges into an existing one. Add the lowering rule in [greycat-analyzer-hir/src/lower.rs](greycat-analyzer-hir/src/lower.rs) and the analyzer/inference rule in [analyzer.rs](greycat-analyzer-analysis/src/analyzer.rs).
- **Known suspects:** template strings with interpolation, range expressions, `as` casts, `is` checks, generators / yield-style constructs (if any).
- **Acceptance:** running a `[hir::Unsupported]` log over `lib/std/*.gcl` reports zero entries.

---

## F3. Resolver — drop `BUILTIN_TYPES`, add cross-module lookup

### F3.1 Replace the builtin allowlist with stdlib-driven resolution
[analysis::resolver](greycat-analyzer-analysis/src/resolver.rs) pre-seeds a `BUILTIN_TYPES` table (`int`, `String`, `Array`, …) so primitive type names don't show as unresolved before P2.6 imports stdlib. Once F4 (below) wires `ProjectIndex` into the resolver, this table goes away — every name resolves to a real declaration in the loaded `lib/std`.
- **Acceptance:** `BUILTIN_TYPES` const is removed; tests still pass against a project with stdlib loaded.

### F3.2 Cross-module name resolution
The resolver currently only sees the module being resolved. Real GreyCat code references stdlib types and `@expose`'d names from other project modules.
- **Foundation:** P2.6 `ProjectIndex` already collects every type / enum / native function across modules. The resolver needs to consult it after exhausting the local module scope.
- **Acceptance:** a project that references `core::time` from `src/foo.gcl` resolves it to the stdlib decl, not "unresolved".

### F3.3 Member-access resolution
Resolver intentionally doesn't bind property names in `a.b` / `a->b` — it's type-driven and was deferred to P2.5. The analyzer now has type info, so member lookup should land on a real `TypeAttr` / `TypeMethod` and feed back into `Resolutions`.
- **Acceptance:** goto-definition on `point.x` lands on the `x: int;` line of the `Point` type decl.

---

## F4. SourceManager → ProjectIndex pipeline

P1.2 (`SourceManager::load_project`) and P2.6 (`ProjectIndex::ingest`) both exist but aren't connected. Today, the analyzer runs per-document with a fresh `TypeArena` / `TypeRegistry` every call.

- **Acceptance:** an `analyze_project(&SourceManager) -> ProjectAnalysis` driver builds one `ProjectIndex`, ingests every loaded module's types into it, and runs the analyzer over each module against that shared index. The LSP backend uses this for `publish_for` instead of re-running the per-file pipeline.
- **Adjacent work:** invalidation strategy — a `did_change` on file A should re-run analysis for A *and* anything that referenced A's exports. File-level dependency tracking goes here.

---

## F5. LSP capabilities — close the placeholder gaps

### F5.1 Scope-aware / cross-module rename (P3.4)
Today `capabilities::rename` does a string-equality walk over idents in the same file. Renaming `helper` accidentally renames any local variable also named `helper` in the same function.
- **Acceptance:** rename consults `Resolutions` to filter to occurrences that *resolve to the same definition* as the cursor, then expands across all loaded modules via F4.

### F5.2 Real code-action edits (P3.6)
`capabilities::code_actions` currently emits one quickfix per diagnostic *with empty edits*. Concrete fixes need real `TextEdit`s:
- "add missing `;`" for `missing-token` diagnostics
- "remove unused local" for `unused-local`
- "remove unused parameter" for `unused-param`
- "import type X from std" for unresolved type names (after F3.2)
- **Foundation:** the linter (P4.2) already has a `LintRule` framework and the byte ranges to operate on. The fix-application driver is the missing piece — see also F6.2.

### F5.3 Workspace symbols
Currently re-uses the document-symbols engine for the active document only. Should iterate every document in `SourceManager` and aggregate.

### F5.4 Goto-implementation as distinct from goto-definition (P3.2)
Today they share a handler. Once HIR represents `abstract` methods + their concrete impls, `gotoImplementation` should jump to the override(s) rather than the declaration.

### F5.5 Port `lsp.*.test.ts` scenarios
ROADMAP §7-B promises porting the 15 `lsp.*.test.ts` files as Rust integration tests against the running LSP. The current test surface ([greycat-analyzer-ls/tests/capabilities.rs](greycat-analyzer-ls/tests/capabilities.rs) + [lsp_smoke.rs](greycat-analyzer/tests/lsp_smoke.rs)) covers the per-handler logic and one round-trip — not all 15 scenarios.
- **Acceptance:** every `lsp.*.test.ts` scenario is mirrored as a Rust integration test using the same fixture inputs.

---

## F6. Formatter — TS prettifier parity (M5)

[greycat-analyzer-fmt](greycat-analyzer-fmt/src/lib.rs) is a foundational printer. The output round-trips through `parse → fmt → parse` cleanly and is idempotent on simple inputs, but does **not** produce byte-for-byte the same output as the TS prettifier on `tests/corpus/parser_fixtures/{name}/in.gcl → out.gcl`.

- **Acceptance:** every fixture's `fmt(in.gcl) == out.gcl` (byte-equal). This is the M5 acceptance criterion that's still open.
- **Effort:** porting `parser/cst/cst_format.ts` (~1,354 LoC of TS) is its own focused milestone. The TS port has per-construct reflow rules (line-break heuristics for long argument lists, alignment of consecutive type attrs, doc-comment placement, etc.) that aren't in the foundational printer.

### F6.1 Fix-application driver (linter side)
The TS lint suite has a fix-loop: collect edits, sort, drop overlapping ones, apply non-overlapping ones, re-run until convergence (max N passes). [analysis::lint](greycat-analyzer-analysis/src/lint.rs) doesn't have this yet.
- **Acceptance:** `cli lint --fix` actually applies safe fixes in place, with the same convergence semantics as TS `cli/src/lint/lint.ts`.

---

## F7. Type system — beyond the foundational pass

[greycat-analyzer-types::is_assignable_to](greycat-analyzer-types/src/lib.rs) covers the cases the analyzer demands today (primitive widening, null/any/never, generic invariance, lambda variance, tuples, unions). The TS reference has more nuance:

- **F7.1** Node-type tagging (`node`, `nodeTime`, `nodeGeo`, `nodeList`, `nodeIndex`) — these are not just named types; the runtime tags them and subtyping has special rules for nodes vs. their inner types.
- **F7.2** Generic instantiation with constraints — declared `type Foo<T> {…}` with bound-style constraints. Currently every generic-param falls back to `Any`.
- **F7.3** Inference table / unification — needed when an analyzer rule wants to *infer* a generic argument from usage rather than read it from a declaration. Today the analyzer punts to `Any` in those cases.
- **F7.4** Anonymous object structural compatibility — `{ a: int, b: String }` vs `{ a: int, b: String, c: bool }`. Foundational `is_assignable_to` doesn't handle anonymous types yet.

Acceptance for each: a unit test in `greycat-analyzer-types` plus an analyzer behavior test that demonstrates the rule kicks in on a real snippet.

---

## F8. Distribution

### F8.1 crates.io publish (ROADMAP 5.3)
Mechanical, not gated on analyzer work.
- Each `Cargo.toml` needs `description`, `license`, `repository`, `keywords`, `categories`.
- `path` deps in workspace members need a `version = "..."` shadow for publication.
- Add `LICENSE-MIT` + `LICENSE-APACHE` at workspace root and reference them in each crate.
- First publish order: `syntax → core → hir → types → fmt → analysis → ls → bin`.

### F8.2 Salsa retrofit (ROADMAP 5.5, optional)
Only do this if profiling on a multi-module workspace shows quadratic blow-up on `did_change`. Foundational design (file-level invalidation as pure functions) keeps the retrofit cheap when it becomes necessary.

### F8.3 Playground UI maturation (ROADMAP 5.2)
The playground shell is in place but the panels are read-only renderers. Productive next steps:
- Click-to-jump from CST / HIR / diagnostic rows back to a Monaco editor selection.
- Live LSP integration in the Monaco editor (run the LSP via a web worker on the wasm crate so completion / hover / diagnostics fire in the editor, not just in panels).
- Persist the editor's text in `localStorage` so refreshes don't lose work.

---

## F9. Test strategy gaps

ROADMAP §7 promised three layers (snapshot conformance, Rust-idiomatic units, fuzzing). Layer C is missing.

- **F9.1** Set up `cargo-fuzz` against the parser/HIR boundary. Targets: `parse(source) -> Tree` then `lower_module(source, _, _, root)` shouldn't panic on arbitrary UTF-8 input.
- **F9.2** Diagnostic-JSON conformance against the TS reference (ROADMAP §7-A). Once F4 lands and the analyzer runs at the project level, snapshot the analyzer's diagnostics output and diff it against TS reference output over the same corpus.

---

## F10. Documentation

- **F10.1** Crate-level rustdoc landing pages — currently each crate's `lib.rs` has a one-line doc. They should each summarize what the crate is for, what it depends on, and what's deferred.
- **F10.2** A "porting from TS" guide — for users / contributors who know the TS reference, a doc that maps every TS module to its Rust crate and notes structural differences (e.g. resolver/environment merged, single typed AST, no class-hierarchy).
- **F10.3** Playground-readme — the playground's `pnpm wasm && pnpm dev` flow, the EMSDK requirement, and how to extend it with a new panel.

---

## Priority

| ID | Title | Why now |
|---|---|---|
| **F1.1** | Null-flow narrowing | Most-asked-for analyzer feature; everything below benefits |
| **F2.1** | Drain known grammar gap | One-liner; deletes the existing allowlist entry |
| **F4** | SourceManager → ProjectIndex | Unblocks F3.2, F5.1, F5.3 — lots of cascades |
| **F3.2** | Cross-module name resolution | Finishes the resolver story |
| **F5.2** | Real code-action edits | Closes the largest visible LSP UX gap |
| **F1.4** | Enum exhaustiveness | High-value catch |
| **F6** | Formatter parity | M5 acceptance criterion still open |
| **F1.6** | declarator/hinter/actions | Substantial port work; do once smaller items are clear |
| F2.2 | Drain `Expr::Unsupported` | Incremental; happens as the corpus grows |
| F3.1 | Drop `BUILTIN_TYPES` | Trivial after F4 lands |
| F5.1 | Scope-aware rename | Clear value but not blocking |
| F5.3 | Workspace symbols | Easy after F4 |
| F5.4 | Goto-implementation distinct | Needs `abstract` semantics first |
| F5.5 | Port `lsp.*.test.ts` | Volume work; not blocking |
| F1.5 | Unused-decl warnings | Needs reverse-reference index |
| F1.2 | `is` type guards | Couples with F1.1 |
| F1.3 | `!!` flow propagation | Couples with F1.1 |
| F7.* | Type system nuance | As specific corpus failures demand |
| F8.1 | crates.io publish | When the API is stable enough to commit to |
| F8.2 | Salsa retrofit | Only if profiling demands |
| F8.3 | Playground maturation | UI polish; non-blocking |
| F9.* | Fuzzing + parity oracle | Quality harness, do once F4 lands |
| F10.* | Docs | Continuous |

Update this document in-place as items land. Promote anything that grows into a multi-week effort into [ROADMAP.md](ROADMAP.md) so it gets the same chunk-by-chunk treatment as the original plan.
