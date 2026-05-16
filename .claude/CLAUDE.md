# greycat-analyzer

Rust port of the [GreyCat](https://greycat.io) language frontend: static analyzer, LSP server, formatter, linter. Targets `.gcl` source. Distributed as a CLI binary, an LSP server, a WASM build, and library crates.

The reference implementation is the TypeScript monorepo at `https://hub.datathings.com/greycat/lang`. The Rust port matches its frontend; no runtime/VM is in scope.

Rust edition 2024. Workspace resolver `"3"`. Workspace metadata (`license = "MIT OR Apache-2.0"`, repo, authors) lives in `[workspace.package]` and every crate inherits via `*.workspace = true`.

## Workspace layout

| Crate | Purpose |
|---|---|
| [greycat-analyzer-syntax](../greycat-analyzer-syntax/) | Tree-sitter wrapper. Owns parsing via [tree-sitter-greycat](../tree-sitter-greycat/) (git submodule at the repo root). |
| [greycat-analyzer-core](../greycat-analyzer-core/) | `Document`, `SourceManager`, `span`, project graph, `@library` / `@include` resolver, parse diagnostics. Re-exports `lsp_types` and `greycat_analyzer_syntax`, `Type` / `TypeKind`, `TypeArena`, subtyping (`is_assignable_to`), `InferenceTable` foundation. |
| [greycat-analyzer-hir](../greycat-analyzer-hir/) | Arena-backed typed HIR, CST→HIR lowering. |
| [greycat-analyzer-analysis](../greycat-analyzer-analysis/) | Resolver, analyzer (inference + null-flow + `is`-narrowing + enum exhaustiveness + member resolution), lints, `ProjectAnalysis` driver, `ProjectIndex` cross-module index, `actions` vocabulary, capability-shaped services (e.g. [`rename`](../greycat-analyzer-analysis/src/rename.rs) for goto-refs / rename target discovery). |
| [greycat-analyzer-fmt](../greycat-analyzer-fmt/) | Formatter (foundational; per-construct parity with TS `cst_format.ts` is P9.1). |
| [greycat-analyzer-server](../greycat-analyzer-server/) | LSP server (`lsp-server` + `crossbeam-channel`). Per-capability handlers under [src/capabilities/](../greycat-analyzer-server/src/capabilities/) (one file per LSP request kind). Shared LSP `Position` / byte-offset helpers in [src/conv.rs](../greycat-analyzer-server/src/conv.rs). |
| [greycat-analyzer](../greycat-analyzer/) | CLI binary `greycat-lang`. `clap` subcommands in [src/cmd/](../greycat-analyzer/src/cmd/). |
| [greycat-analyzer-wasm](../greycat-analyzer-wasm/) | `cdylib` + `rlib`, `wasm-bindgen` bridge. Drives the playground. |
| [playground/](../playground/) | Vite/TS/Lit/WebAwesome/Monaco UI consuming the wasm pkg. **Committed**, *not* a workspace member. |

Dependency direction: `syntax → core → hir → types → analysis → {ls, cli, wasm, fmt}`.

## Project model

GreyCat projects have a single entrypoint (conventionally `project.gcl`) whose `@library` / `@include` mod-pragmas form the closure of analyzed modules. **Never flat-walk a directory for `.gcl` files** — go through [`SourceManager::load_project(entrypoint)`](../greycat-analyzer-core/src/manager.rs), which:

- parses the entrypoint, walks its `mod_pragma` nodes,
- resolves `@library("name", "version")` against `<project_dir>/lib/<name>/` first, falling back to `<greycat_home>/lib/std/` for the `std` library,
- resolves `@include("relative/dir")` against `<project_dir>/relative/dir/`,
- loads every `.gcl` it finds, recurses on each loaded module's pragmas, with cycle protection.

The CLI (`lint`, `fmt`) and LSP (`Backend::load_workspace`) both go through this. The repo-root [project.gcl](../project.gcl) is the canonical entrypoint and pins the stdlib version.

**LSP hosts N independent projects** (P32). One workspace folder = one project (the `project.gcl` at the folder root, loaded eagerly). A nested `project.gcl` deeper in the tree gets loaded lazily the first time the user opens a `.gcl` file under it — the LSP walks parents from that file up to the enclosing workspace folder, picks the nearest `project.gcl`, and spins up a fresh `(SourceManager, ProjectAnalysis, TypeArena, ProjectIndex)` for it. Each `Project` lives in `Backend::projects` keyed by its root directory; URIs route via `Backend::uri_owner`. Cross-project navigation is intentionally absent — projects are isolated closures, matching the runtime. Two kinds of file-spanning advisory diagnostics surface design issues: `orphan-module` (Information+UNNECESSARY) for `.gcl` files inside a workspace with no `project.gcl` up-tree, and `multi-project-owner` for files reachable from two projects' `@include` closures. The CLI is unaffected — it always operates on one explicit entrypoint at a time.

## Server crate layout (LSP)

`greycat-analyzer-server/src/` is split per-capability. **Never grow `capabilities/mod.rs` back into a single big file** — each LSP request kind owns one file under `capabilities/`, and any helper used by exactly two siblings either moves to the analysis crate (if it's analysis work) or lives in the file most associated with it as `pub(super)`.

| File | Owns |
|---|---|
| [src/conv.rs](../greycat-analyzer-server/src/conv.rs) | `position_to_byte`, `byte_to_position`, `byte_range_to_lsp`, `ranges_overlap`, `stmt_byte_range`. Every Position ↔ byte-offset conversion goes through here so the `character == byte column` convention stays consistent. |
| [src/capabilities/mod.rs](../greycat-analyzer-server/src/capabilities/mod.rs) | Re-export hub only. No logic. ~50 lines. |
| [src/capabilities/hover.rs](../greycat-analyzer-server/src/capabilities/hover.rs) | `hover_with_project` + `hover_inner` fallback + the 17 markdown / signature renderers (`render_decl_signature`, `render_fn_signature`, `render_type_ref(_with_subst)`, `RenderCtx`, `decl_doc`, `module_label_for_uri`, …). Several are `pub(super)` because goto / completion / signature_help consume them — this coupling is real and will be the target of follow-up item (4). |
| [src/capabilities/goto.rs](../greycat-analyzer-server/src/capabilities/goto.rs) | Definition / declaration / implementation handlers + `cursor_ident_idx` (the position → `Idx<Ident>` bridge used by every project-aware capability). |
| [src/capabilities/references_rename.rs](../greycat-analyzer-server/src/capabilities/references_rename.rs) | **Thin LSP wrapper.** `references_across_project` / `rename_across_project` / `prepare_rename`. The real work lives in `greycat_analyzer_analysis::rename` (see "Capability services" below). `RenameTarget` is re-exported for downstream consumers. |
| [src/capabilities/completion.rs](../greycat-analyzer-server/src/capabilities/completion.rs) | `completion_with_project` + ~2700 lines of scope walking, member enumeration, type-position completion, snippet emission. **This is the big outstanding refactor target** — most of it should move to an `analysis::completion` service mirroring the `rename` split (see follow-up (2)). |
| [src/capabilities/inlay_hints.rs](../greycat-analyzer-server/src/capabilities/inlay_hints.rs) | `inlay_hints_with_project` + var/return-type/arg-name emitters. |
| [src/capabilities/code_actions.rs](../greycat-analyzer-server/src/capabilities/code_actions.rs) | `code_actions_with_project` + parse-safety gate. |
| [src/capabilities/diagnostics.rs](../greycat-analyzer-server/src/capabilities/diagnostics.rs) | `diagnostics_from_module` (`ModuleAnalysis` → `Vec<Diagnostic>`). Mirror of the CLI `lint` command's per-module conversion. |
| [src/capabilities/semantic_tokens.rs](../greycat-analyzer-server/src/capabilities/semantic_tokens.rs) | `SEMANTIC_TOKEN_TYPES` table + walk + delta-encoder. |
| [src/capabilities/{document_symbols,workspace_symbols,document_highlights,selection_ranges,folding_ranges,formatting,signature_help}.rs](../greycat-analyzer-server/src/capabilities/) | One handler each, all small (≤110 lines). |

**Capability services in the analysis crate.** When a capability's heavy lifting is *analysis* (scope walking, type member discovery, name binding queries), the work lives in `greycat-analyzer-analysis/src/<capability>.rs` and the LSP layer is shape-conversion only. The reference pattern is [`greycat_analyzer_analysis::rename`](../greycat-analyzer-analysis/src/rename.rs):

- `RenameTarget` enum, `resolve_target(project, cursor_uri, cursor_idx) -> Option<RenameTarget>`, `target_sites(project, &target) -> Vec<TargetSite { uri, byte_range }>`. Pure analysis: no `SourceManager`, no source text, no `lsp_types`.
- LSP wrapper (`capabilities::references_rename`) calls these, fetches text via `SourceManager` per-URI, and maps `TargetSite` → `Location` / `TextEdit`.

This shape is the target for completion (follow-up (2)) and any future capability whose logic isn't pure LSP I/O. **Hard rule: when adding a new LSP capability, ask "is this LSP shape work, or is it analysis?" If analysis, the implementation goes in the analysis crate even on the first commit.**

**The cached-`ProjectAnalysis` path is the only path.** Every LSP request handler in [src/server.rs](../greycat-analyzer-server/src/server.rs) resolves the URI to its `Project` via `Backend::project_for`, then forwards to the matching `*_with_project` / `*_across_project` capability. The legacy single-file shims (`hover`, `references`, `rename`, `completion`, `inlay_hints`, `code_actions` without a project arg) have been **deleted** — they re-ran the whole pipeline from scratch and silently diverged from the cached path's cross-module fixups. Don't reintroduce one; if a test thinks it needs single-file mode, use the `TestProject` fixture instead.

**Integration tests use the `TestProject` fixture.** [`tests/support/mod.rs`](../greycat-analyzer-server/tests/support/mod.rs) wraps a `(SourceManager, ProjectAnalysis, Uri)` and exposes capability shortcuts (`project.hover(pos)`, `project.references(pos)`, `project.rename(pos, "new")`, `project.completion(pos)`, `project.inlay_hints(&range)`, `project.code_actions(range)`, `project.goto_definition(pos)`). Tests should always go through these — never construct a `tree-sitter::Tree` + call a capability directly. That bypasses the cached `ModuleAnalysis` and tests something the server doesn't run.

## Parsing

Always parse via `greycat-analyzer-syntax::parse(source)`. The tree-sitter grammar is at ABI v15, so the host `tree-sitter` crate must be ≥ `0.26`.

Tree-sitter owns scanning; do not add a separate Rust lexer.

### Grammar lives in this repo

`tree-sitter-greycat` is a **git submodule** at [tree-sitter-greycat/](../tree-sitter-greycat/) (repo root), pulled in as a Cargo `path` dep. That means: when the parser disagrees with the TS reference, **edit the grammar locally and re-run `cargo test --workspace`** instead of allowlisting the divergence in the analyzer.

Grammar edit loop:

```sh
# 1. Edit tree-sitter-greycat/grammar.js
# 2. Regenerate parser.c + node-types.json
cd tree-sitter-greycat && tree-sitter generate
# 3. Run the in-grammar corpus tests (locks shape of tricky parses)
tree-sitter test
cd -
# 4. Re-run gauntlet
cargo test -p greycat-analyzer-syntax --test coverage
```

`tree-sitter-greycat/test/corpus/*.txt` holds **regression tests for grammar shape** — precedence (`binary_expr_precedence.txt`), associativity, nullable postfix (`optional_postfix.txt`), and any other parse where the right tree was historically not obvious. Each block is a named source snippet plus the exact CST s-expression it should produce. **When you fix a parse bug, add a test here** so it can't regress silently. Run via `tree-sitter test` from inside the submodule. These tests run *fast* (no Rust compile) and catch shape regressions earlier than the Rust gauntlet, so they are the first signal — but they are not a substitute for the gauntlet.

The syntax crate's `build.rs` reads `node-types.json` directly from the submodule; there is no vendored copy in `greycat-analyzer-syntax/`. The submodule SHA *is* the grammar pin.

When grammar fixes are ready, commit inside the submodule and push to `maxleiko/tree-sitter-greycat`, then bump the submodule pointer here. The submodule's own [CLAUDE.md](../tree-sitter-greycat/.claude/CLAUDE.md) has the full grammar workflow (scanner.c boundary, query updates, etc.).

**Hard rule:** when the gauntlet flags ERROR/MISSING, when CST shape diverges from TS reference, when typed-node accessors return `None` where the reference produces a value — pause and ask. Default answer is "fix the grammar," not "work around it." `KNOWN_GRAMMAR_GAPS` in `greycat-analyzer-syntax/tests/coverage.rs` is a temporary buffer between *gap discovered* and *grammar fixed*; it should be empty most of the time (currently `&[]`).

**Hard rule — never assume GreyCat syntax (auto-mode included):** before adding, narrowing, or restricting any grammar / lowering / analyzer rule based on a guess about what is "valid GreyCat," verify against an authoritative source. Stopping to verify is mandatory — auto-mode does **not** waive this. The cost of one paused turn is far less than the cost of regressing the corpus or shipping a parser that rejects valid programs.

Verification order:

1. Inspect `tree-sitter-greycat/grammar.js` for the existing rule shape.
2. Search the stdlib (`lib/std/*.gcl`) and corpus (`tests/corpus/`) for real examples of the construct.
3. Invoke the `/greycat:greycat` skill — it has the canonical syntax/semantics reference.
4. Read the TS reference at `https://hub.datathings.com/greycat/lang` if you have a checkout / can git clone with ssh (`git clone git@hub.datathings.com:greycat/lang.git`).
5. **Run against `greycat run`** — the GreyCat compiler is the *true oracle* for what is and isn't valid. Write a minimal `project.gcl`, run `greycat run`, and read the diagnostic. Caveat: the runtime compiler doesn't do control-flow analysis (narrowing, exhaustiveness, etc.), so behaviors that depend on those still need to be validated through the TS reference / corpus / runtime *assignment* (e.g. provoke the error by actually running the call). When the TS reference and the runtime disagree (it has happened — see P12.2 generic variance), trust the runtime.
6. If still unresolved after (1)–(5): **STOP and ask the user.** Even in auto-mode.

If for any reason you cannot stop (truly unattended run), leave a `// FIXME(syntax-assumption): <what you assumed and why>` comment beside the change AND append a one-line entry to [`docs/syntax-assumptions.log`](../docs/syntax-assumptions.log) so the user can find every assumption on return. Never ship a grammar / lowering change that bakes in a syntactic assumption silently.

Counter-examples (do not repeat):
- Hallucinating `T: Bound` generic-bound syntax — `.gcl` has no such form.
- Assuming typed-suffix literals require a leading `_` — they do not. `42time`, `42_time`, `1.79e+308_f` are all valid; the leading `_` is a formatter convention, not a grammar requirement.
- Using `token.immediate` on `number_suffix` to fix the static-init parse: shifted the lexer balance and made `e` win over scientific-notation `"e"` in `1e3`. Lex-level changes here need to preserve the existing scientific / suffix tie-break.

## Analysis stages — fix the source, not the symptom

**Hard rule:** when a typing / inference / member-resolution diagnostic is wrong, fix it in the stage that *originates* the type, not by post-patching the result downstream.

The analysis pipeline is layered: S1 lower → S2-S6 module-local prep → S7-S11 cross-module signature lowering → S12 body walker → post-S12 cross-module passes → `validate_type_relations`. Each stage builds on what earlier stages settled. A symptom in `validate_type_relations` (e.g. `T?` not assignable to `T`) is almost always a hint that an earlier stage failed to propagate a narrow / element-type / generic-arg / receiver-type that it should have known about.

The temptation, especially under time pressure, is to add a "fix-up" pass that walks the diagnostic set and silences the cases that look like false positives. **That is monkey-patching.** It hides the real defect (an analysis stage not pulling its weight), grows complexity, and the next fix will need a fix-up of the fix-up. Symptoms it leaks:

- a new pass with no clear stage number ("post-S12 cleanup", "pass 3.5") whose only job is to undo earlier diagnostics.
- a special-case in `validate_type_relations` that swallows specific shapes ("if both sides Generic{name=Map}, allow…").
- a `if expr_id in suppressed_set` early-return whose membership is decided by reading the AST again.
- an `is_assignable_to` rule that asymmetrically allows X→Y because some downstream call expected it; if the runtime rejects the same shape, it's the wrong fix.

When you see a wrong diagnostic, **first** ask: "which stage was supposed to know this, and why didn't it?" Then fix that stage. Examples of correct fixes from the recent ROADMAP work:

- `for-in` element typing wasn't reaching the body — bind the iterator params' `def_types` in the `Stmt::ForIn` arm of the body walker (P18.x), not by special-casing the diagnostic in `validate_type_relations`.
- C-style `for (var i = 0; …)` loop var was untyped inside the condition / body — bind `init_name` to declared-or-inferred type at the top of `Stmt::For` (P19.14), don't post-fix.
- `var x = nodeFn(); if (x == null) { x = bar(); } use(x);` — `x` should be non-null after the if. The fix lives in the if-handler's narrow-frame join (P19.16) — *that* is the analysis stage responsible for narrows. A `validate_type_relations`-side suppression list would be monkey-patching.
- post-S12 cross-module passes (`stage_cross_module_post_passes`) ARE legitimate when they fold in *foreign* information that S12 cannot know module-locally (e.g. resolving a Project-fallback ident's foreign decl). They are not legitimate when they re-walk the same module to second-guess what S12 already typed.

If you genuinely cannot find the right stage and need a temporary monkey-patch, leave a `// FIXME(monkey-patch): <stage that should own this>` comment AND open a ROADMAP entry for the proper fix. Do not let "temporary" survive a release.

A good gut-check: if your fix is in a *later* stage than where the type was introduced, justify the layering. The fix probably belongs earlier.

## Common commands

```sh
cargo build --workspace                               # build everything
cargo test  --workspace                               # run tests
cargo install --path greycat-analyzer --debug         # install CLI on host (binary: greycat-analyzer)

# CLI (binary self-identifies as `greycat-lang`)
cargo run -p greycat-analyzer -- lint project.gcl                      # @library/@include closure
cargo run -p greycat-analyzer -- lint project.gcl --fix                # apply auto-fixable lints
cargo run -p greycat-analyzer -- lint project.gcl --format=pretty      # miette rendering (default on TTY)
cargo run -p greycat-analyzer -- fmt                                   # cwd/project.gcl, write mode (default)
cargo run -p greycat-analyzer -- fmt path/to/project.gcl --mode=check  # exit non-zero on drift, list drifted files
cargo run -p greycat-analyzer -- fmt path/to/project.gcl --mode=diff   # unified diff per file (colored on TTY)
cargo run -p greycat-analyzer -- fmt path/to/file.gcl   --mode=stdout  # format only the entrypoint, print to stdout
cargo run -p greycat-analyzer -- server                                # LSP (alias: `lang-server`)
cargo run -p greycat-analyzer -- cst path/to/file.gcl                  # debug: print CST s-expr

# stdlib coverage (optional — needs greycat installed)
greycat install                                       # populates lib/std/
cargo test -p greycat-analyzer-syntax --test coverage

# wasm build (drives the playground)
playground/scripts/build-wasm.sh                      # wraps wasm-pack with the Emscripten sysroot

# playground (committed, separate npm project)
cd playground && pnpm install && pnpm dev

# parity oracle (P10.3 harness)
scripts/parity-oracle.sh <ts-lang-checkout> <corpus-dir>
```

## Adding / removing a lint rule

Whenever you add a new lint rule (or retire an existing one), visit all four touchpoints below in the same change. Forgetting any one silently breaks `--list-rules`, LSP completion, suppression validation, the auto-fix path, or the editor "dim unused code" UX.

| Touchpoint | File | When |
|---|---|---|
| `LINT_RULES` registry — name + one-line summary | [greycat-analyzer-analysis/src/lint.rs](../greycat-analyzer-analysis/src/lint.rs) | **Always.** Drives `lint --list-rules` and the LSP's `// gcl-lint-off …` rule-name completion. Both read straight from this slice — there is no second registry. |
| Emission wiring | HIR-only rules: add the struct to `default_rules()` in [greycat-analyzer-analysis/src/lint.rs](../greycat-analyzer-analysis/src/lint.rs). Typed / CST-shape passes: add the call to `run_typed_lints` in [greycat-analyzer-analysis/src/project.rs](../greycat-analyzer-analysis/src/project.rs), **and** add the rule name to that pass's `module.lints.retain` filter so re-emissions don't duplicate on incremental updates. | **Always.** `run_typed_lints` is the single unified emission point for typed / CST-shape lints — `stage_cross_module_post_passes` (full analyze) and `invalidate` (LSP `did_change`) both go through it. Wiring anywhere else (e.g. in `stage_lower` only) means the rule fires once on workspace open and **never again on edits**. The CLI `lint` command works either way; only the LSP exposes the gap. After adding a typed / CST-shape rule, verify by editing a file in the LSP and confirming the diagnostic survives a keystroke. |
| Auto-fix dispatch | [greycat-analyzer-analysis/src/quickfix.rs](../greycat-analyzer-analysis/src/quickfix.rs) (`edit_for_diagnostic` match + per-rule fn) | When the rule has an auto-fix. Both `lint --fix` and the LSP's `textDocument/codeAction` go through this single dispatch — there is no second one. |
| Editor "unused" tag | [greycat-analyzer-analysis/src/lint.rs](../greycat-analyzer-analysis/src/lint.rs) (`default_tag_for`) | When the rule represents "this code does nothing" (`unused-*`, `unreachable`, `redundant-*`). Editors render the span dimmed via LSP `DiagnosticTag::UNNECESSARY`. |

Removal-side: walk the same four files in reverse, then **grep the repo for the rule name** — fixture files under [greycat-analyzer-analysis/tests/](../greycat-analyzer-analysis/tests/), the conformance corpus, and snapshot tests reference rules by string, and the compiler cannot catch a stale literal.

## Adding a grammar keyword / statement

Whenever you teach the analyzer about a new reserved word or statement form (verified against the runtime per "Hard rule — never assume GreyCat syntax"), every layer below has to learn it in lockstep. Skipping one leaves a silently-broken slice — formatter strips it, LSP completion omits it, reachability mis-classifies it, or worst case the file parses as `ERROR` and lint/fmt refuse to run on the whole project. Treat the table as a checklist for one bundled change (the grammar piece commits in the submodule; the rest in the parent).

| Touchpoint | File | When |
|---|---|---|
| Grammar rule | [tree-sitter-greycat/grammar.js](../tree-sitter-greycat/grammar.js) (`_stmt` / `_expr` / `_decl` choice + new rule), regenerated `src/parser.c` + `src/node-types.json` via `npx tree-sitter generate` | **Always.** The grammar is the only source of truth for what parses. `word: $.ident` means a literal keyword token preempts the ident regex only if it appears in a rule — adding it to a choice array isn't enough on its own if the new rule is unreachable from `_stmt` / `_expr` / `source_file`. |
| Corpus regression test | [tree-sitter-greycat/test/corpus/](../tree-sitter-greycat/test/corpus/) (new `<name>.txt` file) | **Always.** One snippet per shape that matters — standalone, adjacent to other stmts (to prove not-a-terminator vs terminator status), and the negative case (typo / wrong context produces ERROR or unresolved-ident). Cheap, fast, runs via `npx tree-sitter test` before the Rust gauntlet even compiles. |
| Highlight query | [tree-sitter-greycat/queries/highlights.scm](../tree-sitter-greycat/queries/highlights.scm) | **Always.** Both the bare-token alternation (for fallback highlighting) AND the rule-scoped capture (e.g. `(breakpoint_stmt "breakpoint" @keyword.control)`). Editors using the tree-sitter highlights query rely on the rule-scoped form for accurate scope; omitting it makes the keyword highlight as a generic ident. |
| Submodule pointer bump | parent repo's `tree-sitter-greycat/` submodule SHA | **Always.** Without the bump the parent still sees the old grammar; the Rust port silently disagrees with the grammar on disk. Always push the submodule commit to `maxleiko/tree-sitter-greycat` *before* the bump, otherwise the parent points at a SHA no clone can fetch. |
| HIR variant + lowering | [greycat-analyzer-hir/src/types.rs](../greycat-analyzer-hir/src/types.rs) (new `Stmt` / `Expr` variant), [lower.rs](../greycat-analyzer-hir/src/lower.rs) (new CST-name arm) | **Always.** If the lowering arm is missing the construct lands as `Expr::Unsupported`, which the [unsupported_audit](../greycat-analyzer-hir/tests/unsupported_audit.rs) test catches — but only for expressions. Statement-level constructs without a lowering arm silently drop out of the HIR entirely; the test won't catch that. |
| Analysis `match` audit | every `match stmt` / `match expr.kind` site in [greycat-analyzer-analysis/src/](../greycat-analyzer-analysis/src/) — at minimum `analyzer.rs`, `resolver.rs`, `reachability.rs`, `lint.rs`, `project.rs` | **Always.** Rust will force you to handle the new variant in every exhaustive match, which is the safety net — *but* the right arm for control-flow constructs is load-bearing semantically. Decide deliberately: terminator (joins `Break` / `Continue` / `Return` / `Throw` in reachability)? non-terminating no-op? expression-shaped? Pick by checking runtime behavior, not by analogy to a sibling variant. |
| Formatter lowering | [greycat-analyzer-fmt/src/lower.rs](../greycat-analyzer-fmt/src/lower.rs) (new CST-name arm) | **Always.** Missing arm → formatter drops the statement on round-trip, lint --fix corrupts files, fmt --check reports drift forever. |
| CLI dump-types exhaustive arm | [greycat-analyzer/src/cmd/dump_types.rs](../greycat-analyzer/src/cmd/dump_types.rs) | **Always.** Forced by exhaustive match on `Stmt` / `Expr`. |
| WASM HIR bridge | [greycat-analyzer-wasm/src/lib.rs](../greycat-analyzer-wasm/src/lib.rs) (`HirNode` for the new variant) | **Always.** Forced by exhaustive match. The playground's HIR viewer renders whatever this produces. |
| LSP `ALL_KEYWORDS` (stmt/expr-position only) | [greycat-analyzer-server/src/capabilities/completion.rs](../greycat-analyzer-server/src/capabilities/completion.rs) (`ALL_KEYWORDS` slice + its doc comment) | When the keyword is completable at a generic stmt / expr position. Context-only keywords (slot-locked to a single grammar position like `extends` after a type name or `typeof` on a fn-param) belong in their respective contextual handlers, not this slice. |
| LSP `stmt_byte_range` arm | [greycat-analyzer-server/src/conv.rs](../greycat-analyzer-server/src/conv.rs) (the `match stmt` in `stmt_byte_range`) | When you added a new `Stmt` variant. If the HIR variant has no `byte_range` field (unit variant like `Break` / `Continue` / `Breakpoint`), the arm returns `0..0` and capability handlers fall back to the surrounding CST node — same shape as the existing precedent. |
| VSCode TextMate grammar | [editors/code/grammar/Greycat.tmLanguage.json](../editors/code/grammar/Greycat.tmLanguage.json) | When the keyword should highlight in editors that don't speak tree-sitter (the bundled VSCode extension uses TM, not tree-sitter, for syntax). Add to whichever alternation matches the keyword's category (control-flow exit-shape, declaration, modifier, etc.). |
| Snippets (optional) | [editors/code/snippets/snippets.json](../editors/code/snippets/snippets.json) | When the keyword is verbose enough that a short trigger helps (`bp` → `breakpoint;`). |

Verification before commit: `cd tree-sitter-greycat && npx tree-sitter test && npx tree-sitter parse project.gcl | grep -c ERROR` (expect `0`); from the parent, `cargo test --workspace` (the unsupported_audit and any new corpus tests run here) and a manual `cargo run -p greycat-analyzer -- fmt path/to/snippet.gcl --mode=stdout` round-trip on a file using the new construct.

Removal-side: same walk in reverse, then `git grep` for the rule name (e.g. `breakpoint_stmt`, `Stmt::Breakpoint`) — corpus files, fixture snapshots, and editor grammars all reference grammar / HIR names by string and the compiler cannot catch a stale literal there.

## Adding / removing an LSP capability

Whenever you wire a new LSP request kind (or retire one), the same touchpoints repeat. Forgetting any one silently breaks dispatch, tests, the WASM playground, or capability advertisement.

| Touchpoint | File | When |
|---|---|---|
| **Decide where the logic lives.** Pure LSP shape work (e.g. folding ranges) → file in `capabilities/`. Analysis work (scope walking, name binding queries, member discovery) → service in `greycat-analyzer-analysis::<capability>`, with a thin `capabilities/` wrapper. The [`analysis::rename`](../greycat-analyzer-analysis/src/rename.rs) ↔ [`capabilities::references_rename`](../greycat-analyzer-server/src/capabilities/references_rename.rs) split is the reference shape. | — | **Always.** If you can't decide in 30 seconds, default to analysis-side — it's easier to demote from analysis to LSP than to migrate a capability that already inlined LSP types into the analysis crate. |
| Implementation file | [src/capabilities/`<name>`.rs](../greycat-analyzer-server/src/capabilities/) — one file per capability, named after the LSP method (`hover.rs`, `goto.rs`, etc.). Existing per-capability files re-exported from [src/capabilities/mod.rs](../greycat-analyzer-server/src/capabilities/mod.rs). | **Always.** Public entry point name pattern: `<capability>_with_project` (request-shaped data + `&ProjectAnalysis` + `&SourceManager`) for per-document requests; `<capability>_across_project` for project-wide ones (references, rename, goto-impl). |
| `mod.rs` re-export | [src/capabilities/mod.rs](../greycat-analyzer-server/src/capabilities/mod.rs) (`mod` decl + `pub use`) | **Always.** `mod.rs` is a re-export hub only — no logic. Keep it that way. |
| Server dispatch | [src/server.rs](../greycat-analyzer-server/src/server.rs) (`<name>_handler` fn + the `try_handle::<…>()` wiring in `main_loop`) | **Always.** Handler resolves `Backend::project_for(&uri)`, fetches the `Document` via `manager.get(&uri)?.borrow()`, forwards to `capabilities::<name>_with_project`. Mirrors the shape of every other handler. |
| Capability advertisement | [src/server.rs](../greycat-analyzer-server/src/server.rs) (the `ServerCapabilities { … }` literal returned from `initialize`) | When the capability needs client opt-in (semantic tokens registration, completion trigger characters, code-action kinds, etc.). |
| Test fixture method | [tests/support/mod.rs](../greycat-analyzer-server/tests/support/mod.rs) (`impl TestProject`) | **Always when there's a `*_with_project` variant.** Add a one-line shortcut (`fn <name>(&self, …) -> …`) so tests read `project.<name>(pos)` instead of unwrapping doc.text / root / project / manager at every call site. |
| WASM bridge (optional) | [greycat-analyzer-wasm/src/lib.rs](../greycat-analyzer-wasm/src/lib.rs) | When the capability should be reachable from the playground (hover / completion / diagnostics / inlay hints today). Skip for capabilities that are LSP-only. |

Verification before commit: `cargo test --workspace` (the `TestProject`-based integration tests under `greycat-analyzer-server/tests/` are the regression net); `cargo clippy --workspace --all-targets` clean; manual LSP smoke against VSCode if it's a user-visible capability.

Removal-side: walk the same touchpoints in reverse. **Watch out for the LSP↔analysis boundary** — if the capability had an analysis-side service, delete the analysis module too (otherwise the API stays exposed for no caller). `git grep` the analysis fn names to confirm no other capability borrowed them mid-flight.

## Conventions

- **`lsp_types` is re-exported from `greycat-analyzer-core`** — depend on `greycat_analyzer_core::lsp_types` from downstream crates so versions stay in lockstep.
- **Project entrypoint, not directory walk.** Always go through `SourceManager::load_project` (see "Project model"). The CLI was previously buggy on this front; don't reintroduce flat directory walks.
- **One LSP path: the cached `*_with_project` / `*_across_project` capabilities.** The legacy single-file shims have been deleted. New capability code goes through `ProjectAnalysis` even on the first commit; if a test thinks it needs single-file mode, use [`TestProject`](../greycat-analyzer-server/tests/support/mod.rs) instead.
- **Position / byte-range math lives in [`src/conv.rs`](../greycat-analyzer-server/src/conv.rs).** Don't reimplement `position_to_byte` / `byte_to_position` / `byte_range_to_lsp` inline — every capability uses the same conversion.
- **Analysis work belongs in the analysis crate.** If a capability handler is walking scope, enumerating type members, resolving cross-module names, or doing flow inference, it's analysis work. Move it to `greycat-analyzer-analysis::<capability>` and keep the LSP layer doing shape conversion. The [`analysis::rename`](../greycat-analyzer-analysis/src/rename.rs) split is the reference pattern.
- **Gitignored at repo root:** `/target`, `/gcdata`, `/files`, `/lib`, `/webroot`, `/bin`, `/greycat-analyzer-wasm/pkg`, `/playground/{node_modules,dist}`. The root [project.gcl](../project.gcl) IS committed — it pins the stdlib version via `@library("std", "...")` and drives `greycat install`.
- **Conformance corpus:** vendored TS reference parser/project fixtures live at [tests/corpus/](../tests/corpus/). Stdlib (`lib/std/*.gcl`) is *not* vendored — populate via `greycat install`. The coverage gauntlet handles both.
- **`Expr::Unsupported` is a regression marker.** [greycat-analyzer-hir/tests/unsupported_audit.rs](../greycat-analyzer-hir/tests/unsupported_audit.rs) asserts the histogram is empty over stdlib + corpus. If a lowering change re-introduces an Unsupported kind, that test will fail.
- **`Definition::Project` is the cross-module fallback.** Capabilities that need scope-aware behavior (rename, references, goto-def) consult the resolver first; only use text-equality across modules for `Project` until P8.x cross-module decl pointers land.
- **Keep ROADMAP phase markers OUT of doc comments.** `///` and `//!` are public API documentation — they end up in `cargo doc` and ship to consumers who don't care about our internal phase numbering. Phase markers (`P19.6`, `**P22.1** —`, `(P15.7 + P16.4)`, etc.) belong in a regular `// PNN.N` line *adjacent* to the doc comment, never inside the `///` / `//!` text. Same applies to README.md and any other consumer-facing prose. Pattern: `// P19.6` on its own line, then the `/// ...` block immediately below it.
- **LICENSE:** dual MIT / Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE` at workspace root).

## Hard rule — never push with clippy warnings

`cargo clippy --workspace --all-targets` must report **zero warnings**, not just zero errors, before any commit lands on `main` (or any branch that will be pushed). This applies to chunk commits, `chore:` / `fix:` commits, submodule pointer bumps, every push.

**Why:** clippy warnings are how the workspace's invariants (no needless clones on `Copy` types, no `iter().any()` when `contains` exists, no oversized signatures, no orphan doc comments) stay enforced. Each one ignored decays the bar toward "well, it was already noisy" — and once the noise floor rises, real warnings hide in it. The per-chunk checklist step 3 has always said this; promoting it to a hard rule means it sits next to the grammar / syntax-assumption / monkey-patch rules and gets the same gravity.

**How to apply:** the per-chunk loop is `fmt → build → clippy → test`. If clippy reports warnings (your code's *or* pre-existing), fix them in the same commit. Never push and "fix in follow-up." If a warning genuinely warrants an exception (e.g. the existing `#[allow(clippy::too_many_arguments)]` precedent on deep analysis helpers in `project.rs`), gate it behind a narrowly-scoped `#[allow(...)]` on the specific item, never at crate or workspace level. Auto-mode does not waive this.

## Commit cadence (ROADMAP execution)

While executing [ROADMAP.md](../ROADMAP.md), **one commit per chunk** (the `[ ]` items inside each phase). This keeps the history bisectable and lets the user review the port one workpackage at a time.

Standing authorization: commit and **push** per chunk without asking each time. There's no reason to keep a chunk commit local once it's committed; CI / collaborators see it sooner.

Per-chunk checklist:

1. `cargo fmt --all` (no `--check` — just run it). CI runs `cargo fmt --all --check` and a single drifted file fails the build.
2. `cargo build --workspace` clean.
3. `cargo clippy --workspace --all-targets` clean (zero warnings, not just zero errors).
4. `cargo test --workspace` clean (or, for chunks that intentionally don't add tests, the existing tests still pass).
5. Tick the chunk in ROADMAP.md (`[ ]` → `[x]`) in the same commit.
6. Stage only the files relevant to the chunk; never `git add -A`.
7. Don't skip hooks, don't amend prior commits, don't pass `-c commit.gpgsign=false` (signing is off globally; the flag is unnecessary noise).
8. `git push origin main` after the commit lands. Same applies to `chore:` / area-prefixed non-chunk commits in this repo.

Submodule commits (inside `tree-sitter-greycat/` → `maxleiko/tree-sitter-greycat`) are also pushed immediately. After pushing, bump the parent's submodule pointer in a follow-up commit so the new SHA propagates downstream.

Commit message format — match the existing log style (short, lowercase, area-prefixed):

```
P<phase>.<chunk>: <terse summary>
```

Examples:
- `P0.1: workspace re-shape, add greycat-analyzer-syntax crate`
- `P0.3: port Document/Manager to tree-sitter Tree`
- `P2.4: type system core — Type enum, subtyping, generics`

Body is optional; add one when the *why* isn't obvious from the diff. No `Co-Authored-By` footer — repo history doesn't use them.

If a chunk is too big for one commit, split it in ROADMAP.md first, then commit per sub-chunk. Do not bundle multiple chunks unless the user explicitly asks for a batched commit.

Non-chunk work (workflow setup, dependency bumps, doc fixes, bug fixes that aren't tied to a roadmap chunk) uses a `chore:` / `fix:` / area prefix instead of `P<phase>.<chunk>:`.

## GreyCat language

When reading or generating `.gcl`, invoke the `/greycat:greycat` skill — it has the full language reference (nodes, node collections, nullability, type system, annotations, common pitfalls). Don't re-derive language rules from the codebase.

Quick reminders for analyzer work:
- Field access on inner type: `n->name`. Method on the node itself: `n.resolve()`.
- Native types (`geo`, `time`, `duration`) have no fields — methods only.
- `Array<T>{}`, not `Array<T>::new()`. No ternary. No `void` keyword.
- `function` parameter type is opaque; calls return `any?` and require casting.
- Built-in runtime type names (`Array`, `Map`, `Set`, `node`, `nodeTime`, `nodeGeo`, `nodeList`, `nodeIndex`, `function`, `tuple`, `field`, `t2`-`t4`, `t2f`-`t4f`) are seeded into `ProjectIndex::new()` and resolve through `Definition::Project`. They are *not* declared in `.gcl` — they live in the GreyCat runtime.
