# greycat-analyzer

Rust port of the [GreyCat](https://greycat.io) language frontend: static analyzer, LSP server, formatter, linter. Targets `.gcl` source. Distributed as a CLI binary, an LSP server, a WASM build, and library crates.

The reference implementation is the TypeScript monorepo at `https://hub.datathings.com/greycat/lang`. The Rust port matches its frontend; no runtime/VM is in scope.

Rust edition 2024. Workspace resolver `"3"`. Workspace metadata (`license = "MIT OR Apache-2.0"`, repo, authors) lives in `[workspace.package]` and every crate inherits via `*.workspace = true`.

## Workspace layout

| Crate | Purpose |
|---|---|
| [greycat-analyzer-syntax](../greycat-analyzer-syntax/) | Tree-sitter wrapper. Owns parsing via [tree-sitter-greycat](../tree-sitter-greycat/) (git submodule at the repo root). |
| [greycat-analyzer-core](../greycat-analyzer-core/) | `Document`, `SourceManager`, `span`, project graph, `@library` / `@include` resolver, parse diagnostics. Re-exports `lsp_types` and `greycat_analyzer_syntax`. |
| [greycat-analyzer-hir](../greycat-analyzer-hir/) | Arena-backed typed HIR, CST→HIR lowering. |
| [greycat-analyzer-types](../greycat-analyzer-types/) | `Type` / `TypeKind`, `TypeArena`, subtyping (`is_assignable_to`), `InferenceTable` foundation. |
| [greycat-analyzer-analysis](../greycat-analyzer-analysis/) | Resolver, analyzer (inference + null-flow + `is`-narrowing + enum exhaustiveness + member resolution), lints, `ProjectAnalysis` driver, `ProjectIndex` cross-module index, `actions` vocabulary. |
| [greycat-analyzer-fmt](../greycat-analyzer-fmt/) | Formatter (foundational; per-construct parity with TS `cst_format.ts` is P9.1). |
| [greycat-analyzer-server](../greycat-analyzer-server/) | LSP server (`lsp-server` + `crossbeam-channel`). Capability handlers in [src/capabilities.rs](../greycat-analyzer-server/src/capabilities.rs). |
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

## Conventions

- **`lsp_types` is re-exported from `greycat-analyzer-core`** — depend on `greycat_analyzer_core::lsp_types` from downstream crates so versions stay in lockstep.
- **Project entrypoint, not directory walk.** Always go through `SourceManager::load_project` (see "Project model"). The CLI was previously buggy on this front; don't reintroduce flat directory walks.
- **Gitignored at repo root:** `/target`, `/gcdata`, `/files`, `/lib`, `/webroot`, `/bin`, `/greycat-analyzer-wasm/pkg`, `/playground/{node_modules,dist}`. The root [project.gcl](../project.gcl) IS committed — it pins the stdlib version via `@library("std", "...")` and drives `greycat install`.
- **Conformance corpus:** vendored TS reference parser/project fixtures live at [tests/corpus/](../tests/corpus/). Stdlib (`lib/std/*.gcl`) is *not* vendored — populate via `greycat install`. The coverage gauntlet handles both.
- **Examples** for ad-hoc parsing live in [examples/](../examples/). Use these as inputs when smoke-testing parser changes.
- **`Expr::Unsupported` is a regression marker.** [greycat-analyzer-hir/tests/unsupported_audit.rs](../greycat-analyzer-hir/tests/unsupported_audit.rs) asserts the histogram is empty over stdlib + corpus. If a lowering change re-introduces an Unsupported kind, that test will fail.
- **`Definition::Project` is the cross-module fallback.** Capabilities that need scope-aware behavior (rename, references, goto-def) consult the resolver first; only use text-equality across modules for `Project` until P8.x cross-module decl pointers land.
- **Keep ROADMAP phase markers OUT of doc comments.** `///` and `//!` are public API documentation — they end up in `cargo doc` and ship to consumers who don't care about our internal phase numbering. Phase markers (`P19.6`, `**P22.1** —`, `(P15.7 + P16.4)`, etc.) belong in a regular `// PNN.N` line *adjacent* to the doc comment, never inside the `///` / `//!` text. Same applies to README.md and any other consumer-facing prose. Pattern: `// P19.6` on its own line, then the `/// ...` block immediately below it.
- **LICENSE:** dual MIT / Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE` at workspace root).

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
