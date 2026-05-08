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
| LSP capability handlers | `lsp/` (+ 1,527 LoC of tests) | 49 | `greycat-analyzer-server` |
| LSP server transport | `packages/server/` | 1,228 | `greycat-analyzer-server` |
| CLI driver | `packages/cli/` | 743 | `greycat-analyzer` (bin) |
| Linter | `packages/cli/src/lint/` | 242 | `greycat-analyzer-analysis` (rules) + `greycat-analyzer` (CLI) |
| Module resolver (`@library`, `@include`) | `packages/resolver/` | 92 | `greycat-analyzer-core` |
| Error infrastructure | `errors.ts` | 145 | `greycat-analyzer-analysis` (or shared) |
| Highlighter (semantic tokens) | `highlighter.ts` | 141 | `greycat-analyzer-server` |
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
| `greycat-analyzer-server` | LSP server | survives |
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
- [ ] ~~**5.3 crates.io publish** (S) — see **P10.1**.~~ → **moved to P14.1** (publish unblock; the path-dep on `tree-sitter-greycat` is the actual blocker).
- [x] **5.4 VS Code extension** (S) — `editors/code/src/extension.ts` already used the rust LSP via the `lang-server` subcommand; updated to the canonical `server` subcommand (P4.3) and broadened the default `RUST_LOG` to include `greycat_analyzer_analysis`. The extension package itself (`package.json`, manifest, scripts/build) was already in place.
- [ ] ~~**5.5 Salsa retrofit** (M) — see **P10.4**.~~ → **moved to P14.6** (still profiling-driven; tracked there alongside the other production gates).
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

- [x] **7.1 Drain `KNOWN_GRAMMAR_GAPS`** (S) — `type_attr` rule in `tree-sitter-greycat/grammar.js` made the trailing `_semi` optional, parser regenerated, submodule pointer bumped, and the `KNOWN_GRAMMAR_GAPS` allowlist drained to `&[]`. The `core::diagnostics::missing_token_surfaces` test that relied on the missing-`;` recovery was retargeted at an unclosed-block fixture (`fn main() {`) since `type Foo { a; b }` now parses cleanly.
- [x] **7.2 Drain `Expr::Unsupported`** (M) — new `greycat-analyzer-hir/tests/unsupported_audit.rs` walks `lib/std/*.gcl` plus every parser fixture, counts distinct `Expr::Unsupported.kind` values, and asserts the histogram is empty. As of this chunk, **zero distinct `Unsupported` kinds** appear over 20 .gcl files. The earlier suspects (`is` / `as`) were retired in P6.5; what remained turned out to already lower cleanly. The audit is a permanent regression guard — a future grammar / lowering change that re-introduces an unsupported shape now fails the test instead of silently degrading.
- [x] **7.3 Type system — node tagging** (M, foundational pass) — `is_assignable_to` learned a node-tag auto-deref rule: when `from` is a `Generic { name, args: [inner] }` and `name` is in `is_node_tag` (`node` / `nodeTime` / `nodeGeo` / `nodeList` / `nodeIndex`), the relation falls back to `is_assignable_to(arena, inner, to)`. The reverse direction stays asymmetric — bare `T` does *not* auto-promote to `node<T>`. Full TS semantics around tagged-mutation tracking remain a deeper port.
- [x] **7.4 Type system — inference table** (M, foundational pass) — new `InferenceTable` with `bind(name, ty)` / `lookup` / `substitute(arena, ty)`. `substitute` walks `Generic` / `Tuple` / `GenericParam` recursively and replaces `GenericParam(name)` with the recorded witness, preserving nullability. Per-call constraint propagation (record on argument visit, substitute on return type) is still TODO; this chunk lands the foundation so the analyzer / call-site machinery has a typed seam to fill in. 1 unit test. **Note:** GreyCat has no generic-bound syntax — there's nothing like Rust's `T: Bound`; the chunk is purely about inference / unification.
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
- [x] **8.7 Port `lsp.*.test.ts` scenarios** (M, honest first pass) — new `greycat-analyzer-server/tests/lsp_capabilities.rs` exercises every capability via direct function calls on representative source snippets (16 tests covering hover / document symbols / folding / highlights / rename / references / goto-def / goto-impl / formatting / workspace symbols / signature help / inlay hints / selection ranges / semantic tokens / code actions). Full JSON-RPC harness parity with the 15 TS scenario files is left for a future chunk; this gives a regression guard without setting up a wire-protocol harness.
- [x] **8.8 LSP `textDocument/rangeFormatting`** (S) — new `capabilities::range_formatting` parses the slice between the requested LSP positions, runs `greycat_analyzer_fmt::format_tree` on it, and returns a single replacement `TextEdit`. Wired through `range_formatting_handler` and advertised in `server.rs` `initialize` via `document_range_formatting_provider: Some(OneOf::Left(true))`.

**M8: every LSP capability the TS server advertises behaves the same way under the same prompts; `lsp.*.test.ts` parity tests are green in CI.**

---

### Phase 9 — Formatter byte-for-byte parity (~4-6 weeks)

**Goal:** `fmt(in.gcl) == out.gcl` over every fixture in `tests/corpus/parser_fixtures/`. This is the M5 acceptance criterion that P4.1 explicitly left open — ships as its own milestone because it's a focused parity port.

**Chunks:**

- [ ] ~~**9.1 Port `cst_format.ts`** (XL) — ~1,354 LoC of TS. Per-construct reflow rules (line-break heuristics for long argument lists, alignment of consecutive type attrs, doc-comment placement, blank-line preservation between top-level items, etc.). The foundational printer in `greycat-analyzer-fmt` already handles the trivial cases; this is the long tail. **Honest first-pass status (this chunk):** parity gauntlet (P9.2) and idempotency tester (P9.3) shipped as the measurement infrastructure. Current parity floor: **0/8 fixtures byte-for-byte**; current idempotency floor: **0/8 idempotent on `out.gcl` re-format** (string-literal whitespace handling has a known bug). The actual port of `cst_format.ts` per-construct rules remains the long tail and is left for follow-up commits.~~ → **moved to P14.3** (formatter parity floor ratchets to 8/8 — bundles the `cst_format.ts` port + the string-literal whitespace bug fix).
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
- [ ] ~~**10.4 Salsa retrofit** (M, profiling-driven) — explicitly deferred. The acceptance criterion is "profiling shows quadratic blow-up on multi-file edits"; until that signal arrives, retrofitting salsa is premature optimization. The pure-function design from P6.1 keeps the retrofit cheap when it does become necessary. (Subsumes P5.5.)~~ → **moved to P14.6.**
- [ ] ~~**10.5 Playground UI maturation** (M, deferred) — large frontend scope (click-to-jump from CST / HIR / diagnostic rows back to Monaco; LSP-in-web-worker for in-editor completion / hover / diagnostics; `localStorage` persistence). Deferred as a discrete frontend project rather than rolled into this roadmap pass; the playground exists today (see `playground/`) and serves as the analyzer testbed (P5.2).~~ → **moved to P14.7.**
- [x] **10.6 Documentation pass** (S) — crate-level rustdoc paragraphs added to `greycat-analyzer-syntax`, `greycat-analyzer-core`, `greycat-analyzer-server`, and `greycat-analyzer-analysis` lib.rs heads (the others — `-hir`, `-types`, `-fmt`, `-wasm` — already had real doc paragraphs). New `docs/porting-from-ts.md` maps every TS subsystem under `packages/lang/src/` to its target Rust crate plus called-out divergences (no hand-rolled lexer, no general visitor framework, etc.). Playground README is left for the P10.5 follow-up since it's part of the playground UI maturation work.
- [x] **10.7 CLI diagnostic UX (miette)** (S) — `cli lint` defaults to `pretty` (miette: source snippet + caret + color) when stdout is a TTY, and to `compact` (`path:line:col: severity: message`) when piped — so the P10.3 parity oracle and grep-style consumers keep a stable diffable shape. `--format={compact,pretty}` overrides explicitly. The `--format` field is `Option<OutputFormat>`; `OutputFormat::auto()` consults `std::io::IsTerminal` at run time to pick the default. New `print_pretty_diagnostic` helper maps `Diagnostic.severity` / `code` / `range` onto a `MietteDiagnostic` with a `LabeledSpan`. `miette = { version = "7", features = ["fancy"] }` added to the cli crate.

**M10: published on crates.io; nightly fuzz + diagnostic parity gates green; playground is the analyzer's interactive debugger.**

---

