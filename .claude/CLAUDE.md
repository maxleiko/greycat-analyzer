# greycat-analyzer

Static-analysis toolchain for the **GreyCat** language (`.gcl`). Provides a CLI, an LSP server, and a WASM build. Mid-rewrite: the parser was recently rebuilt around an arena-allocated CST.

Rust edition 2024. Workspace resolver `"3"`.

## Workspace layout

```
greycat-analyzer-core/   engine: lexer → CST → AST, doc, manager, span
greycat-analyzer-ls/     LSP server (lsp-server + crossbeam-channel)
greycat-analyzer/        CLI binary (clap subcommands)
greycat-analyzer-wasm/   cdylib + rlib, wasm-bindgen bridge for browsers
greycat-analyzer-playground/   Vite/TS UI consuming the wasm pkg (NOT a workspace member)
```

`core` is the foundation; everything else depends on it. The CLI also wires the LS.

### `greycat-analyzer-core/src/`
- `lexer/` — `tokenizer.rs`, `token.rs`, `test.rs`
- `cst/` — arena-allocated CST: `parser.rs`, `node.rs`, `combi.rs` (`ParserCtx`), `info.rs`, `display.rs`, `node_query.rs`, `visitor/{cst_stats,fn_finder}.rs`. `cursor.rs` is currently disabled.
- `ast/` — `parser.rs`, `pretty.rs` (older layer; check whether to keep when touching it)
- `doc.rs` — `Document` (per-file state)
- `manager.rs` — `Manager<'arena>` keyed by `lsp_types::Uri`, with `apply_changes` for incremental updates
- `span.rs` — source ranges
- `lib.rs` — re-exports `lsp_types` and `bumpalo`; has a stub `parse()` marked TODO ("move to HIR")

Entry point: `cst::parse_file(path, &Bump)` → `SourceModule { source, module: Node }`.

### `greycat-analyzer/src/`
- `main.rs` — clap dispatch
- `cmd/` — one file per subcommand: `lint.rs`, `lang_server.rs`, `cst.rs`, `lex.rs`
- `utils.rs` — `AnyError`

Subcommands: `lint`, `lang-server`, `cst`, `lex`.

### `greycat-analyzer-ls/src/`
- `lib.rs` — re-exports `server`, internal `Result` alias
- `server.rs`, `backend.rs`, `project.rs`
- `#![allow(dead_code)]` is on while the rewrite stabilizes — remove when stable

### `greycat-analyzer-wasm/`
- `src/lib.rs` — wasm-bindgen surface
- `index.html`, `index.js`, `index.css`, `global.d.ts`, `pkg/`
- `serve.sh` — `miniserve` on `127.0.0.1:8080`, no-cache headers
- Has a `cc` build dependency

## Common commands

```sh
# build everything
cargo build

# install CLI in debug mode for system-wide use
cargo install --path greycat-analyzer --debug

# run subcommands directly
cargo run -p greycat-analyzer -- lint <args>
cargo run -p greycat-analyzer -- lang-server
cargo run -p greycat-analyzer -- cst <file>
cargo run -p greycat-analyzer -- lex <file>

# wasm
cd greycat-analyzer-wasm && wasm-pack build --target web
./serve.sh                       # serves index.html + pkg/

# playground (separate, not in workspace)
cd greycat-analyzer-playground && pnpm install && pnpm dev
```

## Conventions

- **Arena lifetimes**: CST nodes live in a `bumpalo::Bump`. Anything holding `Node<'arena>` (e.g. `SourceModule`, `Document`, `Manager`) is parameterized by that lifetime. New code that produces CST data must thread `&'arena Bump` through.
- **`lsp_types` is re-exported from core** — depend on `greycat_analyzer_core::lsp_types` instead of pulling `lsp-types` directly into downstream crates so versions stay in lockstep.
- **`/target`, `/project.gcl`, `/gcdata`, `/files`, `/lib`, `/webroot`** are git-ignored. `project.gcl` at the repo root is local scratch (real GreyCat project for dogfooding) — do not commit.
- **Playground (`greycat-analyzer-playground/`) is git-ignored and intentionally outside the workspace.** Treat it as a separate npm project.

## GreyCat language context

When reading or generating `.gcl` files, GreyCat is a unified language + temporal/graph DB. Notable bits this analyzer must understand:
- Nodes (`node<T>`), node collections (`nodeIndex`, `nodeList`, `nodeTime`, `nodeGeo`)
- Field access on inner type via `->` vs node methods via `.` (e.g. `n->name` vs `n.resolve()`)
- Nullable types (`T?`), optional chaining (`?.`), nullish coalescing (`??`), non-null assertion (`!!`)
- `@expose`, `@volatile`, `@library`, `@include`, `@permission`, `@tag` annotations
- No ternary; no `void`; `Array<T>{}` not `Array<T>::new()`

For deeper questions, invoke `/greycat:greycat` — the skill has the full language reference.
