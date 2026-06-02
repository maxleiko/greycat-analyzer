# ROADMAP

Open work tracker for `greycat-analyzer`. Completed phases live in the git history; what follows is the still-to-land queue, grouped by phase.

Phase numbers are preserved from the historical sequence so existing commit messages (`P14.6:`, `P36.4:`, etc.) stay interpretable. New work picks the next free number.

---

## Phase 14 — Distribution + playground polish

The original phase landed most of its work; what's left is the publish blocker, a profiling-gated salsa retrofit, and the playground maturation passes that were parked here.

- [ ] **14.1 Publish unblock** (S) — either publish `tree-sitter-greycat` to crates.io (preferred — keeps the submodule SHA as the grammar pin) and bump `greycat-analyzer-syntax` to consume the published version, or vendor `parser.c` + `node-types.json` directly into `greycat-analyzer-syntax/src/grammar/` and drop the path-dep. Either path lets `scripts/publish.sh` actually run end-to-end.

- [ ] **14.6 Salsa retrofit** (M, profiling-driven) — gated on profiling showing quadratic blow-up on multi-file edits in real workspaces. The existing pure-function design across the pipeline keeps the retrofit cheap when the signal arrives.

- [ ] **14.7 Playground UI maturation** (M) — click-to-jump from CST / HIR / diagnostic rows back to a Monaco editor selection; LSP-in-web-worker so completion / hover / diagnostics fire in the Monaco editor itself, not just in side panels; `localStorage` persistence so refreshes don't lose the user's source. Discrete frontend project — the playground exists today and serves as the analyzer testbed; this is the polish pass.

- [ ] **14.8 Playground project loading + exposed-API browser** (M) — two new playground capabilities:
  - **Load a project from disk.** Today the playground only edits a single in-memory buffer. Add a "Load project" entry point that walks a user-selected directory (browser File System Access API where available, falling back to `<input type="file" webkitdirectory>`), recognizes `project.gcl` as the entrypoint, and feeds every reachable `.gcl` (via `SourceManager::load_project` semantics) into the wasm analyzer as a multi-doc `SourceManager`. The Monaco editor switches to a file-tree-aware shell so users can hop between modules; cross-module navigation hits real Locations.
  - **Exposed-API browser panel.** New right-rail tab consuming `ProjectIndex::exposed`. Lists every `@expose("rename")` site grouped by exposure key, with the local name, declaring file, and signature. Clicking an entry jumps the editor to the decl.
  - Both rely on a wasm export that returns the `ProjectIndex.exposed` map shape (URI-relative paths, decl byte ranges) and a wasm entry that takes `Vec<(uri, text, lib)>` so the playground can drive multi-doc analysis without round-tripping each file individually.

---

## Phase 30 — `recv.staticName` / `recv->staticName` advisory lint (~2-3 days)

The analyzer already knows whether a member is static (and `fix(analysis): attrs win over methods; instance access skips static` already filters static methods out of instance-access resolution to match the runtime). That fix is **silent**: when a static method exists and no attr of the same name does, the member simply fails to bind and the user never finds out their `.` should have been a `::`. A new opt-in lint surfaces the misuse.

**Fires when:** `recv.staticName` *and* the receiver type has no instance member (attr or non-static method) of that name. **Stays silent when** an instance member of the same name exists in the receiver's chain — rewriting to `::` would change semantics (the runtime returns a `field` handle, not the static method, when an attr collides).

- [ ] **30.1 Lint rule registration + diagnostic shape** (XS) — add `instance-access-on-static` (working name) to `LINT_RULES` in [`greycat-analyzer-analysis/src/lint.rs`](greycat-analyzer-analysis/src/lint.rs); wire emission through `run_typed_lints` and the per-pass `module.lints.retain` filter; add the rule name to `default_tag_for` (probably no UNNECESSARY tag — this is style guidance, not dead code). Touch all four touchpoints in CLAUDE.md's "Adding / removing a lint rule" table.