### Phase 11 — Cross-module identity (~3-5 weeks)

**Goal:** lift `Definition::Project` from "resolved, no detail" to "resolved to (uri, decl_id)" so every capability that needs cross-module navigation (goto-def, references, rename, member-access) stops falling back to text equality. P8.1 / P8.2 / P8.6 land scope-aware semantics for *intra-module* names; this phase finishes the job across modules.

**Chunks:**

- [x] **11.1 Global decl table** (M) — `ProjectIndex` gains `decl_locations: HashMap<String, Vec<(Uri, Idx<Decl>)>>` populated by `ingest`. Collisions across modules are kept; disambiguation happens at the use site via the importing module's lib/include closure. `ProjectAnalysis::analyze` repopulates on every rebuild. Acceptance: querying the index for `"Permission"` returns the URI of `lib/std/runtime.gcl` and the matching `Idx<Decl>`.

- [x] **11.2 `Definition::Project` carries detail** (M) — added a new `Definition::ProjectDecl { uri, decl }` variant carrying the foreign module's `Uri` + `Idx<Decl>`. Resolver's `record_use` now consults `ProjectIndex::locate_decl` (P11.1's global decl table) before the broader `has_name` check, so cross-module references to user `.gcl` decls land on `ProjectDecl` while runtime-only names (`Array` / `Map` / `node*` / native fn signatures / primitives by name) still fall through to the unit `Project` variant. `Definition` dropped `Copy` (the roadmap claim that `Uri` is `Copy` was wrong — it isn't); downstream sites switched to `clone()` / `&` borrows. Capabilities (`hover`, `goto_definition`, `references`, `rename`, `target_binding`, `semantic_tokens`) and the analyzer's `Expr::Ident` arm pattern-match the new shape; cross-module navigation still no-ops at the capability level — that's P11.3 / P11.4 territory. New resolver test asserts a cross-module `Helper` use binds to `ProjectDecl { uri: <a.gcl>, decl: <Helper TypeDecl> }`; companion test asserts `Array` keeps the unit `Project` fallback.

- [x] **11.3 Cross-module goto-definition** (S) — `goto_definition_handler` now consults the cached `ProjectAnalysis` after the in-module `capabilities::goto_definition` returns `None`: it maps the cursor to an `Idx<Ident>` against the cached `Hir`, looks up the `Definition` from cached `Resolutions`, and on a `Definition::ProjectDecl { uri, decl }` hit fetches the foreign `ModuleAnalysis` + `Doc` and asks the new `capabilities::cross_module_decl_location` for the foreign decl-name range. Two new pure helpers in `capabilities.rs`: `cross_module_decl_location(foreign_uri, foreign_text, foreign_hir, decl_id) -> Option<Location>` and `cursor_ident_idx(text, root, pos, hir) -> Option<Idx<Ident>>`. Unit-tested via `cross_module_decl_location_points_at_foreign_name`.

- [x] **11.4 Cross-module references + rename** (M) — `references_handler` and `rename_handler` now route through new `capabilities::references_across_project` / `rename_across_project` instead of the P8.2 text-equality fallback. The new helpers consume the cached `ProjectAnalysis` directly: a `RenameTarget::{LocalIdent, ProjectDecl}` is computed once via `resolve_rename_target`, then `visit_target_sites` walks every cached module's `Resolutions::uses` to emit either `Definition::Decl` (home module) or `Definition::ProjectDecl` (importers) hits, plus the binding site itself. Per-`Uri` aggregation is straightforward — each module's text + HIR are pulled out of the project cache + source manager. The three pre-P11.4 helpers (`cursor_text_at`, `text_matches`, `text_matches_as_edits`) are deleted; `references_by_text` stays as the in-module-only fallback for `capabilities::references` / `rename` (which exist as test-friendly pure functions). Two new tests cover cross-module references (3 hits in user.gcl + 1 binding in home.gcl for `type Helper`) and per-`Uri` rename aggregation.

- [x] **11.5 Cross-module member resolution** (M) — analyzer's `resolve_member` now records `(property_idx, type_name)` into a new `AnalysisResult::deferred_member_uses` whenever the receiver's type isn't declared in the current module. `ProjectAnalysis::rebuild` (and `invalidate`) drains those pairs in a third pass: each name goes through `ProjectIndex::locate_decl` (P11.1) → foreign module's cached HIR → its `TypeDecl::attrs` / `methods`, and the resulting binding lands in the per-module `AnalysisResult::foreign_member_uses: HashMap<Idx<Ident>, ForeignMember { uri, member }>`. New `analyze_with_index` analyzer entry point threads the shared `ProjectIndex` through `lower_type_ref` so cross-module type names lower to `Named(name)` instead of falling back to `any` (otherwise the deferred list stayed empty). `goto_definition_handler` now consults `foreign_member_lookup` after the `ProjectDecl` branch and uses the new `capabilities::cross_module_member_location` helper to compute the foreign attr / method's name range. Unit-tested: `p.x` across `shapes.gcl` (declares `Point { x: int }`) and `main.gcl` produces a `ForeignMember { uri: shapes.gcl, member: Attr(_) }` binding.

- [x] **11.6 Cross-module goto-implementation** (S) — `goto_implementation_handler` now routes through new `capabilities::goto_implementation_across_project`, which walks every cached `ModuleAnalysis::hir` for `TypeDecl::methods` whose name matches the cursor's ident text (skipping `abstract` / `native`). Falls through to in-module `goto_implementation` → `goto_definition` for non-method idents and when no project-wide match exists. Unit-tested: `Foo::run` in `a.gcl` + `Bar::run` in `b.gcl` → cursor on `run` returns both locations.

**M11: cross-module navigation (goto-def / goto-impl / references / rename / member access) works end-to-end on a multi-file project. The unit `Definition::Project` placeholder is *partly* gone: user `.gcl` decls now resolve to `Definition::ProjectDecl { uri, decl }` (P11.2), while runtime-only names (Array / Map / node tags / native fns / primitives) keep the unit `Project` variant since they have no `.gcl` declaration to point at.**

---

### Phase 12 — Type system completion (~4-6 weeks)

**Goal:** real generic inference + variance + the rest of the TS subtyping rules. P7's "foundational pass" entries get their full deepening here. (GreyCat has no generic-bound syntax — `T: Bound` is a Rust concept; nothing analogous exists in `.gcl`.)

**Chunks:**

- [x] **12.1 Generic inference at call sites** (M, foundational pass) — analyzer's `Cx` gained a `generics_in_scope: Vec<HashMap<String, GenericOwner>>` stack pushed/popped on `visit_fn_decl` / `visit_type_decl` so `lower_type_ref` mints `GenericParam(name, owner)` for in-scope generics instead of falling back to `any`. New `try_generic_call_inference` runs on `Expr::Call`: when the callee resolves to an in-module `Decl::Fn` with `generics`, it lowers the declared param + return types under that fn's generic scope, walks each `(declared_param, arg_ty)` pair via `collect_witnesses` (which descends into matching `Generic` / `Tuple` shapes and also handles `T?` parameters), records witnesses into an `InferenceTable`, and substitutes the return type. Conflicts emit a `cannot infer \`T\`: \`A\` conflicts with \`B\`` diagnostic. Cross-module callees and method-call generic inference are deferred — they need foreign HIR access the analyzer doesn't yet carry. New `TypeArena::generic_param` constructor. 2 unit tests cover the substitution path and the conflict diagnostic.

- [x] **12.2 Variance for user-declared generics** (S) — `is_assignable_to`'s `Generic` arm stays **invariant** in every generic parameter, matching the GreyCat runtime (the true oracle). The TS reference checker (`GreycatGenericType.isAssignableTo` in `analysis/types.ts:1071-1090`) implements covariance — but that's a TS-side bug; the runtime rejects covariant assignment (e.g. `Array<float>` is **not** assignable to `Array<int>`, and the reverse direction is also rejected). The chunk's net code change is therefore the comment + a new `generic_name_mismatch_stays_unassignable` test that locks in the head-name check. Inheritance-aware assignability across different generic names (`type Child<T> extends Parent<T>`) is a later phase — the analyzer doesn't yet thread declared supertypes into `is_assignable_to`.

