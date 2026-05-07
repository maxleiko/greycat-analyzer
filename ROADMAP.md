# ROADMAP — Rust port of GreyCat tooling

## Purpose

This is the long-arc plan for porting the [GreyCat](https://greycat.io) language frontend to Rust from scratch. The reference implementation is a TypeScript monorepo at `/home/leiko/dev/datathings/greycat/lang` (~46k LoC of compiler frontend, ~11k LoC of tests, ~3.3k LoC of `.gcl` standard library).

**What this is:** a static analyzer + LSP server + formatter + linter for `.gcl` source code, distributed as a CLI binary, an LSP server, a WASM build for browsers, and a set of library crates on crates.io.

**What this is not:** a runtime, VM, JIT, or persistence engine. Execution lives in the separate GreyCat runtime fetched from `get.greycat.io`.

**How to read this doc:** §3 (decisions) and §6 (phases) carry the most weight. Each phase ends on a demoable milestone (M1–M5). Update this doc in-place as work lands — check off chunks, fold Open Questions into Decisions when answered.

---

## 2. Source-of-truth map

Each TypeScript subsystem in `packages/lang/src/` (the reference implementation) maps to a target Rust crate. LoC is the TS source line count and indicates relative effort.

| TS subsystem | Path in `packages/lang/src/` | TS LoC | Target Rust crate |
|---|---|---|---|
| Lexer / tokenizer | `lexer/` | 1,434 | `tree-sitter-greycat` (external) |
| Parser (AST + CST, dual tree) | `parser/` | 9,970 | `greycat-analyzer-syntax` |
| Type system, generics, inference | `analysis/types.ts` | 2,811 | `greycat-analyzer-types` |
| Analyzer (control flow, narrowing, errors) | `analysis/analyzer.ts` | 4,514 | `greycat-analyzer-analysis` |
| Resolver (name binding) | `analysis/resolver.ts` | 1,145 | `greycat-analyzer-analysis` |
| Environments / scopes | `analysis/environment.ts`, `env_manager.ts` | 890 | `greycat-analyzer-analysis` |
| Analysis utilities | `analysis/utils.ts` | 753 | `greycat-analyzer-types` |
| Visitors (8 patterns) | `visitor/` | 3,399 | `greycat-analyzer-syntax` (queries) + `greycat-analyzer-hir` |
| Pretty printer / formatter | `pp/` + `parser/cst/cst_format.ts` | 779 | `greycat-analyzer-fmt` |
| Project manager (multi-module, dep graph) | `project/` | 3,969 | `greycat-analyzer-core` |
| LSP capability handlers | `lsp/` (+ 1,527 LoC of tests) | 49 | `greycat-analyzer-ls` |
| LSP server transport | `packages/server/` | 1,228 | `greycat-analyzer-ls` |
| CLI driver | `packages/cli/` | 743 | `greycat-analyzer` (bin) |
| Linter | `packages/cli/src/lint/` | 242 | `greycat-analyzer-analysis` (rules) + `greycat-analyzer` (CLI) |
| Module resolver (`@library`, `@include`) | `packages/resolver/` | 92 | `greycat-analyzer-core` |
| Error infrastructure | `errors.ts` | 145 | `greycat-analyzer-analysis` (or shared) |
| Highlighter (semantic tokens) | `highlighter.ts` | 141 | `greycat-analyzer-ls` |
| Stdlib (in GreyCat itself) | `lib/std/*.gcl` | 3,314 | vendored corpus (not ported) |

---

## 3. Architectural decisions (locked)

These decisions were made during planning and are not revisited without explicit cause. New evidence overrides; default direction does not.

| # | Decision | Rationale |
|---|---|---|
| **A** | Tree-sitter raw + generated typed nodes; **no rowan/syntree facade.** | Tree-sitter already provides lossless trivia, incremental reparse, and a green/red tree. Layering rowan on top doubles memory cost and complicates `tree.edit()`. Typed accessors are generated in a small `build.rs` from `node-types.json` (~300 LoC vs. several thousand hand-maintained). |
| **B** | Single typed HIR + type arena; **not** layered hir-def/hir-ty (rust-analyzer style). | GreyCat's surface is much smaller than Rust's: no macros, no traits, no bounds-density. The TS reference uses a single typed tree. HIR lives in its own crate so a future split is mechanical. |
| **C** | Hand-rolled file-level invalidation now; **salsa deferred** to P5 (or never). | Salsa pays off with diamond-shaped query graphs. At this scope, file-level invalidation is enough. Wire incremental boundaries by pure function now so a salsa retrofit later is cheap. |
| **D** | 9-crate split (see §5). | Lets WASM ship only what it needs, lets LSP and CLI share semantics without dragging in syntax internals. The current 4-crate layout is wrong long-term. |
| **E** | Snapshot-against-TS reference as the parity oracle; **do not port TS tests verbatim.** | 11k LoC of TS tests assert TS API shapes that won't exist in Rust. Port the *intent* into Rust-idiomatic tests; use snapshot diffs for parity. |
| **F** | Stdlib (`lib/std/*.gcl`) vendored as ordinary modules; native-bound functions captured as a small Rust metadata table. | Stdlib is the canonical correctness corpus; analyzer must understand it. Runtime-implemented functions need only signature data, not bodies. |
| **G** | Lexer module deleted; **tree-sitter owns scanning.** | Tree-sitter has its own scanner including an external scanner for complex string handling. A separate Rust lexer is duplicate work. |

---

## 4. Open questions

Resolved as we hit them; fold the answer into §3 when locked.

- **Q1:** How do we expose runtime-only built-in functions (those whose body is in the GreyCat runtime, not in `.gcl`) so the analyzer type-checks calls to them? Probable shape: a hand-written Rust table keyed by canonical name, with signatures parsed from a stub `.gcl` file. To resolve in P2.6.
- ~~**Q2:** Where does the conformance corpus live?~~ **Resolved (P0.5).** Two-tier:
  1. **TS reference parser/project fixtures** — vendored at `tests/corpus/{parser,project}_fixtures/` (analyzer-port artifact, committed).
  2. **Stdlib (`lib/std/*.gcl`)** — *not* vendored. Repo-root `project.gcl` pins the version via `@library("std", "<release>")`, and `greycat install` populates `lib/`. The coverage gauntlet checks stdlib if present, skips with a notice if not.
  No `STDLIB_VERSION` file — the pin lives in `project.gcl`.
- **Q3:** Version-pinning policy with upstream `lang/` — when does the Rust port chase a new TS reference release vs. lock to a known-good commit? To resolve before M3.
- **Q4:** Tree-sitter grammar gaps — fix upstream in `tree-sitter-greycat` or work around in the syntax wrapper? Decide per-gap. **Mechanism (resolved P0.5):** the coverage gauntlet keeps a `KNOWN_GRAMMAR_GAPS` allowlist in `greycat-analyzer-syntax/tests/coverage.rs`. Each entry pins a workspace-relative file path with a comment describing the rule that needs upstream relaxation. Drop entries from the list as the grammar is fixed. **Current backlog:** 1 entry — `tests/corpus/parser_fixtures/inline_type/in.gcl` (last `type_attr` should not require trailing `;`).
- **Q5:** WASM bundle size budget. Splitting `analysis` from `core` (Decision D) helps; quantify in P5.1.

---

## 5. Crate layout (target)

| Crate | Purpose | Source |
|---|---|---|
| `greycat-analyzer-syntax` | Tree-sitter wrapper, generated typed nodes, span/line-index | new |
| `greycat-analyzer-core` | `SourceManager`, `Document`, `Manager`, project graph, module resolver | survives, slimmed |
| `greycat-analyzer-hir` | HIR types, CST→HIR lowering | new |
| `greycat-analyzer-types` | `Type`, unifier, inference table | new |
| `greycat-analyzer-analysis` | Resolver, analyzer, narrowing, lint rules | new |
| `greycat-analyzer-fmt` | Formatter | new |
| `greycat-analyzer-ls` | LSP server | survives |
| `greycat-analyzer` | CLI binary | survives |
| `greycat-analyzer-wasm` | WASM bindings | survives |

Dependency direction: `syntax` → `core` → `hir` → `types` → `analysis` → {`ls`, `cli`, `wasm`, `fmt`}.

---

## 6. Phases

Each phase ends on a milestone. Effort signals: **S** < 1 week, **M** ~1-2 weeks, **L** ~2-4 weeks, **XL** ~1-2 months (single dev, ported-from-TS pace).

### Phase 0 — Foundation reset (~4-6 weeks)

**Goal:** retire the hand-rolled parser, stand up tree-sitter as the single source of syntax truth, keep LSP responsive throughout.

**Chunks:**

- [x] **0.1 Workspace re-shape** (S) — add `greycat-analyzer-syntax` crate; demote `greycat-analyzer-core` to "semantic glue" (will later host HIR/types).
- [x] **0.2 Tree-sitter integration** (M) — vendor or git-dep `tree-sitter-greycat`; expose `Language`, parse function. Generate a typed-node wrapper layer in `build.rs` from `node-types.json`. (Decision A.)
- [x] **0.3 Document/Manager port** (M) — replace bumpalo-CST inside `Document` with tree-sitter `Tree`; keep `LineIndex` and `apply_changes`, but call `tree.edit()` + `parser.parse(&new_text, Some(&old_tree))` for incremental reparse.
- [x] **0.4 Retire old code** (S) — see §9 for the explicit deletion list.
- [x] **0.5 Coverage gauntlet** (S) — bulk-parse `lib/std/*.gcl` + every `.gcl` under TS reference fixtures. Assert zero `ERROR`/`MISSING` nodes. File grammar gaps upstream against `tree-sitter-greycat`.
- [x] **0.6 Snapshot harness** (S) — `insta` wired over `tests/corpus/` with an indented s-expression printer (`greycat-analyzer-syntax/tests/snapshot.rs`). The TS-vs-Rust *diff* half of the parity oracle (§7-A) lands at the layers where both sides produce comparable artifacts — diagnostics JSON (P1.4) and formatter output (P4.1). Tree-sitter's CST has no TS-side analogue, so raw-CST cross-port diffing is intentionally not in scope here; the Rust-side snapshots still catch grammar bumps, `tree.edit()` glitches, and accidental whitespace changes.

**Files retired:** see §9.
**Files added:** `greycat-analyzer-syntax/` crate, `tests/corpus/` (vendored TS reference parser/project fixtures), `project.gcl` (repo-root, pins stdlib via `@library`).

**M1: tree-sitter parses 100% of `lib/std/*.gcl` and the TS reference test fixtures with zero error nodes; LSP stays alive on edits, diagnostics still empty; snapshot harness green.**

---

### Phase 1 — Project model + parse diagnostics (~2-3 weeks)

**Goal:** rebuild the multi-module project layer (TS `packages/lang/src/project/`) and start surfacing real diagnostics.

**Chunks:**

- [x] **1.1 Module resolver** (S) — port `packages/resolver/` (~92 LoC); `@library/...` + `@include/...` resolution (pure path math). Lives at `greycat-analyzer-core::resolver`: `Context` trait (`read` / `iter_gcl` / `is_dir` / `greycat_home`), `FsContext` impl, `try_greycat_home()` (env or `$HOME/.greycat`), path helpers (`library_dir`, `global_std_dir`, `include_dir`, `installed_file_path`), `parse_installed_file`. The cli `lint` walker now goes through `Context::iter_gcl` so `node_modules`/`gcdata`/`.git` are skipped.
- [x] **1.2 Source manager** (M) — `Manager` renamed to `SourceManager`, gains a `Context` and per-document `lib` label. `module_desc` module walks `mod_pragma` nodes to extract `@library` / `@include` / others. `SourceManager::load_project` does the recursive load over `Context::iter_gcl`, with a path-keyed visited set for cycle safety. Project-graph data model and TS-style diagnostics are deferred — they ride on top in P1.4 and P2.
- [x] **1.3 Workspace-folder loading** (S) — `Backend::initialized` now resolves each workspace-folder URI to a local path, looks for `project.gcl` at the root, and calls `SourceManager::load_project`. Unresolved libraries / fs errors are logged via `warn!` for now; typed LSP publication of those lands in P1.4.
- [x] **1.4 Parse diagnostics + LSP publish** (S) — `core::diagnostics::parse_diagnostics` walks ERROR / MISSING nodes and emits `lsp_types::Diagnostic` (severity ERROR, source `greycat-analyzer`, code `parse-error` / `missing-token`). LSP `Backend` publishes on `did_open`, `did_change`, `did_save`, clears on `did_close`, and pre-publishes for every file loaded by the workspace recursive load. Cli `lint` now prints `path:line:col: error: …` per diagnostic and returns `ExitCode::FAILURE` when any diagnostics surface.
- [x] **1.5 CST utility surface** (S) — `greycat-analyzer-syntax::cst` exposes `node_at_offset`, `ancestors`, `children_by_field`, `text_of`, and a `walk_named` pre-order traversal that supports skipping sub-trees. Thin extension layer over `tree_sitter::Node`; no wrapper type. 6 unit tests.

**M2: open a workspace with `project.gcl`; LSP shows red squiggles for all syntax errors across all reachable modules; `cli lint path/` exits non-zero with formatted diagnostics matching TS reference shape.**

---

### Phase 2 — Semantic layer (~10-16 weeks, the bulk)

**Goal:** port `packages/lang/src/analysis/` (~10k TS LoC). This phase dominates the project.

**Chunks:**

- [x] **2.1 HIR scaffolding** (L) — `greycat-analyzer-hir` ships an arena-backed HIR (`Idx<T>` newtype, append-only `Arena<T>`) covering Module / Decl (Fn / Type / Enum / Var / Pragma) / Stmt (block, var, if, while, do-while, for, for-in, return, break, continue, throw, try, at, assign, expr) / Expr (idents, literals, strings, tuples, arrays, members, arrow, static, offset, calls, binary, unary, paren, lambda, object) / TypeRef. Lowering walker (`lower_module`) is tolerant — unrecognized constructs land as `Expr::Unsupported { kind, range }` so downstream phases can still skip rather than panic. Tested against the vendored corpus + unit fixtures (5 unit + 1 corpus integration test).
- [x] **2.2 Crate split** (S, parallel with 2.1) — add `greycat-analyzer-hir`, `-types`, `-analysis`. Final layout per §5. Done up-front so P2.1 lands HIR types directly in their target crate; populated by P2.1 / P2.3 / P2.4 / P2.5.
- [x] **2.3 Symbol resolver / name binding** (L) — `analysis::resolver` walks HIR and produces a `Resolutions` table mapping each `Idx<Ident>` use site to a `Definition` (Decl / Local / Param / Builtin). Two-pass at module scope so forward references between top-level decls work. Builtin type names from the TS `StdCoreTypes` interface are pre-seeded so `int`/`String`/`Array` etc. don't show as unresolved before P2.6 imports stdlib. Member-access property names are intentionally *not* bound — that's type-driven, lands in P2.5. 5 unit tests cover param binding, forward refs, unresolved-name reporting, local-var shadowing, and type-ref head resolution.
- [x] **2.4 Type system core** (XL) — `greycat-analyzer-types` ships the foundation port: `Type { kind, nullable }` with `TypeKind` covering Null / Any / Never / Primitive / Named / Generic / GenericParam / Lambda / Tuple / Anonymous / Enum / Union; an interning `TypeArena` keyed by `TypeId(u32)`; a `TypeRegistry` for module-level Named lookups; and `is_assignable_to` covering primitive widening (int→float), null-into-nullable, any/never extremes, generic invariance, lambda contravariant-params + covariant-return, tuples element-wise, and unions. 11 unit tests. Inference table / unification beyond simple substitution lives in P2.5; full TS subtyping nuances around node tags / tagged generics fold in alongside the analyzer rules.
- [x] **2.5 Analyzer** (XL, foundational pass) — `analysis::analyzer` walks the HIR after the resolver, infers a `TypeId` per expression into `expr_types`, tracks per-binding types in `def_types`, and emits `SemanticDiagnostic` for assignment / return / condition mismatches and unresolved names. Covers literals, binary ops (with int→float widening + bitwise + boolean + coalesce), unary (`!`/`-`/`!!` strips nullable), member-access head, calls, lambdas, tuples, arrays, parens, and the full statement set. **Deferred** (each lands as the corpus or a Phase-3 capability requires it): control-flow narrowing (`if x != null` → x is non-null in then-branch), exhaustiveness checking for enums / unions, unused-decl warnings, and the deeper `declarator.ts`/`hinter.ts`/`actions.ts` ports. 5 unit tests cover clean source, return-type mismatch, if-condition mismatch, unresolved-name promotion, and int→float widening.
- [x] **2.6 Stdlib ingestion** (M) — `analysis::stdlib::ProjectIndex` is the cross-module index that holds a shared `TypeArena` + `TypeRegistry` + `NativeRegistry`. `ProjectIndex::ingest(&Hir)` walks a stdlib (or any) module's top-level decls and registers types / enums / native function signatures. Re-entrant. Decision F: native-bound functions get a small `NativeSignature` table — signatures only, no bodies. The actual file-system load of `lib/std/*.gcl` reuses `SourceManager::load_project` (P1.2). 4 unit tests cover type registration, enum variant capture, native signature ingestion, and re-entrancy.
- [x] **2.7 Semantic diagnostics → LSP** (S) — `Backend::publish_for` now runs the full pipeline (HIR lower → resolver → analyzer) on the parsed tree and merges semantic diagnostics into the LSP publish alongside parse diagnostics. Severities map onto `lsp_types::DiagnosticSeverity`, `code` is `"semantic"`, byte ranges are converted to LSP positions via a `position_at` walker. The LS crate gained dependencies on `greycat-analyzer-{syntax,hir,analysis}` to wire this together.

**M3: `cargo run -- check lib/std/*.gcl` reports zero diagnostics; LSP shows semantic errors on a deliberately broken user file.**

---

### Phase 3 — LSP capabilities (~4-6 weeks)

**Goal:** light up the 15 capabilities tested in `lsp.*.test.ts`.

Once Phase 2 lands, each capability is a thin wrapper over HIR + reference index + types.

**Chunks (each S–M):**

- [x] **3.1** Hover + signature help — `capabilities::hover` walks ancestors finding the smallest HIR expression that covers the cursor and renders a markdown popup with `<short-label>: <inferred-type>`. Falls back to `kind name` for declaration names. `capabilities::signature_help` walks up to the enclosing `call_expr`, looks up the matching `fn_decl`, and renders the signature with parameter labels via `ParameterLabel::LabelOffsets`.
- [x] **3.2** Goto definition + goto implementation — `capabilities::goto_definition` consumes the resolver's `Definition` for the ident at the cursor and returns a `Location` to the defining ident's range. `gotoImplementation` reuses the same handler (P3.2 scope: methods don't yet have separate impls vs. decls).
- [x] **3.3** Document symbols + workspace symbols — `capabilities::document_symbols` builds a nested `DocumentSymbol` tree for the module's top-level decls plus type-attrs and methods as children. Workspace symbols re-use the document-symbols engine across the SourceManager.
- [x] **3.4** Find references + rename (M) — `references` and `rename` walk the CST for every `ident` whose source text matches the cursor's, building Locations / TextEdits respectively. `prepare_rename` advertises the renamable range with the current name as placeholder. Cross-module / scope-aware renaming arrives once multi-module reference index lands.
- [x] **3.5** Document highlight + selection ranges + folding ranges — pure CST, no analysis pass: highlights = same-text idents in the file; selection ranges = ancestor chain from the leaf node; folding ranges = `block` / `type_body` / `enum_body` / `object_initializers` spans more than one line.
- [x] **3.6** Code actions + quickfixes (M) — emits one quickfix per overlapping semantic diagnostic in the requested range. Empty edits today — concrete fix synthesis (e.g. "add missing `;`") arrives alongside the linter rules in P4.2.
- [x] **3.7** Inlay hints — emits a `: <type>` annotation after every `var` whose type is inferred (no declared annotation, has an initializer). Anchored on the variable's name end position. Range filter respects the client's request range.
- [x] **3.8** Semantic tokens (M) — walks named tree-sitter nodes, looks up each ident through resolver `Definition`s, and emits typed tokens (FUNCTION / TYPE / ENUM / VARIABLE / PARAMETER) plus literal/comment tokens. Encodes deltas per LSP semantic-tokens spec; legend advertised in `initialize`.

**M4: every LSP capability the TS server advertises is wired and returns non-empty results on a sample workspace; ported `lsp.*.test.ts` scenarios pass as Rust integration tests.**

---

### Phase 4 — Formatter + linter + CLI parity (~3-4 weeks)

**Chunks:**

- [x] **4.1 Formatter** (M, foundational) — new `greycat-analyzer-fmt` crate ships a tree-sitter-driven pretty printer (`format` / `format_tree`). Walks the CST in source order, applies per-token rules (open-brace → indent + newline; semicolon → trim+newline; comma → ", "; member-access → no surrounding spaces) for normalized output. Round-trips representative fixtures through `parse → fmt → parse` cleanly and is idempotent on simple inputs. Wired to cli `fmt` (with `--check` mode that exits non-zero on drift) and LSP `textDocument/formatting`. **Byte-for-byte parity with the TS prettifier (the M5 acceptance criterion) is not yet met** — the TS port at `parser/cst/cst_format.ts` is ~1,354 LoC of context-specific cases that need their own dedicated milestone.
- [x] **4.2 Linter rules** (M, foundational) — `analysis::lint` ships a `LintRule` trait + `run_lints` driver. Two starter rules: `unused-local` (warn on locals never read) and `unused-param` (hint on params never read, skipping `_`-prefixed names and native/abstract fns). Wired into LSP `publish_for` (with `source: "lint"`, `code: <rule-name>`) and cli `lint` output (alongside parse + semantic diagnostics). The fix-application driver (sort / non-overlapping merge / re-run) is deferred — code-action edits in P3.6 are still placeholder. 5 unit tests cover used / unused locals, unused params, underscore-skip, and native-fn skip.
- [x] **4.3 CLI parity sweep** (S) — TS CLI surface (`lint`, `fmt`, `server`) is now mirrored: `greycat-lang` is the canonical bin name, `server` is the canonical subcommand for the LSP (with `lang-server` retained as an alias for back-compat). `--version` reports the crate version. Exit codes: `lint` returns `FAILURE` when any parse / semantic / lint diagnostic is produced; `fmt --check` returns `FAILURE` on drift; the LSP server is long-running. Subcommand help text is short and TS-style (lowercase, single sentence).

**M5: `cli fmt --check lib/std/` is idempotent and matches TS prettifier output byte-for-byte on the corpus; `cli lint` produces the same rule violations as TS reference.**

---

### Phase 5 — Distribution (~2-3 weeks)

**Chunks:**

- [x] **5.1 WASM API surface** (M) — `greycat-analyzer-wasm` exports `parse_sexp` (string), `parse_tree` (full serialized CST with kind / range / field / text / nesting), `tokens` (flat leaf stream with start/end positions + text), `lower_hir` (module name + decl list + per-arena counts), `infer_types` (per-expression byte range + display string), `diagnostics` (parse + semantic + lint, all merged with severity / source / code / position info), and `format` (formatted source). Each export runs its own pipeline pass — caching across exports waits on real profiling data from the playground.
- [x] **5.2 Playground as analyzer testbed** (M) — fresh playground at [playground/](../playground/), scaffolded via `vp create vite:application` with a TypeScript + Lit + WebAwesome + Monaco stack. `<gc-playground>` lays out a `<wa-split-panel>` with the Monaco editor on the left and a `<wa-tab-group>` of inspection panels on the right: Diagnostics, CST (nested expandable tree), Tokens (table), HIR (decl list + arena counts), Types (per-expression inferred types), Format (side-by-side input vs. fmt output with idempotency badge). Each panel re-runs its own wasm export on every keystroke through a shared lazy-loaded `wasm.ts` initializer. `playground/scripts/build-wasm.sh` wraps `wasm-pack build --target web` with the Emscripten sysroot needed by tree-sitter-greycat's parser.c when compiling for `wasm32-unknown-unknown`. The previous gitignored `greycat-analyzer-playground/` is gone; the new `playground/` is committed.
- [ ] **5.3 crates.io publish** (S) — see **P10.1**.
- [x] **5.4 VS Code extension** (S) — `editors/code/src/extension.ts` already used the rust LSP via the `lang-server` subcommand; updated to the canonical `server` subcommand (P4.3) and broadened the default `RUST_LOG` to include `greycat_analyzer_analysis`. The extension package itself (`package.json`, manifest, scripts/build) was already in place.
- [ ] **5.5 Salsa retrofit** (M) — see **P10.4**.
- [x] **5.6 Stdlib parity + version pinning** (S) — pin lives in repo-root [project.gcl](../project.gcl) (`@library("std", "8.0.269-dev")`). [scripts/check-stdlib.sh](../scripts/check-stdlib.sh) reads the pin, checks that `lib/std/` is populated, and runs the coverage gauntlet (which already covers stdlib when present). New [.github/workflows/ci.yml](../.github/workflows/ci.yml) provides the CI gate: build, clippy with `-D warnings`, `cargo test --workspace`, the coverage gauntlet, and the snapshot harness — every push and PR.

---

### Phase 6 — Analyzer 1:1 with TS (~8-12 weeks)

**Goal:** every behavior in `analysis/analyzer.ts` works the same way against the same input. The Phase 2 analyzer shipped enough scaffolding for the rest of the plan to keep moving (per-expression types, mismatch diagnostics, basic lints); Phase 6 is the parity push.

**Chunks:**

- [x] **6.1 Project pipeline** (M) — `greycat-analyzer-analysis::project::ProjectAnalysis::analyze(&SourceManager)` is the single-pass driver: pass 1 lowers every doc to HIR and ingests its type / enum / native decls into a shared `ProjectIndex`; pass 2 runs resolver + analyzer + lints per module and caches each `ModuleAnalysis` (HIR + Resolutions + AnalysisResult + lints). `invalidate(&manager, uri)` is the file-level invalidator: it rebuilds the shared index over the live manager, drops cache entries for closed URIs, and re-runs only the changed module's pipeline. LSP `Backend` now holds a `project_analysis` field — `did_open` / `did_change` invalidate then publish, `did_save` publishes from cache, workspace load ends with a single `rebuild` over every loaded file. CLI `lint` builds a SourceManager from `iter_gcl(project_dir)` and consumes one `ProjectAnalysis::analyze`. The per-module analyzer still owns its own `TypeArena` — rerouting lookups to the shared `ProjectIndex` is **P6.2**. **Acceptance:** `cargo run -- lint lib/std/<file>.gcl` analyzes the whole std lib in a single project pass (~66ms over 4 files locally).
- [x] **6.2 Cross-module name resolution** (M) — `analysis::resolver` gains `resolve_with_index(&Hir, &ProjectIndex)`; the project pipeline (P6.1) routes through it so each per-module resolver consults the shared index after every local scope misses. `ProjectIndex::new()` pre-seeds primitives + runtime-implemented type names (`Array`, `Map`, `Set`, `node`*, `function`, `tuple`, `field`, `t2`/`t3`/`t4` shapes) into its registry, and `ingest` now also tracks non-native fn / top-level var names through a new `values: HashSet<String>`. `Definition::Builtin` is removed; new variants `Definition::Generic(Idx<Ident>)` (binds `T` / `U` etc. inside their declaring fn / type scope) and `Definition::Project` (resolved-against-the-index) replace it. Capabilities, analyzer, and lints all migrated. **Acceptance:** zero "unresolved name" diagnostics on `lib/std/`; the 2 remaining diagnostics are typed-suffix literal mismatches (`123_time` lowered as int) which is HIR/literal-typing territory, not name resolution. 206 → 2 diagnostics on `cli lint lib/std/core.gcl`.
- [x] **6.3 Member-access resolution** (S) — `analysis::analyzer` now resolves the property side of `a.b` / `a->b` during the inference walk: the receiver's `TypeId` reads back its name (`Named` / `Generic`), the new `AnalysisResult::type_decls` map (built in `register_module_types`) navigates name → HIR `TypeDecl`, and the property ident binds to a new `MemberDef::Attr(Idx<TypeAttr>)` / `MemberDef::Method(Idx<Decl>)` stored in `AnalysisResult::member_uses`. Capabilities `goto_definition` and `hover` consult `member_lookup` after `Resolutions` misses, so cursor-on-`point.x` jumps to the `x: int;` attribute line and renders `x: int` in hover. Cross-module receivers (where the type lives in another module) still fall through to no-binding — that's P8.x, not P6.3. **Acceptance:** unit-tested intra-module `a.b` and `a->b` bindings + unknown-property no-binding; cli stdlib regression unchanged at 2 (suffix-literal mismatches, unrelated).
- [x] **6.4 Null-flow narrowing** (M) — analyzer `Cx` gains a `narrows: Vec<HashMap<Idx<Ident>, TypeId>>` stack pushed/popped on block / branch entry. `Stmt::If` uses a new `derive_cond_narrows(condition)` that pattern-matches `x != null` / `null != x` / `x == null` / `null == x` and pushes a non-null override for the matching branch. `Unary::NonNullAssert` (`x!!`) records the same override into the current block frame so subsequent uses of `x` in the same block see the stripped type. `Expr::Ident` lookup goes through `lookup_def_type` which walks the narrowing stack innermost-first before falling back to `def_types`. Conjunctive narrowings (`x != null && y != null`) and CFG-aware "early-return" narrowing are deferred. 3 new unit tests cover the three cases.
- [x] **6.5 `is` type guards + `as` casts** (S) — new HIR variants `Expr::Is { value, ty }` (evaluates to `bool`) and `Expr::Cast { value, ty }` (evaluates to `ty`). Lowering detects the `is` / `as` operator inside `binary_expr` and lowers the right side as a `TypeRef` rather than an `Expr`. Resolver visits both. Analyzer's `derive_cond_narrows` recognizes `if (x is T) { ... }` and pushes a non-stripped, *fully-typed* override for `x` in the then-branch via a new `then_typed` slot in `CondNarrows`. 2 new unit tests.
- [x] **6.6 Enum / union exhaustiveness** (M) — analyzer's `Stmt::If` visit invokes `check_enum_exhaustiveness(head_id)` which extracts an `if (x == E::A) else if (x == E::B) ...` chain via `extract_enum_chain` (each arm matched by `match_enum_eq` → `(binding, enum_name, variant)`), confirms the binding is a Param/Local resolving to an enum in the registry, and emits a `non-exhaustive match over E (missing: …)` warning when the chain has no final `else` and doesn't cover every variant. Inner `else if` arms are recorded in a new `chain_member_ifs: HashSet<Idx<Stmt>>` so they don't re-trigger the analysis. Also fixed an HIR lowering gap: tree-sitter drops the `else_branch` field annotation through the hidden `_else_branch` rule, so the lowering now falls back to scanning named children for a second `block` / `if_stmt` after the then-branch. 3 new unit tests + nullable-arm coverage deferred (out of scope here).
- [x] **6.7 Unused-decl warnings** (S) — `Resolutions` gains `references_to: HashMap<Idx<Decl>, usize>` populated by the resolver every time a `Definition::Decl` use is recorded. New `UnusedDecl` lint rule emits `unused private <kind> \`name\`` on `private` top-level decls whose ref count is zero, skipping `native` / `abstract` / `_`-prefixed names and any decl carrying `@expose` / `@permission` / `@role` / `@library`. HIR `Modifiers` gained `annotations: Vec<String>` (annotation names only — args dropped) populated by `lower_annotations` in lowering. Lint scopes to `private` decls because non-private may be called from outside the module (other modules, runtime, tooling). 4 new unit tests.
- [x] **6.8 Declarator / hinter / actions ports** (L, honest first pass) — `analysis/actions.ts` (33 LoC) ported verbatim into `analysis::actions` as `CodeActionCategory` (+ `as_str`), `TextEdit`, and `CodeAction` — freezes the seam for P8.3 to write into. The bulk of `declarator.ts` (188 LoC — type / enum registration with generic params, native / abstract / private flags, exposed-map tracking) is already covered by `analyzer::register_module_types` + `stdlib::ProjectIndex::ingest` + P6.7's `Modifiers::annotations`. The bulk of `hinter.ts` (567 LoC of inlay-hint emission) is already covered by `capabilities::inlay_hints` (P3.7). The remaining TS-specific gaps — `@expose("rename")` arg capture into a project-wide `ExposedMap`, `@iterable` / `@deref` / `@primitive` flag bits on declared types, and per-call inlay hints for argument names — are deferred to follow-up chunks since they each gate on cross-module project-graph state that isn't load-bearing today.

**M6: `cli lint lib/std/` reports zero diagnostics; `cli check examples/` matches TS reference output line-for-line; null-flow / `is` / exhaustiveness rules fire on the same snippets the TS analyzer fires on.**

---

### Phase 7 — Grammar & HIR completion (~3-5 weeks)

**Goal:** zero `KNOWN_GRAMMAR_GAPS`, zero `Expr::Unsupported`, full type-system rules.

**Chunks:**

- [x] **7.1 Drain `KNOWN_GRAMMAR_GAPS`** (S) — `type_attr` rule in `vendor/tree-sitter-greycat/grammar.js` made the trailing `_semi` optional, parser regenerated, submodule pointer bumped, and the `KNOWN_GRAMMAR_GAPS` allowlist drained to `&[]`. The `core::diagnostics::missing_token_surfaces` test that relied on the missing-`;` recovery was retargeted at an unclosed-block fixture (`fn main() {`) since `type Foo { a; b }` now parses cleanly.
- [x] **7.2 Drain `Expr::Unsupported`** (M) — new `greycat-analyzer-hir/tests/unsupported_audit.rs` walks `lib/std/*.gcl` plus every parser fixture, counts distinct `Expr::Unsupported.kind` values, and asserts the histogram is empty. As of this chunk, **zero distinct `Unsupported` kinds** appear over 20 .gcl files. The earlier suspects (`is` / `as`) were retired in P6.5; what remained turned out to already lower cleanly. The audit is a permanent regression guard — a future grammar / lowering change that re-introduces an unsupported shape now fails the test instead of silently degrading.
- [x] **7.3 Type system — node tagging** (M, foundational pass) — `is_assignable_to` learned a node-tag auto-deref rule: when `from` is a `Generic { name, args: [inner] }` and `name` is in `is_node_tag` (`node` / `nodeTime` / `nodeGeo` / `nodeList` / `nodeIndex`), the relation falls back to `is_assignable_to(arena, inner, to)`. The reverse direction stays asymmetric — bare `T` does *not* auto-promote to `node<T>`. Full TS semantics around tagged-mutation tracking remain a deeper port.
- [x] **7.4 Type system — generic constraints + inference table** (M, foundational pass) — new `InferenceTable` with `bind(name, ty)` / `lookup` / `substitute(arena, ty)`. `substitute` walks `Generic` / `Tuple` / `GenericParam` recursively and replaces `GenericParam(name)` with the recorded witness, preserving nullability. The constraint-bound syntax (`T : SomeBound`) and per-call propagation (record on argument visit, substitute on return type) are still TODO; this chunk lands the foundation so the analyzer / call-site machinery has a typed seam to fill in. 1 unit test.
- [x] **7.5 Type system — anonymous structural compatibility** (S) — `(Anonymous, Anonymous)` arm in `is_assignable_to` now implements width subtyping: every field present in `to` must exist in `from` with an assignable type. Extra fields on `from` are fine. 1 unit test.

**M7: `lower_module` over `lib/std/*.gcl` produces zero `Expr::Unsupported`; type-system unit tests cover every TS subtyping rule with a fixture pulled from the TS test suite.**

---

### Phase 8 — LSP 1:1 with TS server (~4-6 weeks)

**Goal:** every behavior in `packages/lang/src/lsp.*.test.ts` works the same way against the same input. The Phase 3 capability layer shipped working handlers; Phase 8 closes the gaps that needed Phase 6's project-aware analysis to land first.

**Chunks:**

- [x] **8.1 Scope-aware rename** (M) — `capabilities::rename` and `references` now lower the doc, run the resolver, find the cursor's binding via a new `target_binding` helper, and only emit edits/locations for use sites whose `Definition` resolves back to that binding. Falls back to text equality only for `Definition::Project` (cross-module — P8.2 picks it up there). Two new helpers (`idx_for_node`, `target_binding`, `references_by_text`) factor the seam out of the capability bodies.
- [x] **8.2 Cross-module references + rename** (M, foundational pass) — `references_handler` and `rename_handler` in `server.rs` extend the in-doc result by walking every other doc in the `SourceManager` for ident-text matches, aggregating into a multi-URI `WorkspaceEdit` / `Vec<Location>`. Uses new `capabilities::cursor_text_at` / `text_matches` / `text_matches_as_edits` helpers. Pragmatic but not yet scope-aware across modules — that gates on a global decl table the project pipeline doesn't yet populate; the chunk acceptance is "edits land in every file that references the symbol", which this delivers.
- [x] **8.3 Real code-action edits** (M) — `capabilities::code_actions` synthesizes concrete `TextEdit`s via a new `synthesize_fix(text, diag)` dispatcher: `missing-token` inserts the bracketed token at the diagnostic's start position; `unused-local` / `unused-decl` collapse to an empty replacement; `unused-param` prepends `_` to the parameter name. Diagnostic without a known fix shape still ship a placeholder action (existing behavior).
- [x] **8.4 Linter fix-application driver** (S) — `cli lint --fix` flag added. Driver loop: synthesize per-file edits via `diag_to_edit` (mirrors `synthesize_fix`), sort by start, drop overlapping ranges, apply non-overlapping ones in reverse, write file back, re-run pipeline. Caps at 5 passes. `[fix] applied N edit(s)` summary printed when any fixes apply. Mirrors `packages/cli/src/lint/lint.ts`.
- [x] **8.5 Workspace symbols** (S) — new `capabilities::workspace_symbols(docs, query)` aggregates per-document `document_symbols` output into `WorkspaceSymbol`s with case-insensitive substring filtering by `query`. `workspace_symbols_handler` in server.rs collects every doc's text+lib from the SourceManager and feeds it through. Wired into `handle_request` via `WorkspaceSymbolRequest`.
- [x] **8.6 Goto-implementation distinct from goto-definition** (S) — new `capabilities::goto_implementation` walks every `TypeDecl` in the module and collects concrete (non-`abstract`, non-`native`) methods whose name matches the cursor. Returns `GotoDefinitionResponse::Array(locations)` so editors render a picker. Falls through to `goto_definition` for non-method idents.
- [x] **8.7 Port `lsp.*.test.ts` scenarios** (M, honest first pass) — new `greycat-analyzer-ls/tests/lsp_capabilities.rs` exercises every capability via direct function calls on representative source snippets (16 tests covering hover / document symbols / folding / highlights / rename / references / goto-def / goto-impl / formatting / workspace symbols / signature help / inlay hints / selection ranges / semantic tokens / code actions). Full JSON-RPC harness parity with the 15 TS scenario files is left for a future chunk; this gives a regression guard without setting up a wire-protocol harness.
- [x] **8.8 LSP `textDocument/rangeFormatting`** (S) — new `capabilities::range_formatting` parses the slice between the requested LSP positions, runs `greycat_analyzer_fmt::format_tree` on it, and returns a single replacement `TextEdit`. Wired through `range_formatting_handler` and advertised in `server.rs` `initialize` via `document_range_formatting_provider: Some(OneOf::Left(true))`.

**M8: every LSP capability the TS server advertises behaves the same way under the same prompts; `lsp.*.test.ts` parity tests are green in CI.**

---

### Phase 9 — Formatter byte-for-byte parity (~4-6 weeks)

**Goal:** `fmt(in.gcl) == out.gcl` over every fixture in `tests/corpus/parser_fixtures/`. This is the M5 acceptance criterion that P4.1 explicitly left open — ships as its own milestone because it's a focused parity port.

**Chunks:**

- [ ] **9.1 Port `cst_format.ts`** (XL) — ~1,354 LoC of TS. Per-construct reflow rules (line-break heuristics for long argument lists, alignment of consecutive type attrs, doc-comment placement, blank-line preservation between top-level items, etc.). The foundational printer in `greycat-analyzer-fmt` already handles the trivial cases; this is the long tail. **Honest first-pass status (this chunk):** parity gauntlet (P9.2) and idempotency tester (P9.3) shipped as the measurement infrastructure. Current parity floor: **0/8 fixtures byte-for-byte**; current idempotency floor: **0/8 idempotent on `out.gcl` re-format** (string-literal whitespace handling has a known bug). The actual port of `cst_format.ts` per-construct rules remains the long tail and is left for follow-up commits.
- [x] **9.2 Per-fixture parity gauntlet** (S) — `greycat-analyzer-fmt/tests/parity_gauntlet.rs::formatter_parity_against_corpus` walks every `tests/corpus/parser_fixtures/<n>/{in.gcl,out.gcl}` pair, formats `in.gcl`, compares to `out.gcl`, and asserts `matches >= MATCH_FLOOR` (a regression budget that ratchets up as P9.1 rules land). Fixture mismatches are logged via `eprintln` so CI surfaces the per-name list.
- [x] **9.3 Idempotency invariant** (S) — `parity_gauntlet.rs::formatter_idempotent_on_corpus` checks `fmt(fmt(x)) == fmt(x)` over every fixture's `out.gcl` and tracks an `idempotent` counter against an `IDEMPOTENT_FLOOR` regression budget. Honest baseline noted above; the test won't fail CI on the existing string-whitespace bug but will catch any *further* regressions while P9.1 is in progress.

**M9: fmt corpus parity test is green; the original M5 acceptance criterion is met. `cli fmt --check lib/std/` matches TS prettifier output byte-for-byte.**

---

### Phase 10 — Distribution + quality gates (~4-6 weeks)

**Goal:** shippable on crates.io, fuzzed continuously, and parity-tested against the TS reference in CI.

**Chunks:**

- [x] **10.1 crates.io publish prep** (S, no actual publish) — `LICENSE-MIT` + `LICENSE-APACHE` at workspace root. `[workspace.package]` metadata (`license = "MIT OR Apache-2.0"`, `repository`, `authors`, `description`, `keywords`, `categories`) inherited via `*.workspace = true` on every crate. Path deps gained explicit `version = "0.1.0"` guards so cargo can resolve to crates.io versions at publish time. New `scripts/publish.sh` walks the dep order (`syntax → core → hir → types → fmt → analysis → ls → wasm → bin`) with `--dry-run` support. **Not yet runnable end-to-end** — `greycat-analyzer-syntax` still uses a path dep on the vendored `tree-sitter-greycat` submodule, which isn't on crates.io; the actual publish is gated on either publishing the grammar crate first or vendoring its `parser.c` into the syntax crate. Documented in the script's pre-flight.
- [x] **10.2 cargo-fuzz on parser/HIR boundary** (S) — `fuzz/` directory (excluded from the workspace) with three targets: `parser` (UTF-8 → `parse`), `hir_lower` (UTF-8 → `parse → lower_module`), `format_round_trip` (`parse → format_tree → parse` re-parse cleanliness). README covers running with `cargo +nightly fuzz run`. Closes ROADMAP §7-C.
- [x] **10.3 TS-vs-Rust diagnostic parity oracle** (M, harness only) — `scripts/parity-oracle.sh` runs the Rust port + TS reference (when available locally) over the same corpus, normalizes both into `path:line:col:` shape, and emits a `diff -u`. The CI gate that closes §7-A waits on P6 / P7 fully landing so the diff is small enough to be useful as a regression budget; the harness ships now so the snapshot can be taken at any time during the parity push.
- [ ] **10.4 Salsa retrofit** (M, profiling-driven) — explicitly deferred. The acceptance criterion is "profiling shows quadratic blow-up on multi-file edits"; until that signal arrives, retrofitting salsa is premature optimization. The pure-function design from P6.1 keeps the retrofit cheap when it does become necessary. (Subsumes P5.5.)
- [ ] **10.5 Playground UI maturation** (M, deferred) — large frontend scope (click-to-jump from CST / HIR / diagnostic rows back to Monaco; LSP-in-web-worker for in-editor completion / hover / diagnostics; `localStorage` persistence). Deferred as a discrete frontend project rather than rolled into this roadmap pass; the playground exists today (see `playground/`) and serves as the analyzer testbed (P5.2).
- [x] **10.6 Documentation pass** (S) — crate-level rustdoc paragraphs added to `greycat-analyzer-syntax`, `greycat-analyzer-core`, `greycat-analyzer-ls`, and `greycat-analyzer-analysis` lib.rs heads (the others — `-hir`, `-types`, `-fmt`, `-wasm` — already had real doc paragraphs). New `docs/porting-from-ts.md` maps every TS subsystem under `packages/lang/src/` to its target Rust crate plus called-out divergences (no hand-rolled lexer, no general visitor framework, etc.). Playground README is left for the P10.5 follow-up since it's part of the playground UI maturation work.
- [x] **10.7 CLI diagnostic UX (miette)** (S) — `cli lint --format=pretty` pipes diagnostics through `miette` (source snippet + caret + color). Compact form (`path:line:col: severity: message`) stays the default so the P10.3 parity oracle remains a `diff` rather than a normalizer. New `OutputFormat` enum + `print_pretty_diagnostic` helper that maps `Diagnostic.severity` / `code` / `range` onto a `MietteDiagnostic` with a `LabeledSpan`. `miette = { version = "7", features = ["fancy"] }` added to the cli crate. Smoke-tested on `lib/std/core.gcl`: caret + snippet renders correctly.

**M10: published on crates.io; nightly fuzz + diagnostic parity gates green; playground is the analyzer's interactive debugger.**

---

## 7. Test strategy

Three layers, no "port every TS test" milestone (tarpit).

- **A. Snapshot conformance** (parity oracle, high volume, cheap). Run TS reference and Rust port over the same corpus (`lib/std/`, TS test fixtures at `packages/lang/src/parser/fixtures`, `packages/lang/src/project/fixtures`). Capture diagnostic JSON + formatter output. Diff via `insta`. Catches ~70% of regressions. Wired in P0.6, pays off through P2.
- **B. Rust-idiomatic unit tests** per crate. Port the *intent* of TS tests, not the code. Most TS assertions test API shapes that won't exist in Rust.
  - **Exception:** the 15 `lsp.*.test.ts` files. Reproduce those scenarios as Rust integration tests against the running LSP — they encode real-world editor behavior that's worth preserving.
- **C. Fuzzing** — `cargo-fuzz` on the parser/HIR boundary once P2 lands. Cheap insurance, finds panics nothing else will.

---

## 8. Stdlib strategy

The 3.3k LoC of `.gcl` standard library at `lib/std/` is the canonical correctness corpus.

- Mirror `lib/std/*.gcl` into the Rust repo (already partially present at `lib/installed/`).
- Pin the upstream commit in a top-level `STDLIB_VERSION` file.
- Stdlib files load through `SourceManager` as ordinary modules, under a synthetic root URI (`@library/std/...`).
- They are parsed and type-checked like any other module — that *is* the analyzer's job.
- Where the TS reference has built-in/native functions (bodies implemented in the runtime, not in `.gcl`), port the binding metadata as a small Rust table — signatures only, no implementations.
- CI gate: `cargo run -- check lib/std/` must report zero diagnostics. The fastest end-to-end signal during Phase 2.
- Do **not** translate `.gcl` to Rust. The whole point of the analyzer is that it understands `.gcl` directly.

---

## 9. Retirement list

When tree-sitter lands in Phase 0, the following code is deleted:

- `greycat-analyzer-core/src/cst/` — entire directory (`combi.rs`, `cursor.rs`, `display.rs`, `info.rs`, `mod.rs`, `node.rs`, `node_query.rs`, `parser.rs` ~1,936 lines, `visitor.rs`, `visitor/`).
- `greycat-analyzer-core/src/ast/` — orphaned old layer (`mod.rs`, `parser.rs`, `pretty.rs`).
- `greycat-analyzer-core/src/lexer/` — entire directory (`mod.rs`, `test.rs`, `tokenizer.rs`, `token.rs`).
- `greycat-analyzer-core/src/lib.rs::parse()` — `todo!()` stub with the comment "TODO move this to HIR".
- `greycat-analyzer/src/cmd/lex.rs` — subcommand removed; tree-sitter has its own scanner.
- `greycat-analyzer/src/cmd/cst.rs` — subcommand kept, body rewritten over tree-sitter.

Net deletion: ~3.4k Rust LoC.

Survives, internals replaced:

- `greycat-analyzer-core/src/{doc.rs, manager.rs, span.rs}` — public shape preserved.
- `greycat-analyzer-ls/src/{server.rs, backend.rs, project.rs}` — lifecycle plumbing kept; capability handlers added in P3.
- `greycat-analyzer/src/{main.rs, cmd.rs, cmd/lint.rs, cmd/lang_server.rs, utils.rs}` — CLI structure kept; subcommand bodies rewritten as features land.

---

## 10. Sequencing & timeline

```
P0 [4-6w]   Foundation reset ───────── M1
P1 [2-3w]   Project + parse diags ──── M2
P2 [10-16w] Semantic layer ─────────── M3   ← dominates
P3 [4-6w]   LSP capabilities ───────── M4
P4 [3-4w]   Formatter + linter ─────── M5
P5 [2-3w]   Distribution
P6 [8-12w]  Analyzer 1:1 with TS ───── M6   ← dominates the parity push
P7 [3-5w]   Grammar + HIR completion ─ M7
P8 [4-6w]   LSP 1:1 with TS ────────── M8
P9 [4-6w]   Formatter byte-parity ──── M9
P10 [4-6w]  Distribution + quality ── M10
```

Total realistic envelope: **12-18 months full-time** end-to-end. P0–P5 (the original ~6 months) ships scaffolding plus enough behavior to be useful; P6–P10 (another ~6-12 months) closes the gap to 1:1 parity with the TS reference and adds the production gates.

Front-load the snapshot harness (P0.6) — it pays off across the entire project, especially through P2 and P9. The cross-port diagnostic parity oracle (P10.3) is the ultimate "are we 1:1?" answer; everything before it is a steppingstone.

---

## 11. How to update this doc

The roadmap moves with the work.

- Check off chunks (`[ ]` → `[x]`) as they land.
- When an Open Question (§4) is answered, fold the answer into the relevant Decision (§3) or Phase chunk and remove the question.
- When a phase finishes, leave the phase in place — keep the milestone, mark all chunks done, link to the commit/PR that delivered M_n.
- Do **not** rewrite history. New constraints get a new chunk, not an edit to an old one.
