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
- [ ] **1.4 Parse diagnostics + LSP publish** (S) — walk tree-sitter `ERROR`/`MISSING` nodes, convert to `lsp_types::Diagnostic`, publish on `did_open`/`did_change`/`did_save`. Replace the parse-only output in `cmd/lint.rs` with a real `Diagnostic` formatter.
- [ ] **1.5 CST utility surface** (S) — `node_at_offset`, `ancestors`, `children_by_field`, `text_of` — replace the retired `node_query.rs`/`cursor.rs`.

**M2: open a workspace with `project.gcl`; LSP shows red squiggles for all syntax errors across all reachable modules; `cli lint path/` exits non-zero with formatted diagnostics matching TS reference shape.**

---

### Phase 2 — Semantic layer (~10-16 weeks, the bulk)

**Goal:** port `packages/lang/src/analysis/` (~10k TS LoC). This phase dominates the project.

**Chunks:**

- [ ] **2.1 HIR scaffolding** (L) — lower tree-sitter CST to a typed HIR (declarations, expressions, types, patterns). Single typed-AST + type arena, in its own crate. (Decision B.)
- [ ] **2.2 Crate split** (S, parallel with 2.1) — add `greycat-analyzer-hir`, `-types`, `-analysis`. Final layout per §5.
- [ ] **2.3 Symbol resolver / name binding** (L) — port `resolver.ts` (1,145) + `environment.ts` (890) + `env_manager.ts`. Produces: definition table, scope tree, reference index.
- [ ] **2.4 Type system core** (XL) — port `types.ts` (2,811): the `Type` enum, subtyping, generics, function signatures, nullable types, generic substitution.
- [ ] **2.5 Analyzer** (XL) — port `analyzer.ts` (4,514): type inference, control-flow narrowing, null-flow, exhaustiveness, unused-decl checks. Plus `declarator.ts`, `hinter.ts`, `actions.ts`.
- [ ] **2.6 Stdlib ingestion** (M) — load `lib/std/*.gcl` as ordinary modules under a synthetic `@library/std/...` URI. Native-bound functions captured in a small Rust metadata table (Decision F).
- [ ] **2.7 Semantic diagnostics → LSP** (S) — pipe analyzer diagnostics through the same `publish_diagnostics` pipeline from 1.4.

**M3: `cargo run -- check lib/std/*.gcl` reports zero diagnostics; LSP shows semantic errors on a deliberately broken user file.**

---

### Phase 3 — LSP capabilities (~4-6 weeks)

**Goal:** light up the 15 capabilities tested in `lsp.*.test.ts`.

Once Phase 2 lands, each capability is a thin wrapper over HIR + reference index + types.

**Chunks (each S–M):**

- [ ] **3.1** Hover + signature help — needs types, resolver.
- [ ] **3.2** Goto definition + goto implementation — needs reference index.
- [ ] **3.3** Document symbols + workspace symbols.
- [ ] **3.4** Find references + rename (M) — rename needs careful CST text-edit construction.
- [ ] **3.5** Document highlight + selection ranges + folding ranges — pure CST.
- [ ] **3.6** Code actions + quickfixes (M) — depends on which TS code actions exist.
- [ ] **3.7** Inlay hints — needs inferred types.
- [ ] **3.8** Semantic tokens (M) — port `highlighter.ts` over tree-sitter queries.

**M4: every LSP capability the TS server advertises is wired and returns non-empty results on a sample workspace; ported `lsp.*.test.ts` scenarios pass as Rust integration tests.**

---

### Phase 4 — Formatter + linter + CLI parity (~3-4 weeks)

**Chunks:**

- [ ] **4.1 Formatter** (M) — port `pp/` + `parser/cst/cst_format.ts` (~779 LoC) over tree-sitter CST. Add `cli fmt` and LSP `textDocument/formatting`.
- [ ] **4.2 Linter rules** (M) — port `cli/src/lint/` (242 LoC) + any rule logic embedded in analyzer. Re-expose via `cli lint` (replace current parse-only stub) and LSP diagnostics with `source: "lint"`.
- [ ] **4.3 CLI parity sweep** (S) — match TS CLI subcommands and flags exit-code-for-exit-code.

**M5: `cli fmt --check lib/std/` is idempotent and matches TS prettifier output byte-for-byte on the corpus; `cli lint` produces the same rule violations as TS reference.**

---

### Phase 5 — Distribution (~2-3 weeks)

**Chunks:**

- [ ] **5.1 WASM API surface** (M) — expose every analyzer stage the playground inspects. Minimum public functions: `parse_sexp`, `parse_tree` (serialized CST), `tokens` (lexable token stream over tree-sitter ranges), `lower_hir`, `infer_types`, `diagnostics`, `format`. Each returns serializable JSON so the playground can render trees side-by-side.
- [ ] **5.2 Playground as analyzer testbed** (M) — promote `greycat-analyzer-playground` from a parse-only smoke harness to a full inspection UI: Monaco editor wired to the wasm LSP for completion / hover / diagnostics, plus tabbed views for tokens, CST, HIR, type-inference table, and diagnostics. The playground is the visual debugger for every analyzer phase, not just a demo.
- [ ] **5.3 crates.io publish** (S) — `greycat-analyzer-syntax`, `-core`, `-hir`, `-types`, `-analysis`, `-fmt`, `-ls`, plus the `greycat-analyzer` binary.
- [ ] **5.4 VS Code extension** (S) — wire `editors/code/` to the new LSP binary.
- [ ] **5.5 Salsa retrofit** (M, optional) — only if profiling shows quadratic blow-up on multi-file edits.
- [ ] **5.6 Stdlib parity + version pinning** (S) — sync `lib/std/` from upstream; CI gate.

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
```

Total realistic envelope: **6-9 months full-time**, dominated by Phase 2.

Front-load the snapshot harness (P0.6) — it pays off across the entire project, especially through Phase 2.

---

## 11. How to update this doc

The roadmap moves with the work.

- Check off chunks (`[ ]` → `[x]`) as they land.
- When an Open Question (§4) is answered, fold the answer into the relevant Decision (§3) or Phase chunk and remove the question.
- When a phase finishes, leave the phase in place — keep the milestone, mark all chunks done, link to the commit/PR that delivered M_n.
- Do **not** rewrite history. New constraints get a new chunk, not an edit to an old one.