- [x] **12.3 Deeper node-tag rules** (M) — new `is_castable(arena, from, to)` in `greycat-analyzer-types` ports the TS reference's `isCastable` (`packages/lang/src/analysis/utils.ts:360`), including the asymmetric `int ↔ {int, float, node, nodeTime, nodeList, nodeIndex, nodeGeo}` promotions and `node*` → `int` round-tripping. The analyzer's `Expr::Cast` arm consults `is_castable` and emits a `cannot cast \`A\` to \`B\`` diagnostic for invalid casts (the resulting expression type stays `to_ty` so downstream inference doesn't cascade). 2 new analyzer unit tests cover the rejected `String as int` case and the allowed `int as nodeTime` promotion. Tagged-mutation tracking on top of the auto-deref rule (e.g. `var x: node<T>; x.field = ...;` distinguishing reads vs writes) remains for a later chunk — `is_castable` covers the cast-and-promotion side now.

- [x] **12.4 TS subtyping fixtures gauntlet** (S) — new [`greycat-analyzer-types/tests/subtyping_gauntlet.rs`](../greycat-analyzer-types/tests/subtyping_gauntlet.rs) ports the *intent* of the TS `analysis/typesystem.test.ts` suite into 21 Rust unit tests, but each rule is verified against the **GreyCat runtime** (the true oracle) rather than the TS reference checker. Probes captured live via `greycat run` against `var v: <SRC> = <INIT>; take(v); fn take(_: <TGT>) {}`. Three runtime-vs-TS divergences were folded into the analyzer along the way: (a) `primitive_assignable` collapsed to identity-only (the runtime rejects every cross-primitive widening at parameter / binding sites — including `int → float`, which TS allows), (b) `lambda_contravariant_params_covariant_return` test rewritten to exercise variance through `any` instead of `int → float`, (c) numeric literals now classify int vs float by source-text inspection (`_f` suffix / decimal point / scientific notation, with a digit-anchored guard so `_time` doesn't false-trigger on its `e`). Stdlib lint still hits the 9-diagnostic baseline (2 typed-suffix `_time` literals remain — P13.3 territory).

**M12: every TS subtyping rule has a passing Rust unit test pulled from the TS suite; the type-system foundational-pass disclaimers in P7's roadmap entries are lifted.**

---

### Phase 13 — Analyzer parity closeout (~3-4 weeks)

**Goal:** CFG-aware narrowing, declarator/hinter deep ports, decl-level annotation flags, typed-suffix literals — the long tail of "behaves the same as the TS analyzer on the same input."

**Chunks:**

- [x] **13.1 CFG-aware narrowing (early-return)** (M) — new `stmt_terminates(hir, stmt)` walker recognizes `return` / `throw` / `break` / `continue` (and their nested-block variants; `if` terminates iff both branches do). Analyzer's `Stmt::If` checks each branch's terminal status after visiting it; if the then-branch terminates, the post-if scope inherits the else-side `non_null` narrowing into the enclosing block frame; mirrored for the else side (which also propagates `then_typed` from `is`-guards). Handles the `if (x == null) { return; } use(x);` and `if (x == null) { throw; } use(x);` idioms; 2 new unit tests.

- [x] **13.2 Conjunctive / disjunctive narrowings** (S) — `derive_cond_narrows` now recurses through `BinOp::And` / `BinOp::Or` (and unwraps `Expr::Paren`). For `A && B` the then-branch unions both then-narrowings (both held); for `A || B` the else-branch unions both else-narrowings (both inverses held). Mixed forms can't safely narrow either side. 2 new unit tests cover `if (x != null && y != null)` and `if (x == null || y == null)` with paired non-null bindings.

- [x] **13.3 Typed-suffix literal lowering** (S) — new `LiteralKind::Time` variant joins the existing `Iso8601` / `Duration` slots; HIR's `classify_number` walks `(number_suffixed (number_suffix))` children and routes `time` → `Time`, every duration unit (`y`/`d`/`h`/`m`/`s`/`ms`/`us`/`ns`/named forms) and explicit `_duration` → `Duration`, everything else (including `_f` floats) stays `Number` for the analyzer's text-inspection dispatch. Closes the 2 residual `static min: time = -9223372036854775808_time;` diagnostics — stdlib lint baseline drops 9 → 7. 4 new HIR tests cover `_time`, duration unit, `_f` float, plain int.

- [x] **13.4 `@expose("rename")` capture into ExposedMap** (M) — `Modifiers::annotations` lifted from `Vec<String>` to `Vec<Annotation { name: String, args: Vec<String> }>`. `lower_annotations` walks each annotation's `(args (string …))` children and captures every string-literal arg. `ProjectIndex` gained `exposed: HashMap<String, Vec<ExposureSite>>` keyed by the rename string (or the decl's own name when `@expose` is bare). `ingest` walks each decl's annotations and records `ExposureSite { uri, decl, local_name, rename }`. The `unused-decl` lint's `exposes_runtime` reader migrated to the new `Annotation` shape. New stdlib unit test asserts `@expose("public_alpha")` keys by `public_alpha`, bare `@expose` keys by the local name, and unrelated annotations (`@library`) stay out of the map.

- [x] **13.5 Decl-level type flags** (M, foundational pass) — new `TypeFlags { iterable, deref: Option<String>, primitive }` plus `ProjectIndex::type_flags: HashMap<String, TypeFlags>` populated by `ingest` from each `type` decl's `@iterable` / `@deref("method")` / `@primitive` annotations. The data table is in place; wiring the consumer side (`for x in t` legality gated on `iterable`, member resolution falling through `@deref`'s named method, primitive structural-exemption) lands in P13.6 / P13.7 as the analyzer's behavioral parity push — adding those checks today over stdlib would surface noisy false-positive churn that's better paired with the deeper declarator / hinter ports. New unit test asserts `@iterable @deref("resolve") type Bag {}`, `@primitive type Marker {}`, and an unannotated type land the right flag bits.

- [x] **13.6 declarator.ts deep port residual** (S) — two ports landed: (a) `@permission("name")` mod-pragmas now populate `ProjectIndex::module_permissions: HashMap<Uri, HashSet<String>>` during `ingest`; (b) new `DuplicateDecl` lint rule mirrors the TS reference's `Identifier 'X' is already declared` check (`packages/lang/src/analysis/declarator.ts:130`) and emits a `duplicate-decl` error when two top-level decls share a name in the same module. `LintSeverity` gained an `Error` variant (downstream readers in cli / lsp / wasm updated to map it to `DiagnosticSeverity::ERROR` / `"error"`). The "static field initializer can't depend on `this`" check and other per-decl validation diagnostics from `declarator.ts` are downstream — they need flow-sensitive expression analysis we don't have today and would surface noisy churn over stdlib until then.

- [x] **13.7 hinter.ts deep port residual** (S) — `inlay_hints` now also emits two new hint shapes alongside the P3.7 `var x: <inferred>` annotations: (a) per-call **argument-name** hints (`f(1, 2)` against `fn f(x: int, y: int)` renders `x:` / `y:` before each arg) via a new `emit_call_arg_hints` walker that recurses through every expression shape and pattern-matches `Expr::Call` whose callee resolves to a `Decl::Fn`, skipping `_`-prefixed param names; (b) **inferred return-type** hints anchored after the fn name when the decl has no declared return type but the body's last `Stmt::Return(Some(e))` carries an inferred `TypeId` (via `inferred_fn_return`). Lambda-param inlay hints are deferred — they need call-site type-flow propagation that today's analyzer doesn't carry. 2 new unit tests cover the new shapes.

**M13: `cli lint project.gcl` returns zero diagnostics on a workspace whose project.gcl pulls in stdlib (closing the original M3 acceptance gap from typed-suffix literals); the lsp.*.test.ts parity tests (P8.7) are deepened with the full set of TS scenario inputs and pass.**

---

### Phase 14 — Final parity gate (~2-3 weeks)

**Goal:** turn the harnesses (P10.3 parity oracle, fuzz, formatter parity gauntlet) into actual CI gates and resolve the publish blocker. Closes ROADMAP §7-A end-to-end.

**Chunks:**

- [ ] **14.1 Publish unblock** (S) — either publish `tree-sitter-greycat` to crates.io (preferred — keeps the submodule SHA as the grammar pin) and bump `greycat-analyzer-syntax` to consume the published version, or vendor `parser.c` + `node-types.json` directly into `greycat-analyzer-syntax/src/grammar/` and drop the path-dep. Either path lets `scripts/publish.sh` actually run end-to-end. Subsumes the "currently blocked" note in P10.1.

- [ ] **14.2 Diagnostic parity gate in CI** (M) — `scripts/parity-oracle.sh` becomes a CI step gated against a per-fixture diff budget that ratchets toward zero. Closes ROADMAP §7-A. Gates on P11/P12/P13 fully landing so the diff is small enough to ratchet meaningfully.