- [ ] **30.2 Detection pass** (S) — for each `Expr::Member` / `Expr::Arrow` in HIR, walk the receiver type's supertype chain and decide:
  - is there a *static* member (`static_attrs` ∪ `static_methods`) named `prop`?
  - is there an *instance* member (non-static `methods` entry OR an `attrs` entry) named `prop`?

  Fire **only** when the static answer is yes AND the instance answer is no. Diagnostic span: the property segment's byte range. Message: `` static `X::{prop}` accessed via instance form — use `X::{prop}` instead ``.

- [ ] **30.3 Quickfix (auto-fix path)** (S) — register a per-rule fixer in [`greycat-analyzer-analysis/src/ide/quickfix.rs`](greycat-analyzer-analysis/src/ide/quickfix.rs) that rewrites the byte range `[receiver_start, prop_end]` from `recv.prop` / `recv->prop` to `Type::prop`. The `Type` to substitute is the **declared type** of the receiver — *not* the runtime instance type — because `Sub::staticName` resolves through the supertype chain at runtime when `Sub` doesn't declare its own `staticName`. For receivers whose declared type can't be cheaply recovered as text (computed expressions, casts, anonymous types) the quickfix is unavailable; the diagnostic still fires.

- [ ] **30.4 Suppression + tests** (S) — verify `// gcl-lint-off instance-access-on-static` silences the rule; add tests covering: (a) the no-instance-member case (lint fires, quickfix rewrites to `::`); (b) the attr-collision case (lint stays silent); (c) non-static method present locally (lint stays silent); (d) negative for non-static methods called via `.`; (e) negative for the already-correct `Type::staticName` form; (f) suppression directive silences.

- [ ] **30.5 Run against the stdlib closure** (XS) — confirm zero false positives.

**Out of scope:** rewriting `Type::instanceMember` to `recv.instanceMember` (the opposite direction). That would require the LSP to materialize a receiver, which is a heavier code action — leave it for a separate phase if real demand surfaces.

---

## Phase 34 — Server-side filesystem watcher (~3-5 days, optional)