- **14.3 Formatter parity floor ratchets toward 8/8** (XL — bundles P9.1) — ports `cst_format.ts`'s per-construct rules incrementally; the parity gauntlet's `MATCH_FLOOR` ratchets up as rules land. **Current baseline: 3/8 byte-for-byte, 6/8 idempotent.** Rules added in this pass:
  - `:` is no-space-before, space-after (covers fn params, type-attrs, `var x: T`).
  - `::` and `?` and `<` / `>` are tight (no space around).
  - `@` is the annotation-opener: no surrounding space.
  - Empty container `{}` (block / type_body / object_initializers / object_fields with zero named children) stays on one line.
  - `type_body` `}` inserts a missing `;` before close.
  - Trailing newline mirrors the input (the corpus fixtures don't carry one; saved files usually do).
  - Annotations group emits a trailing newline so `@pragma\nfn foo()` survives the formatter.
  - Blank lines between top-level / sibling decls are preserved; doc-comments suppress the gap so they "stick" to their decl.
  - Inline EOL `// ...` comments after a `{` / `;` survive on the same line (uses source-byte gap to detect).

  Remaining hard cases (not in this pass): line-length-aware reflow for long `<...>` / `(...)` (args_split, nested_args_split, if_var_object), block-comment placement, and the comment-vs-blank-line ordering nuance (doc_eol_stmt, stmts_rules). M9 acceptance ("8/8") still depends on these.

- [x] **14.4 Continuous fuzzing** (S) — new [`.github/workflows/fuzz.yml`](../.github/workflows/fuzz.yml) runs all three targets (`parser`, `hir_lower`, `format_round_trip`) on a nightly schedule + manual dispatch. Each fuzz target gets a 10-minute libfuzzer budget (`-max_total_time=600`) wrapped in a 30-minute hard wall-clock cap. Failures upload `fuzz/artifacts/<target>/` as a job artifact for triage. Closes ROADMAP §7-C's runtime portion (the harness already shipped in P10.2).

- [x] **14.5 `lint --csv` per-file timing restored + enhanced** (S) — `LoadReport` carries `loaded: Vec<(Uri, LoadTimings { read, parse })>` instead of `Vec<Uri>`, capturing file I/O and tree-sitter parse separately. `ProjectAnalysis::ModuleAnalysis` gained a per-module `timings: ModuleTimings { lower, resolve, analyze, lint }` populated by `rebuild` / `invalidate`. The CLI `--csv` columns are now `total_us,read_us,parse_us,lower_us,resolve_us,analyze_us,lint_us,nb_nodes,nb_diagnostics,filepath` sorted by total descending — surfaces both where the fixed regression went (parse durations are visible again) and the pipeline's deeper hot spots (lowering dominates over resolver/analyzer at ~25%/3%/3% on stdlib core).

- [ ] **14.6 Salsa retrofit** (M, profiling-driven) — moved up from P10.4 / P5.5. Still gated on profiling showing quadratic blow-up on multi-file edits in real workspaces; the pure-function design from P6.1 keeps the retrofit cheap when the signal arrives. **Not** required for M14 — listed here to consolidate the deferred work into the parity-finish phase rather than leave it dangling under P10.

- [x] **14.9 `private` semantics correction** (M, foundational pass) — two pieces landed: (a) `lower_type_ref` previously kept only the *first* `params:` child of a generic `type_ident`, so `Map<K, V>` lost `V` from the HIR (and thus the resolver never recorded a use of `V`'s name). Walking every `type_ident` named-child collects all params correctly. This was the dominant source of false positives — 4 of the 7 stdlib warnings were referenced via `Map<String, X>?` and silently dropped. (b) New `compute_qualified_refs` pass walks every module's CST for `static_expr` chains rooted at a known module name (`<module>::<name>` / `<module>::<type>::<name>`), and bumps the target decl's `references_to` count. The `unused-decl` lint is then re-run with the enriched counts. Stdlib lint baseline: 7 → 3 (the residual 3 are genuinely declared without any `.gcl` reference — runtime API surface). 3 new tests: HIR `map_two_generic_params_lower_both`, resolver `forward_ref_to_type_in_nested_generic_param`, project `qualified_access_keeps_private_decl_alive`.

- [ ] **14.7 Playground UI maturation** (M) — moved up from P10.5. Click-to-jump from CST / HIR / diagnostic rows back to a Monaco editor selection; LSP-in-web-worker so completion / hover / diagnostics fire in the Monaco editor itself, not just in side panels; `localStorage` persistence so refreshes don't lose the user's source. Discrete frontend project — the playground exists today (P5.2) and serves as the analyzer testbed; this chunk is the polish pass once the analyzer is parity-complete.

- [ ] **14.8 Playground project loading + exposed-API browser** (M) — two new playground capabilities once P11 (cross-module identity) and P13.4 (`ProjectIndex.exposed`) have landed:
  - **Load a project from disk.** Today the playground only edits a single in-memory buffer. Add a "Load project" entry point that walks a user-selected directory (browser File System Access API where available, falling back to a `<input type="file" webkitdirectory>` upload), recognizes `project.gcl` as the entrypoint, and feeds every reachable `.gcl` (via `SourceManager::load_project` semantics) into the wasm analyzer as a multi-doc `SourceManager`. The Monaco editor switches to a file-tree-aware shell so users can hop between modules; cross-module navigation (P11.3 / P11.4) hits real Locations.
  - **Exposed-API browser panel.** New right-rail tab consuming `ProjectIndex::exposed` (already populated by `ingest`). Lists every `@expose("rename")` site grouped by exposure key, with the local name, declaring file, and signature. Clicking an entry jumps the editor to the decl. Doubles as a "what's the runtime API surface of this project?" overview the CLI doesn't surface today.
  - Both rely on a wasm export that returns the `ProjectIndex.exposed` map shape (URI-relative paths, decl byte ranges) and a wasm entry that takes `Vec<(uri, text, lib)>` so the playground can drive a multi-doc analysis without round-tripping each file individually. Add those exports as part of this chunk.

**M14: published on crates.io; nightly fuzz green; diagnostic parity diff is empty over the corpus; formatter byte-for-byte parity (M9) met. The Rust port is 1:1 with the TS reference. (P14.6 / P14.7 / P14.8 ride alongside but don't gate the milestone — they're consolidated here so the deferred work has a single home.)**

---

### Phase 15 — Interactive-LSP regression sweep (~3-4 weeks)

**Goal:** close the LSP capability gaps that surfaced once attention shifted back to interactive editor behavior. The parity push (P11–P14) operated against the diagnostic corpus + fixture gauntlets and didn't catch capability-shape regressions: hover got thinner, completion was never wired in any phase, and `@include` / `@library` resolution failures go silent. P15 is the catch-up.

**Chunks:**

- [x] **15.1 Hover content restoration** (M) — three regressions restored on top of `capabilities::hover`:
  - **Doc-comments** above the declaration. `Decl::{Fn,Type,Enum}` already carry `doc: Option<String>` (populated by `lower::doc_text`); the hover renderer now prepends the trimmed doc paragraph above the signature code-block.
  - **Function signature.** New `render_fn_signature` walks `FnDecl.modifiers` / `generics` / `params` / `return_type` and emits `fn name<G>(p: T): R` (modifiers prefix when present); `render_type_signature` mirrors it for `type` decls. `Decl::{Var, Pragma}` get terse signatures too. Type-attr / method member hovers pick up the same shape.
  - **Symbol provenance.** New `hover_with_project(text, lib, root, pos, uri, &ProjectAnalysis, &SourceManager)` consumes the cached `ModuleAnalysis` (so cross-module names actually resolve to `Definition::ProjectDecl`), looks up the foreign decl's HIR, and renders its full doc + signature followed by an italic `*defined in `<module>`*` footnote (`<module>` = foreign URI's file stem). Intra-module idents skip the line; the runtime `Definition::Project` placeholder renders as "(runtime built-in) name". `hover_handler` in `server.rs` now routes through `hover_with_project`. 4 new unit tests in `lsp_capabilities.rs` cover full-fn-signature rendering, doc-comment inclusion, generic-param rendering, and cross-module provenance.

- [x] **15.2 Completion capability — base implementation** (L) — LSP `textDocument/completion`. The TS reference at [packages/server/src/index.ts:401](file:///tmp/lang/lang/packages/server/src/index.ts) (server hookup) and [packages/lang/src/project/analysis_result.ts:738](file:///tmp/lang/lang/packages/lang/src/project/analysis_result.ts) (`AnalysisResult.completion`) shows the minimal cases. Split into 7 sub-chunks (P15.2.1–P15.2.7) below — all landed:

  - [x] **15.2.1 Pragma completion after `@`** (S) — `capabilities::pragma_completion(text, cursor_byte)` walks back from the cursor over `[A-Za-z0-9_]*` and confirms the run is preceded by `@`; on success emits the TS reference's `PRAGMA_COMPLETION_ITEMS` list (`@library`, `@include`, `@role`, `@permission`, `@expose`, `@volatile`) prefix-filtered case-insensitively against the typed run. Snippet items (`@library`, `@include`, `@role`, `@permission`) carry tabstop bodies so editors that honor `InsertTextFormat::Snippet` get parametric expansion. Trigger character `@` advertised in `initialize`. 2 new tests in `lsp_capabilities.rs` cover the empty-prefix and `@li`-prefix cases.
  - [x] **15.2.2 Keyword completion at statement / expression positions** (S) — `capabilities::keyword_completion` walks back from the cursor over `[A-Za-z0-9_]*`, gates on `is_keyword_position` (skips strings / comments / annotations / type-ident slots / `.` / `:` / `>` / `@` separators), and emits the full grammar-baked keyword set (`abstract`, `as`, `at`, `catch`, `do`, `else`, `enum`, `false`, `fn`, `for`, `if`, `in`, `is`, `native`, `null`, `private`, `return`, `static`, `this`, `throw`, `true`, `try`, `type`, `var`, `while`) prefix-filtered. 3 new tests cover `re|` → `return`, no-keyword-leak inside strings, and no-keyword-leak after `.`.
  - [x] **15.2.3 Scope-aware ident completion** (M) — new `capabilities::completion_with_project(text, root, pos, uri, project, project_root)` is the project-aware entry point; `completion_handler` in `server.rs` routes through it so locals / params / generics / in-module decls + cross-module project surface (`ProjectIndex::decl_locations` / `values` / `module_names` / `BUILTIN_RUNTIME_TYPES`) all surface alongside keywords. Scope walker (`scope_names_at` + `collect_fn_scope` + `collect_stmt_scope` + `stmt_byte_range`) walks the cached HIR top-to-cursor: module-level decls always visible (forward refs allowed), enclosing fn / type adds params + generics, blocks add `var` bindings whose `byte_range.end <= cursor_byte`, and for / for-in / try / at descend into matching branches. `CompletionItemKind` per shape (FUNCTION / CLASS / ENUM / VARIABLE / TYPE_PARAMETER / MODULE / CONSTANT). Sort priority orders locals > params > generics > module decls > project surface > keywords. 3 new tests cover locals + decl + keyword interleaving, future-local exclusion, and runtime-type surface.
  - [x] **15.2.4 Member completion after `.` / `->`** (M) — `member_completion` detects `.` / `->` separators by walking back over the typed prefix and checking the byte before, then resolves the receiver's `TypeId` via a 3-stage `receiver_type_at`: (1) HIR fast path matching an `Expr` whose byte_range ends at the separator; (2) CST + resolver fallback when the receiver lowered into the HIR but the parser dropped the property side; (3) CST + name-in-scope fallback (`lookup_name_type_at`) for the common `recv.|` case where tree-sitter's error recovery wraps the receiver in an ERROR node and the lowering skips it. Type-head extraction (`type_head_name`) drops nullability / generic args; cross-module receivers fall back to `ProjectIndex::decl_locations`. Static methods are skipped on instance access (P15.2.5 owns the `::` path). Auto-deref via `@deref` is a refinement and ships in a follow-up. 3 new tests cover same-module attr+method listing, prefix filtering, and cross-module member access.
  - [x] **15.2.5 Static completion after `::`** (M) — `static_completion` walks back from the cursor over the typed prefix, confirms `::` precedes, and extracts the receiver via `static_receiver_at`. Two dispatch shapes: (a) receiver matches a known type decl (in-module `type_decls` *or* cross-module `decl_locations`) → emits its `static` methods; (b) receiver matches a known module name (`ProjectIndex::module_names`) → emits the foreign module's top-level decls (FUNCTION / CLASS / ENUM / VARIABLE per shape). Chain context (`module::Type::|`) is transparent — receiver lookup by name still finds `Type` and emits its statics. Trigger characters `.`, `:`, `>` added to `initialize`. 2 new tests cover `Type::` static methods and `module::` decl listing.
  - [x] **15.2.6 Type-position completion** (S) — `type_position_completion` fires when the cursor sits inside a `type_ident` ancestor (`var x: |`, `<|`, fn param / return type, etc.). Emits only type-shaped names: in-module `Type` / `Enum` decls, in-scope generic params (filtered out of `scope_names_at` by kind), every `decl_locations` whose first hit is a `Type` / `Enum`, `BUILTIN_RUNTIME_TYPES`, primitives, and module names (so `module::Foo` chains start cleanly). Skipped on RHS of member / annotation chains; the static `module::|` case remains owned by P15.2.5. New test asserts MyShape / Map surface but `helper` (a fn) and `return` (a keyword) don't.
  - [x] **15.2.7 Object literal field completion** (S) — `object_field_completion` walks up to find the enclosing `object_initializers` / `object_fields` body, then to the `object_expr` parent's `type_ident` head. Resolves the type via the per-module `type_decls` first, then cross-module via `decl_locations`, and emits each `attr.name` as a `CompletionItemKind::FIELD` (with `: ` appended in `insert_text` so editors land the cursor in the value slot). Methods are omitted. New test asserts `Point { x: 1,  | }` surfaces `x` and `y` but not `norm`.

  Trigger characters: `.`, `:`, `>`, `@`, `"` (existing). `completion_provider` advertised in `initialize` with the full set. Test scaffold lives in `lsp_capabilities.rs`. **Gated on P15.6 + P15.7 (already landed).** Completion routing also has to handle the `@include` shape from P15.4 — keep that as the existing first dispatch case.

- [ ] **15.3 `@library` version completion against the GreyCat registry** (M) — the regression the user surfaced. TS reference at [packages/server/src/registry.ts](https://hub.datathings.com/greycat/lang) does this as a *lazy* completion: the analyzer's `completion()` returns a single placeholder `LazyCompletionItem` carrying the lib name + the version-slot range; the LSP layer then fetches `https://get.greycat.io/files/<name>/` to enumerate branches, recurses into `<branch>/<major.minor>/<arch>/` (preferring `x64-linux` over `noarch`), collects every `*.zip` filename + last-modification date, and returns those as concrete completion items with the date in `labelDetails.description` and `sortText` preserving the registry's semver-descending order. Channel-aware: when the cursor's existing text already has a `-dev` / `-beta` prerelease tag, filter the registry results to that channel. Special case: `std` is aliased to `core` at the registry root. Mirror the same two-stage shape in the Rust port: detect the version slot in `capabilities::completion` (P15.2) and emit a placeholder; resolve the placeholder against the registry in a completion-resolve path.

  **Architecture — must work in both LSP and WASM/Monaco.** The registry-walk algorithm lives in a shared crate (probably `greycat-analyzer-core::registry` or a new `greycat-analyzer-registry` crate) behind a `RegistryFetcher` trait that takes a URL and returns the JSON listing. Two backings:
  - **Native** (LSP server, CLI). Sync HTTP client (`ureq` is the lightest option; `reqwest::blocking` is the fallback). Cache lives in-process behind a `RwLock<HashMap<String, (Instant, Vec<Version>)>>` with a TTL.
  - **WASM** (playground / Monaco). The browser's `fetch` is the only HTTP available, and it's `Promise`-shaped. Two viable paths: (a) `web_sys::window().fetch_with_str(...)` returning a `JsFuture` — the WASM completion entry then has to be `async` (i.e. `wasm-bindgen` returns a `Promise` to JS); (b) a JS-side callback the playground passes in (`pass_registry_fetcher: js_sys::Function`) that the Rust code invokes — keeps the Rust API sync and lets the playground decide how to fetch (and cache via the browser's HTTP cache / a service worker). Path (b) is the lighter option and matches how the resolver `Context` already abstracts I/O for native vs. WASM (see P1.1).
  Pick (b) unless there's a profiling reason not to — it keeps the WASM bundle from carrying `web_sys::Fetch` machinery, and the playground's existing `wasm.ts` initializer (P5.2) is the natural place to inject the JS fetcher.

  **Better than the TS impl:** the TS version walks 4 nested `fetch` calls per request and re-fetches on every keystroke. Improvements that drop out of the trait-backed design: (a) cache the registry response per-`name` with a TTL so consecutive completions in the same session are free (works in both LSP and WASM via the JS-side fetcher's cache); (b) parallelize the per-major.minor fetches — TS awaits them serially in a `for` loop, which is most of the latency. (c) Worth checking with the registry maintainers whether `get.greycat.io` exposes a flatter endpoint that returns the full version listing in one round trip before porting the 4-fetch dance verbatim.

  Out of scope here: `@library("<cursor>", ...)` *name* completion. Doesn't exist in the TS reference; follow-up chunk if wanted. `@include("<cursor>")` directory completion is **in scope as P15.4** since it's a clean filesystem walk.

- [x] **15.4 `@include("<cursor>")` directory completion** (S, opportunistic) — new `capabilities::completion(text, root, pos, project_root)` is the foundational LSP completion entry point; today it dispatches only to the `@include` directory case but leaves room for P15.2 / P15.3 extensions. Activated when the cursor sits inside a `string` child of a `mod_pragma` whose annotation name is `include`. Walks the project root via `std::fs::read_dir` (one level, no recursion) and emits a `CompletionItem { kind: FOLDER }` per subdirectory, case-insensitively prefix-matched against whatever the user has typed before the cursor. Skips conventional ignore dirs (`node_modules`, `gcdata`, `.git`, `target`, `lib`, `bin`, `files`, `webroot`) plus any dotfile-named dir. `completion_provider` advertised in `initialize` with trigger character `"`. New `completion_handler` in `server.rs`. WASM no-ops because `project_root` is `None` until P14.8 wires multi-doc loading. New unit test: `completion_inside_at_include_lists_subdirs`.

- [x] **15.5 `@include` / `@library` resolution diagnostics** (S) — new `core::diagnostics::pragma_diagnostics(text, &ModuleDesc, project_dir, &dyn Context)` walks the parsed module's pragma list and emits four diagnostic codes:
  - `unresolved-include` — `@include("...")` whose target dir doesn't exist under `project_dir`.
  - `unresolved-library` — `@library("name", ...)` not found at `<project_dir>/lib/<name>` (and not under `<greycat_home>/lib/std/` for the `std` fallback).
  - `duplicate-include` / `duplicate-library` — second-and-later occurrences of the same pragma in the module.
  All four are `WARNING` severity, `source: "greycat-analyzer"`, with byte ranges anchored on the offending `mod_pragma` so the LSP underlines them and the cli compact-format renders `path:line:col`. `Backend.publish_for` re-runs `pragma_diagnostics` on every publish (so edits to a pragma reflect immediately) using a project root captured during `load_workspace`. Cli `lint` runs the same function on every loaded doc. Loader-side string error duplicates removed (`process_includes` no longer pushes "directory not found" into `LoadReport.errors`; cli no longer prints `unresolved_libraries` as free-form warnings — both are now subsumed by the typed diagnostics). 5 new core unit tests cover all four codes plus the `std`-falls-back-to-greycat-home positive case.

- [x] **15.6 Cross-module `static_expr` resolution (FQN goto-def + binding)** (M) — analyzer's `Expr::Static` arm now lowers the receiver type ref *and* runs `resolve_member(recv_ty, s.property)`. In-module hits land in `member_uses`; cross-module hits go through the existing `deferred_member_uses` → pass 3 (`resolve_cross_module_members`, P11.5) pipeline and end up in `foreign_member_uses`. The previous single line (`let _ = self.lower_type_ref(s.ty)`) silently dropped the property — that's why `Identity::create` produced no goto-def hit. The LSP path needed no change: `goto_definition_handler` already consults `foreign_member_lookup` after the `ProjectDecl` branch (P11.5) and uses `cross_module_member_location` to compute the foreign attr/method name range. New test in `lsp_capabilities.rs::cross_module_static_call_binds_foreign_method` mirrors the `.`-shape test from P11.5 for the `::` shape.

- [x] **15.7 Static-expr / static-call type inference** (S) — new pass 3.5 in `ProjectAnalysis::rebuild` (between cross-module member resolution and the qualified-refs pass) infers types for four previously-`any` shapes:
  - `var x = Identity::create("root", "root");` → `x: Identity` (call's return type translated from the foreign method's `return_type` into the local arena).
  - `var y = Identity::create;` → `y: function` (method reference; `function` is a `native type` in `lib/std/core.gcl`).
  - `var z = Identity::id;` → `z: field` (attr reference; `field` is also a `native type` in core.gcl).
  - `var w = runtime::Identity;` → `w: type` (module-prefixed type ref; `type` is a `native type` too).
  Two-phase: phase 1 walks every module's HIR read-only and snapshots the foreign return type into an arena-free `TypeShape` enum; phase 2 mints those shapes into each affected module's `TypeArena` and updates `expr_types[call_id]` and (for `var x = ...` initializers without a declared type) re-links `def_types[name]` so inlay hints / hover / downstream inference see the right type. The fourth shape (`module::Decl`) leans on a new `ProjectIndex.module_names: HashMap<String, Uri>` map (populated by `ingest` from the URI's filename stem); the resolver also consults `has_module(name)` so leftmost segments of `module::X::Y` chains aren't flagged as unresolved. Generic substitution across modules (e.g. `Array<T>::create()` returning `Array<T>` where `T` is bound at the call site) keeps the generic head as-is without binding `T` — follow-up. 4 new tests in `lsp_capabilities.rs` cover the four shapes.

- [x] **15.8 FQN `module::Type::member` lowering + inference** (M) — chained `static_expr` (`runtime::Identity::create`, `runtime::Identity::id`, etc.) used to lower to `Expr::Unsupported` because HIR's `StaticExpr` had `ty: Idx<TypeRef>` and the grammar's chained head is itself a `static_expr` rather than a `type_ident`. Two pieces:
  - **New HIR variant `Expr::QualifiedStatic { chain: Vec<Idx<Ident>>, byte_range }`.** The lowering's `static_expr` arm detects chained heads and walks the chain into a flat `Vec<Idx<Ident>>`. Simple `Type::name` shapes still use the existing `Expr::Static`. New helper `collect_static_chain_idents` in `lower.rs`.
  - **Pass 3.5 extension.** New `resolve_qualified_static_shape` / `resolve_qualified_static_call_shape` / `resolve_qualified_chain` helpers in `project.rs` follow `chain[0]` → `ProjectIndex::module_uri` → foreign module → `chain[1]` (TypeDecl) → foreign type → `chain[2]` (Attr / Method). Method-ref → `function`, attr-ref → `field`, call-of-method → method's foreign return type translated via `read_type_shape` + `mint_type_shape`. The `def_types[name]` re-link from P15.7 picks up `var x = <chain>` shapes automatically.
  - **Acceptance:** `var x = runtime::Identity::create("a", "b");` infers `x: Identity` (matching the unprefixed form's behavior from P15.7). `var y = runtime::Identity::create;` infers `y: function`. New analyzer / resolver / capability arms cover `QualifiedStatic` (analyzer returns `any` from the per-module pass and pass 3.5 overrides; resolver records the leftmost segment via `record_use`; `Expr::Unsupported` regression audit stays empty). 2 new tests in `lsp_capabilities.rs` cover the chained call + chained method ref.

  **Chain-segment hover/goto follow-up landed.** New `AnalysisResult.foreign_decl_uses: HashMap<Idx<Ident>, ForeignDecl>` map binds chain[1] (the type segment) to its foreign top-level decl. Pass 3.5 populates both `foreign_decl_uses` (chain[1]) and `foreign_member_uses` (chain[2]) when resolving every `Expr::QualifiedStatic`. `ident_hover_markdown` now also consults `foreign_member_lookup` (closes a P15.6 gap where cross-module `Type::method` hover fell through to "expression: function") and `foreign_decl_lookup`. `goto_definition_handler` consults `foreign_decl_lookup` between the `ProjectDecl` branch and the `foreign_member_lookup` branch. 3 new tests cover hover on the static-method case, the chain-type segment, and the chain-member segment.

- [x] **15.10 Call-site argument-type validation** (S) — new pass 3.6 in `ProjectAnalysis::rebuild` (after pass 3.5 settles static-expr return types) walks every module's `Expr::Call`, resolves the callee's declaring `Decl::Fn` via a new `resolve_call_target` helper (covers `Expr::Ident → Decl/ProjectDecl`, `Expr::Static → member_uses/foreign_member_uses`, and `Expr::QualifiedStatic → resolve_qualified_chain`), and emits `value of type X is not assignable to parameter Y: T` for each mismatched arg. Foreign param types are translated into the caller's arena via `read_type_shape` + a clone of the caller's `TypeArena` (the arena gained a `Clone` derive — cheap because both fields are `Vec<Type>` / `HashMap`). Pass 3.5 also gained a 1a-tris sub-step that turns bare ident references into the right runtime type (`Identity` used as a value → `type`, `someFn` used as a value → `function`), and pass 3.6's diagnostic byte-range special-cases `Expr::Ident` (whose own `byte_range()` returns `0..0`) to use the ident's arena entry. **Acceptance:** the user's baseline (project.gcl) produces exactly the four expected `value of type 'int' is not assignable to parameter '_: <type>'` diagnostics, no more no less. 2 new tests cover the mismatch case + the bare-type-ident-as-value case.

  Generic callees still validate exclusively through `try_generic_call_inference` (P12.1) — pass 3.6 skips them so generic-param mismatch errors don't double up.

- [x] **15.9 Goto-def on module-prefix idents** (S) — new `capabilities::goto_module_segment` is consulted from `goto_definition_handler` *before* the regular HIR-based goto path. When the cursor sits on an `ident` whose parent is a `type_ident` whose grand-parent is a `static_expr` (the leftmost segment of any `module::X` / `module::X::Y` chain), and the cursor's text matches a known module's `Document::name()`, the handler returns a `Location` pointing at line 0 of that module's URI. Pure CST walk — independent of how the chain ultimately lowers in HIR (P15.8 is still pending), so it works today on the user's `runtime::Identity::create` baseline. New unit test in `lsp_capabilities.rs`.

**M15: hover restores doc / signature / provenance; cross-module `Type::method` resolves and infers correctly (`Identity::create`, `runtime::Identity::create`, method-references, attr-references all infer the right type, not `any`); basic completion lights up in the LSP and the WASM/Monaco playground (sharing the same algorithm via a `RegistryFetcher` trait — native HTTP for the LSP, JS-callback fetch for WASM); `@library` version completion hits `get.greycat.io` in both targets and matches TS-reference behavior; `@include` directory completion is wired where the project root is known; `@include` / `@library` typos and duplicates are surfaced as diagnostics in both `cli lint` and the LSP.**

---

### Phase 16 — Member-flow inference & node-deref (~2-3 weeks)

**Goal:** chained member expressions (`x.s.size()`), method calls on primitive / cross-module receivers, and node-deref completion all behave the way the user expects. Closes the three bugs surfaced against the canonical [project.gcl](../project.gcl):

1. `var s = x.s.size();` infers `s: int`, not `s: any` — the `Expr::Member` arm's `self.any()` short-circuit hides the attr's declared type, and `Expr::Call` short-circuits the same way.
2. Completion on `x.s.|` (where `x.s: String`) lists `String`'s methods (`size`, `to_lower`, etc.) — `resolve_member` doesn't know how to find members on a `Primitive` receiver.
3. Completion on `x: node<Foo>` `.|` surfaces both node's own methods *and* `Foo`'s attrs/methods with a `.` → `->` rewrite — the TS reference does this via `derefType.suggest()` + `additionalTextEdits`.

**Chunks:**

- [x] **16.1 Member-expr typing (intra-module)** (S) — `Expr::Member` / `Expr::Arrow` arm in `analyzer.rs` now consults `member_uses[property]` after `resolve_member`. `MemberDef::Attr(id)` returns the attr's `lower_type_ref(ty)`; `MemberDef::Method(_)` returns `Named("function")`. Cross-module bindings (in `foreign_member_uses`) still type as `any` here — pass 3.7 (P16.3) writes those back. 2 new tests: `member_attr_typing_matches_attr_decl_type`, `member_method_ref_types_as_function`. Closes the user's `project.gcl` baseline bug 1 for the intra-module shape.

- [x] **16.2 Primitive receiver member resolution** (S) — `resolve_member` now extracts a type name from `TypeKind::Primitive` (e.g. `Primitive::String → "String"`, `Primitive::Int → "int"`) and falls through to the existing `type_decls` / `decl_locations` lookup path. Lets `"hello".size()`, `1.to_string()`, etc. bind to the foreign primitive type's methods. (Effective once 16.3 / 16.4 light up cross-module typing — primitives' methods live in stdlib.)

- [ ] **16.3 Member-expr typing (cross-module)** (M) — new `ProjectAnalysis` pass 3.7 (after pass 3.5 settles static-call returns and pass 3 binds foreign members) walks each module's `foreign_member_uses[ident] = ForeignMember { uri, member: Attr(attr_id) }` and writes the foreign attr's translated declared type into the local module's `expr_types` for the parent `Expr::Member` / `Expr::Arrow`. Same pass re-links `def_types` for `var x = recv.attr` shapes so inlay hints / hover / downstream inference all see the right type. Mirrors the P15.7 / P15.8 `read_type_shape` + `mint_type_shape` pattern.

- [ ] **16.4 Call-on-member return-type inference** (M) — extend `infer_cross_module_call_types` in [project.rs:250](../greycat-analyzer-analysis/src/project.rs#L250) to recognize `Expr::Call { callee: Expr::Member(..) | Expr::Arrow(..) }`. Pull the method via `member_uses[property]` (intra-module) → `foreign_member_uses[property]` (cross-module), translate its `return_type` into the caller's arena, write it onto the call's `expr_types`. Same writeback re-links `def_types` for `var s = recv.method(...)`. This is what flips `x.s.size()` from `any` to `int`.

- [ ] **16.5 node-tag / `@deref` auto-deref (analyzer + completion)** (M) — coupled fix so navigation and completion arrive together:
  - **Completion side** ([capabilities.rs](../greycat-analyzer-server/src/capabilities.rs) `member_completion`): when receiver type is `Generic { name: <is_node_tag>, args: [inner] }` *or* the receiver type carries `@deref("method")` (P13.5's `TypeFlags::deref`), and the separator is `.`:
    - Emit the receiver type's own members verbatim (current behavior).
    - Plus the inner / deref'd type's members each carrying `additional_text_edits: [TextEdit { range: <sep_range>, new_text: "->" }]` so accepting them rewrites `.` → `->` in source.

    When the separator is `->`: list the inner / deref'd type's members directly (no rewrite needed).
  - **Analyzer side** (`resolve_member`): `Expr::Arrow` with a node-tag / `@deref` receiver resolves the property against the inner type, recording the binding into `member_uses` so 16.1's typing, hover, and goto-def all light up for `n->field` shapes. `Expr::Member` (regular `.`) with a node-tag receiver still resolves against the tag's own methods only — the `.` → `->` rewrite advice from completion is what lets users *type* the right shape; the analyzer doesn't silently auto-deref `.`.

- [ ] **16.6 `arrow on non-deref receiver` lint** (S) — new `LintRule` (rule code: `arrow-on-non-deref`) walks every `Expr::Arrow` and emits an error when the receiver's type is neither a node tag (`is_node_tag` from `greycat-analyzer-types`) nor carries `@deref(...)` in `ProjectIndex::type_flags`. Severity `Error` so it surfaces in cli `lint` and the LSP red-squiggle layer. Mirrors the runtime's "cannot deref" rejection — caught at edit time instead of run time.

  Out of scope: chained generic inference (`Array<int>::iter().next()`). The runtime doesn't currently support chained generic instantiation either, so we'd be inventing semantics.

**M16:** `cargo run -- lint project.gcl` reports zero false `any` types in `var s = x.s.size()` (s infers `int`); LSP `textDocument/completion` on `x.|` where `x: node<Foo>` returns both node's own method list and `Foo`'s attrs with the `.→->` rewrite; cross-module `recv.attr.method()` chains type correctly through the cached `ProjectAnalysis`; `recv->prop` on a non-deref receiver fires an `arrow-on-non-deref` error.

---

### Phase 17 — Real-corpus parity ratchet (~1-2 weeks)

**Goal:** drop the false-positive count on real-world projects to (near) zero, so the diagnostic-parity oracle (P14.2) can ratchet meaningfully. Driven by [tests/parity/registry_baseline/REPORT.md](../tests/parity/registry_baseline/REPORT.md), which captured 224 false-positive diagnostics on `~/dev/datathings/greycat/apps/registry` against TS reference's 0. Every chunk targets a specific bucket from that report.

P17 sits in parallel with P16: P16 handles "Bucket A" (member-flow `any` cascades) via the typing pipeline; P17 handles the lowering / lint-policy bugs that surface as Buckets B-F regardless of typing.

**Chunks:**

- [x] **17.1 Capture baseline** (S) — done in this session. [tests/parity/registry_baseline/](../tests/parity/registry_baseline/) holds the TS + Rust outputs verbatim plus a bucketed analysis. The `REPORT.md` is the source-of-truth tracking doc for everything below.

- [x] **17.2 `for-in` lowering + tuple-form support** (M) — fixed [`hir/lower.rs`](../greycat-analyzer-hir/src/lower.rs)'s `for_in_stmt` arm which read `child_by_field_name("iterator")` (the *iterable* expression, not a param wrapper) and asked for `name` on it, dropping the entire `for_in_stmt` from the HIR via the `?` short-circuit. New shape walks named children for `for_in_param` nodes. `ForInStmt` now carries `params: Vec<ForInParam { name, ty }>` instead of a single `iterator_name` / `iterator_type` pair, so the grammar's `sepBy2(",", $.for_in_param)` (every for-in is at least 2-tuple) is representable. Updated callers in resolver / capabilities scope walker / `lookup_name_type_in_stmt`. New resolver test `for_in_tuple_form_binds_both_params`. **Acceptance:** registry baseline went from 103 → 31 warnings (–70%) as for-in bodies finally reach the resolver. Errors went 121 → 183 because the body now visits with `any`-typed receivers — Bucket A territory, closes with P16.

- [x] **17.3 `catch (ex)` param binding** (XS) — `_catch_param` is a hidden rule (`seq("(", $.ident, ")")`), so tree-sitter inlines its children into the parent and tags every child with the `error_param` field. The previous lowering asked for a non-existent `name` sub-field; my first attempt picked the first field-tagged child (the `(` token); the working fix uses `children_by_field_name("error_param", &mut cursor).find(|c| c.kind() == "ident")`. New resolver unit test `catch_param_binds_in_catch_block` locks the binding in.

- [x] **17.4 `@library` webroot fallback** (S) — `pragma_diagnostics`'s resolution check now considers four locations before flagging an unresolved library: `<project>/lib/<name>/`, `<project>/webroot/<name>/`, the `<project>/lib/installed` manifest (`greycat install`'s record of names → versions, counts even when the directory hasn't been materialized), and the `std` fallback under `<greycat_home>`. Asset-only libraries (e.g. `explorer` ships as a webroot bundle with no `.gcl`) now resolve cleanly. 2 new tests cover the webroot and manifest cases. **Acceptance:** registry baseline's `@library('explorer'): library not found` warning is gone.

- [ ] **17.5 String-interpolation `${expr}` lowering** (M) — [`hir/types.rs:365`](../greycat-analyzer-hir/src/types.rs#L365) `StringExpr.value` stores raw fragments concatenated; the `${expr}` parts are dropped before the resolver sees them. Replace with `StringExpr { parts: Vec<StringPart> }` where `StringPart::Lit(String) | Interp(Idx<Expr>)`. Lowering walks `string` node's `string_fragment` + interpolation children. Resolver visits `Interp` exprs. Knock-on effects: analyzer's type rules need to handle interpolation as type-checked sub-exprs (each `Interp` should accept any value expressible as a `String` via runtime `to_string()`); the formatter and `Expr::Unsupported` audit need to keep working. **Acceptance:** locals / params referenced only inside `"${name}"` interpolations are no longer flagged as unused; new HIR test for `"hello ${name}"` exposing two `StringPart`s.

- [ ] **17.6 Gate unused-local / unused-param behind `--lint-unused`** (S) — TS reference doesn't emit unused-local / unused-param at all by default. Gate the [`unused-local`](../greycat-analyzer-analysis/src/lint.rs) + [`unused-param`](../greycat-analyzer-analysis/src/lint.rs) lint rules behind a CLI flag (`greycat-lang lint --lint-unused`) and an LSP setting (later — for now, off in the LSP path too). Default `lint` matches TS's "trust the user" policy. **Acceptance:** registry baseline's 31 + 23 unused-param/local warnings disappear at the default cli flag set; explicit `--lint-unused` re-enables them.

- [ ] **17.7 Re-baseline + parity ratchet test** (S) — re-run both linters on the registry project after P17.2-6 land, regenerate [registry_baseline/](../tests/parity/registry_baseline/), and add a CI test (`registry_parity_floor`) that asserts the rust output's diagnostic count is `<= N` for some N that ratchets toward zero. Mirrors the formatter parity gauntlet's `MATCH_FLOOR` pattern. P14.2 (the diagnostic parity gate) consumes this as one of its corpora.

**M17:** `greycat-analyzer lint` on `~/dev/datathings/greycat/apps/registry` produces ≤ 5 residual diagnostics (all genuine), down from 224. The registry-parity CI gate is green and the floor is live.

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
- `greycat-analyzer-server/src/{server.rs, backend.rs, project.rs}` — lifecycle plumbing kept; capability handlers added in P3.
- `greycat-analyzer/src/{main.rs, cmd.rs, cmd/lint.rs, cmd/lang_server.rs, utils.rs}` — CLI structure kept; subcommand bodies rewritten as features land.

---

## 10. Sequencing & timeline

```
P0  [4-6w]   Foundation reset ──────── M1
P1  [2-3w]   Project + parse diags ─── M2
P2  [10-16w] Semantic layer ────────── M3   ← dominates the original push
P3  [4-6w]   LSP capabilities ──────── M4
P4  [3-4w]   Formatter + linter ────── M5
P5  [2-3w]   Distribution
P6  [8-12w]  Analyzer 1:1 with TS ──── M6   ← dominates the parity push
P7  [3-5w]   Grammar + HIR completion ─ M7
P8  [4-6w]   LSP 1:1 with TS ───────── M8
P9  [4-6w]   Formatter byte-parity ─── M9
P10 [4-6w]   Distribution + quality ── M10
P11 [3-5w]   Cross-module identity ── M11   ← unblocks P12-P14 cross-module work
P12 [4-6w]   Type system completion ─ M12
P13 [3-4w]   Analyzer parity closeout ─ M13
P14 [2-3w]   Final parity gate ────── M14   ← the "are we 1:1?" gate
P15 [3-4w]   Interactive-LSP sweep ── M15   ← hover / completion / pragma diags
P16 [2-3w]   Member-flow + node-deref M16   ← member-call typing, auto-deref completion
P17 [1-2w]   Real-corpus parity ratch M17   ← lowering bugs surfaced by greycat/apps/registry
```

Total realistic envelope: **13-19 months full-time** end-to-end. P0–P5 (the original ~6 months) ships scaffolding plus enough behavior to be useful; P6–P10 (another ~6-12 months) closes the foundational gap to 1:1 parity with the TS reference and adds the harness infrastructure; P11–P14 (~3-5 months) are the parity-push closeout that turns harnesses into gates and the foundational passes into 1:1; P15 (~3-4 weeks) catches the interactive-LSP capability gaps the corpus-driven parity push doesn't surface; P16 (~2-3 weeks) tightens up the member-access type chain that pass 3.5 left at `any`; P17 (~1-2 weeks) drains the lowering / lint-policy bucket surfaced by running against a real project.

Front-load the snapshot harness (P0.6) — it pays off across the entire project, especially through P2 and P9. The cross-port diagnostic parity oracle (P10.3 → P14.2) is the ultimate "are we 1:1?" answer; everything before it is a steppingstone.

P11 is on the critical path for P12–P14 because most cross-module capabilities (member resolution across modules, scope-aware rename / references / goto-def across modules, real `Definition::Project` data) blocks behind a global decl table. P12 and P13 can run in parallel after P11. P14 gates on all of P11/P12/P13. P15 leans on P11 (provenance) and P11.5 (cross-module member completion) but is otherwise independent — it can run in parallel with P14's CI-gate work.

---

## 11. How to update this doc

The roadmap moves with the work.

- Check off chunks (`[ ]` → `[x]`) as they land.
- When an Open Question (§4) is answered, fold the answer into the relevant Decision (§3) or Phase chunk and remove the question.
- When a phase finishes, leave the phase in place — keep the milestone, mark all chunks done, link to the commit/PR that delivered M_n.
- Do **not** rewrite history. New constraints get a new chunk, not an edit to an old one.