Stop trusting the editor's `workspace/didChangeWatchedFiles` for filesystem deltas and run our own watcher in the LSP process — the rust-analyzer model. When the editor's file watcher misses or coalesces events (notably `rm -rf <dir>` from a terminal, where VSCode's watcher behaviour varies by platform and version), the analyzer can hold stale URIs and goto-def returns locations for files that no longer exist. A server-side watcher gives us a single, predictable code path regardless of the client.

**Trigger to do this phase:** real-world reports of stale URIs or missed reloads. Until then we live with whatever the editor's watcher gives us. The phase is **opt-in** — it adds a thread, a channel, a dep, and a non-trivial amount of plumbing.

**Behavioural contract:**

- Add a `notify`-driven watcher thread per [`Backend`](greycat-analyzer-server/src/backend.rs). Watched roots: every entry in `workspace_roots`, every `<greycat_home>/lib/std` directory, every loaded `project.root`'s `lib/` subtree. Roots are registered/unregistered as projects load/unload and workspace folders come and go.
- Events ship through a `crossbeam_channel::Sender<WatcherEvent>` into the main `lsp-server` loop alongside `Message::*`. The main loop's `match` gains a third arm that dispatches to a new `Backend::on_fs_event` handler.
- `on_fs_event` is a strict superset of today's `did_change_watched_files`. We keep `did_change_watched_files` wired for editors that prefer to drive their own watcher; both paths funnel into the same `apply_fs_event` helper.
- Debounce / coalesce notify's high-frequency event stream — 50–100 ms window, batched flush.
- Cross-platform: notify abstracts inotify (Linux), FSEvents (macOS), ReadDirectoryChangesW (Windows).
- Fall back to the editor watcher when notify fails to start (e.g. CI sandboxes with inotify disabled). Log an `info!` once, operate as today.

**Chunks:**

- [x] **34.1 Add `notify` dep + per-Backend watcher thread** (S) — spawn in `Backend::initialized` (or lazily on first project load). Channel into the main loop. Plumb `WatcherEvent { uri, kind: Create | Modify | Remove }`.
- [ ] **34.2 Root registration** (S) — `Backend::register_fs_root(path)` / `unregister_fs_root(path)`, called from `load_workspace`, `spawn_lazy_project`, `drop_project`, `did_change_workspace_folders`. Watch only what's loaded.
- [x] **34.3 Event → handler dispatch** (M) — main loop's `select!` between `conn.receiver` and the watcher channel. Debounce. Translate notify events into the existing `did_change_watched_files`-shaped processing.
- [x] **34.4 Editor-watcher coexistence** (S) — keep the existing `register_file_watchers` capability registration so editors that *do* forward events still work; route both paths through a shared `apply_fs_event`. Avoid double-counting identical events.
- [ ] **34.5 Tests** (S) — drop a real file on disk between `did_open` and a subsequent `goto_definition`; assert the LSP picks up the change without an editor-side event. Test the failure-to-start fallback by injecting a watcher mock that always errors.

**Out of scope:**

- Watching arbitrary subtrees the user might care about. Limit watched roots to the loaded project closure and `<greycat_home>/lib/std`.
- A user-facing knob to disable the watcher. The fallback already covers "won't start"; a manual toggle would just complicate the configuration surface.

---

## Phase 36 — Decl-handle identity migration tail

Finish what Phase 35 started: delete `TypeKind::Named` and `TypeKind::Generic` (the SmolStr-keyed variants) and `BUILTIN_RUNTIME_TYPES`, leaving exactly three resolved-type variants in `TypeKind`: `Type(TypeDeclId)`, `GenericInstance { decl, args }`, and `Unresolved { name, byte_range }`. Closes the soundness gap where a user-defined `type node<T>` could still collapse with std-core's `node<T>` in any code path that hasn't been migrated.

**Behavioural contract:**

- Every resolved-type reference in `expr_types` / `def_types` / `attr_types` / `method_returns` / `var_types` is `Type(decl)` or `GenericInstance { decl, args }`. No `Named` / `Generic` shapes remain. `Unresolved` is the *only* SmolStr-keyed type variant.
- Every consumer that today branches on `TypeKind::Named { name }` or `TypeKind::Generic { name, .. }` either reads the decl name via `decl_registry.name(handle)` or dispatches on the handle directly.
- The `arena.named(...)` and old `arena.generic(name, args)` constructors are gone. `arena.alloc_type(handle)` and `arena.alloc_generic_instance(handle, args)` are the only handle-aware constructors.
- `is_node_tag(name)` (string-keyed) is deleted. `WellKnown::is_node_tag(decl)` is the only node-tag check.
- `BUILTIN_RUNTIME_TYPES` is deleted. Std-less projects produce `Unresolved` for names the resolver can't bind (the `missing-std` Phase 33 diagnostic explains why); they do *not* mint synthetic `Named` shapes.
- The `function_ty()` / `type_ty()` / `field_ty()` analyzer helpers drop their legacy `arena.named(...)` fallback. Either `well_known.X_decl` is populated (std loaded → `Type(handle)`) or the helper returns `any`.

**Chunks:**

- [ ] **36.4 Assignability + display in [`types/src/lib.rs`](greycat-analyzer-types/src/lib.rs)** (M) — the `Named ↔ Named` and `Generic ↔ Generic` arms in `is_assignable_to` are deleted; the `Type ↔ Type` and `GenericInstance ↔ GenericInstance` arms (already added in 35.2) become the only paths. Same for `is_castable`'s node-tag-head dispatch and tuple-primitive special-case — both use `is_node_tag(name)` today; replace with a closure passed in OR thread `&WellKnown` through. `generic_or_named_name` (used by display) drops its Named/Generic arms.

- [ ] **36.7 Delete the variants + `BUILTIN_RUNTIME_TYPES` + legacy constructors** (S) — once 36.4 lands and no production caller mints `Named` / `Generic`, the variants can go. Delete `TypeKind::Named`, `TypeKind::Generic`, `arena.named`, the old `arena.generic`, `greycat_analyzer_types::is_node_tag(&str)`, `BUILTIN_RUNTIME_TYPES` (and its seeding loop in `ProjectIndex::new`), the legacy `function_ty()` / `type_ty()` / `field_ty()` fallbacks. The name-keyed `is_subtype_of` on `ProjectIndex` goes too (only the handle-keyed `is_subtype_of_decl` survives).

- [ ] **36.8 Migrate `TypeKind::Enum` to carry `TypeDeclId`** (S) — the last name-keyed identity variant. Enums today carry `name: SmolStr, variants: Vec<SmolStr>`; after this, `decl: TypeDeclId, variants: Vec<SmolStr>` (variants stay — they're part of the value, not the identity). Cross-arena enum identity now lives on the handle.

**Out of scope:**

- Migrating `TypeKind::Primitive` to also key off a decl handle. Primitives don't have user-overridable decls.
- Removing `Unresolved`. It's a deliberate kept variant — the only diagnostic-shaped type kind.

---

## Phase 40 — Project-pragma lint control: cross-file quickfix tail

`@lint_off` / `@lint_on` recognition, the entrypoint-only enforcement rule (`lint-pragma-outside-entrypoint`), validation diagnostics, and LSP auto-apply landed in 40.1-40.5. The tail is the cross-file quickfix that *moves* a non-entrypoint pragma into `project.gcl`, plus CLI flag layering and docs.

- [ ] **40.6 Cross-file quickfix plumbing + `lint-pragma-outside-entrypoint` move-fix** (M) — the analyzer's current quickfix shape (`Vec<TextEdit>`) is single-file by construction. 40.5's diagnostic needs an auto-fix that *moves* the pragma — delete it in the source module *and* insert it into the project's `project.gcl`. Two parts:
  - **Plumbing:** extend the quickfix surface with a workspace-level shape (`WorkspaceTextEdit { uri, byte_range, new_text }` or `WorkspaceEdit { changes: Vec<(Uri, TextEdit)> }`), parallel to (not replacing) the single-file `edit_for_diagnostic` so existing fixers don't churn. LSP boundary maps to LSP `WorkspaceEdit`. CLI `--fix` learns to apply edits across multiple files in one invocation.
  - **Fix for `lint-pragma-outside-entrypoint`:** the first consumer. Delete the offending `mod_pragma` (plus its trailing newline) and insert the same pragma text at the head of `project.gcl` (right after the existing pragma block, before any decl). Reuses the entrypoint URI from `ProjectAnalysis` / `SourceManager`.
  - **Tests:** applying the fix yields a re-parseable project where the pragma now lives in `project.gcl` and the source module no longer contains it; the post-fix analysis surfaces zero `lint-pragma-outside-entrypoint`.

- [ ] **40.7 CLI flag layering** (S) — `--off` / `--on` populate `enabled_rules` / `disabled_rules` *after* the pragma walk, so flags always win against pragmas. Smoke test: `@lint_on("no-breakpoint")` in `project.gcl` + `lint --off=no-breakpoint` → silenced.

- [ ] **40.8 Docs + checklist update** (XS) — `README.md` gets a "Project-wide lint policy" section; CLAUDE.md's lint-rule and grammar-keyword checklists gain a "pragma recognizer" row if pragma support is rule-specific (probably not — the recognizer is rule-agnostic, so no per-rule churn).

---

## Phase 41 — wasm bridge restructure + `@greycat/*` npm packages (~3-5 weeks)

The current `greycat-analyzer-wasm` is shaped for the playground (seven single-file `source-in / JSON-out` functions) and ships no `@greycat/*` npm package. This phase reshapes it into a persistent `Project`-handle API, validates the `#[cfg_attr(feature = "wasm", wasm_bindgen)]` pattern across the `analysis::ide::*` ADTs, and lands three npm packages: `@greycat/analyzer` (wasm + TS), `@greycat/monaco` (TS-only Monaco providers consuming `@greycat/analyzer`), `@greycat/shiki` (TS-only TextMate grammar bundle, no wasm).

**Behavioural contract:**

- One Rust crate, `greycat-analyzer-wasm`. Two wasm-pack build configurations: default features → published `@greycat/analyzer`; `--features playground` → playground-local build that adds the CST / HIR / tokens / types dumper exports. Each app loads exactly one of these wasm bundles.
- LSP capability ADTs live in [`greycat-analyzer-analysis/src/ide/<capability>.rs`](greycat-analyzer-analysis/src/ide/) and are wasm-friendly: `#[cfg_attr(feature = "wasm", wasm_bindgen)]`-gated for the 15-of-22 shapes that flatten cleanly (primitives + C-style enums + `Vec<primitive>`), newtype-wrapped opaque handles in the wasm crate for the `Idx`/`Symbol`-bearing shapes (`RenameTarget`, `ScopeName`, `NameSource`).
- The LSP server's [`capabilities/<file>.rs`](greycat-analyzer-server/src/capabilities/) becomes a thin converter from the analysis ADT to `lsp_types::*`. The analysis crate's ADTs stop being shaped by `lsp_types` — the dependency in `analysis` stays only for `Position` / `Range` re-export pending a later cleanup.
- `Project::new` accepts a pre-built `Map<filename, source>` for the project's `@library` / `@include` closure. Wasm is fetch / storage-agnostic. The JS side of `@greycat/analyzer` handles `fetch https://get.greycat.io/... → unzip → IndexedDB cache by version → pass map to Project::new`.
- `@greycat/analyzer` ships both a main-thread entry (`@greycat/analyzer`) and an opt-in worker entry (`@greycat/analyzer/worker`). Same TS interface; worker version Promise-wraps everything.

**Chunks:**

- [x] **41.1 `wasm` feature flag scaffolding** (S) — add `[features] wasm = ["dep:wasm-bindgen"]` to [`greycat-analyzer-analysis/Cargo.toml`](greycat-analyzer-analysis/Cargo.toml); `greycat-analyzer-wasm` enables it. No ADTs migrated yet — verifies the feature compiles through the dep graph and the existing playground build still works.

- [x] **41.2 `Diagnostic` ADT migration (proof of pattern)** (S) — move `lsp_types::Diagnostic` consumption out of [`capabilities/diagnostics.rs`](greycat-analyzer-server/src/capabilities/diagnostics.rs) into a new `analysis::ide::diagnostics::Diagnostic` ADT, feature-gated with `#[cfg_attr(feature = "wasm", wasm_bindgen)]`. Server file becomes a 5-line converter to `lsp_types::Diagnostic`. Wasm crate re-exports the ADT.

- [x] **41.3 `Project` opaque handle in wasm crate** (M) — new `#[wasm_bindgen] pub struct Project` wrapping `(SourceManager, ProjectAnalysis, TypeArena, ProjectIndex)`. Methods: `new(entrypoint_uri, files: js_sys::Map)`, `open(uri, source)`, `change(uri, source)`, `close(uri)`, `diagnostics(uri) -> Vec<Diagnostic>`. Mirrors [`Backend`](greycat-analyzer-server/src/backend.rs) but with no `lsp-server` channels — JS calls methods directly.

- [x] **41.4 Hover ADT + Project method** (S) — mirror of 41.2 for hover. `Project::hover(uri, line, character) -> Option<Hover>`. Position / Range ADTs land here too (the smallest wasm-friendly shape that all subsequent capabilities consume).

- [x] **41.5 Bulk feature-gate migration for primitive-shaped ADTs** (M) — `FoldingRange`, `DocumentHighlight`, `TextEdit` (formatting result), `SignatureHelp` (+ `SignatureInformation` + `ParameterInformation`), `InlayHint`, `SemanticTokens`. One commit per ADT (six total). `CompletionItem` migration is split out as **41.5b** below because the existing `analysis::ide::completion` is 2798 lines / ~190 `lsp_types` references — order-of-magnitude larger than the other six combined and deserving its own commit.

- [x] **41.5b `CompletionItem` IDE ADT migration** (M) — moved the rich completion ADT shape (`CompletionList`, `CompletionItem`, `CompletionItemKind` enum, `InsertTextFormat`, flattened `Documentation` → markdown string, `CompletionTextEdit::Edit` → plain `TextEdit`, optional `additionalTextEdits`) out of `lsp_types` into `analysis::ide::completion`. Server [`capabilities/completion.rs`](greycat-analyzer-server/src/capabilities/completion.rs) is the thin converter; wasm carries `Project::completion(uri, line, character)`. `LibVersionPayload.range` now uses the IDE `Range` shape.

- [x] **41.6 URI-bearing ADTs with `uri()` getters** (M) — `Location` (in [`analysis::ide::types`](greycat-analyzer-analysis/src/ide/types.rs)), `WorkspaceSymbol`, `DocumentSymbol` (with self-recursive `children`). Pattern: `uri: Uri` field stays in the struct (`#[wasm_bindgen(skip)]`); `#[wasm_bindgen(getter)] fn uri(&self) -> String` exposes it as a JS string. Wasm: `Project::documentSymbols(uri)`, `Project::workspaceSymbols(query)`.

- [x] **41.7 Opaque newtype wrappers for `Idx`-bearing handles** (M) — `RenameTarget` lifted to an opaque `#[wasm_bindgen] pub struct RenameTarget(AnalysisRenameTarget)` in the wasm crate. `Project::resolveRenameTarget(uri, line, character) → Option<RenameTarget>` and `Project::renameTargetSites(handle) → Vec<Location>` validate the round-trip without JS inspecting any `Idx` payload. `Project::references` is the convenience combination. `cursor_ident_idx` migrated into [`analysis::ide::rename`](greycat-analyzer-analysis/src/ide/rename.rs) so the wasm bridge reuses the same helper as the LSP. `ScopeName` / `NameSource` stay private to the completion subsystem — no public bindgen surface yet because no JS-side caller consumes them directly.

- [x] **41.8 Recursive / map-shaped outputs** (S) — `SelectionRange` flattens to a `Vec<Range>` (leaf-to-root) per cursor position; server `capabilities/selection_ranges.rs` re-walks in reverse to rebuild the LSP linked list. `CodeAction`'s `WorkspaceEdit { changes: HashMap<Uri, Vec<TextEdit>> }` flattens to `Vec<UriEdits { uri, edits }>` ([`analysis::ide::code_actions`](greycat-analyzer-analysis/src/ide/code_actions.rs)). Wasm: `Project::selectionRanges`, `Project::codeActions`.

- [x] **41.9 Stdlib bootstrap (`LibraryResolver`)** (M) — JS-side reactive resolver layered above the wasm `Project`. [`RegistryLibraryResolver`](packages/analyzer/src/library-resolver.ts) fetches `@library(name, version)` zips from `https://get.greycat.io/...`, decodes via `fflate.unzipSync`, caches per session (`MemoryLibraryCache`) and persistently (`IndexedDbLibraryCache` in browsers, `NoopLibraryCache` elsewhere). Cache + fetch are injectable. Version is read FROM the project source via the `Project.create` lifecycle, never passed by the caller. Covered by [`library-resolver.test.ts`](packages/analyzer/src/library-resolver.test.ts) (13 tests: URL composition, happy-path resolve, wrapper-dir stripping, concurrent dedup, persistent cache hit, `bypassCache`, HTTP errors, cache round-trip + defensive copy). **When the registry moves to `.json.gz` map the decode path swaps in one place; the contract holds.**

- [x] **41.10 Worker scaffolding in `@greycat/analyzer`** (M) — [`packages/analyzer/src/worker.ts`](packages/analyzer/src/worker.ts) exposes the JS-side `Project` via Comlink (`expose(Project)`). Two entries: bare (`@greycat/analyzer`, main-thread sync surface) and worker (`@greycat/analyzer/worker`, Comlink-wrapped Promise surface). Consumer wires the worker with `new Worker(new URL("@greycat/analyzer/worker", import.meta.url), { type: "module" })`.

- [x] **41.11 `@greycat/analyzer` package publish setup** (S) — [`packages/analyzer/`](packages/analyzer/) (`@greycat/analyzer@0.1.0`) under a pnpm workspace ([`pnpm-workspace.yaml`](pnpm-workspace.yaml)). `package.json` exports map, `vp pack` config via [`vite.config.ts`](packages/analyzer/vite.config.ts), unified wasm-pack build script at [`scripts/build-wasm.sh`](scripts/build-wasm.sh) (switched to `--target bundler`; playground forwards via [`playground/scripts/build-wasm.sh`](playground/scripts/build-wasm.sh)). CI publish workflow [`.github/workflows/npm-publish.yml`](.github/workflows/npm-publish.yml) is scaffolded but gated on the `NPM_TOKEN` secret — without it the workflow builds + packs everything and exits without touching the registry.

- [x] **41.12 `@greycat/monaco` package** (M) — [`packages/monaco/`](packages/monaco/) scaffolded with `registerGreycat(monaco, project)` + two proof-of-concept providers (`completion`, `hover`). The full provider matrix (signature help, inlay hints, code actions, references, rename, document symbols, folding ranges, selection ranges, document highlights, formatting, semantic tokens, diagnostics) lands one-by-one against the established shape now that the two seed providers prove the pattern.

- [x] **41.13 `@greycat/shiki` package** (S) — [`packages/shiki/`](packages/shiki/) ships the TextMate grammar at [`editors/code/grammar/Greycat.tmLanguage.json`](editors/code/grammar/Greycat.tmLanguage.json) (mirrored via [`scripts/sync-grammar.mjs`](packages/shiki/scripts/sync-grammar.mjs)) + a `registerGreycat(highlighter)` helper. No wasm, no `@greycat/analyzer` dep.

- [x] **41.14 Playground migration** (M) — the CST / HIR / tokens / types / diagnostics / format dumpers moved into [`greycat-analyzer-wasm/src/playground.rs`](greycat-analyzer-wasm/src/playground.rs), gated by `#[cfg(feature = "playground")] mod playground;` at the crate root (no per-item `#[cfg]`). [`playground/scripts/build-wasm.sh`](playground/scripts/build-wasm.sh) passes `--features playground`. The default-feature build (used by the future `@greycat/analyzer` npm package) ships only the `Project` handle + IDE ADTs. **Migrating the playground UI from the dumpers to the `Project` handle is left for after the `@greycat/analyzer` package lands** (41.11) — the dumpers are still active under the `playground` feature so nothing breaks today.

**Out of scope:**

- Multi-project routing inside `Project`. One Rust `Project` per JS-side instance; JS can instantiate multiple if it needs to multi-edit, mirroring how the LSP `Backend::projects` lives at a layer above the analyzer core.
- A native (non-wasm) `@greycat/analyzer-node` package via napi-rs. Possible later; not needed for the playground or Monaco web editors.
- Divorcing `analysis` from `lsp_types::Position` / `Range`. The ADT shapes move; the position primitives keep their current re-export from `core` until a later cleanup phase justifies the churn.

---

## How to update this doc

- Tick chunks (`[ ]` → `[x]`) as they land. When every chunk in a phase is done, delete the whole phase section — git log holds the history.
- New phases get a short heading + a 1-2 sentence goal + the chunk list. Don't pad with retrospective context.
- Phase numbers are append-only; pick the next unused integer rather than reusing a retired phase's slot.
