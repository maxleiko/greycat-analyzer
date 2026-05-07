# greycat-analyzer

Rust port of the [GreyCat](https://greycat.io) language frontend — a static analyzer, LSP server, formatter, and linter for `.gcl` source.

The CLI binary (`greycat-analyzer`) doubles as the LSP server, so editors only need one executable on `$PATH`.

## Install

```sh
cargo install --path greycat-analyzer --debug
greycat-analyzer --version    # → greycat-lang 0.1.0
```

The binary identifies itself as `greycat-lang` to match the TS reference CLI.

## CLI

```text
greycat-analyzer <COMMAND>

Commands:
  lint    Lint a project — parse + semantic + lint diagnostics
  fmt     Format a `.gcl` file (`--check` mode exits non-zero on drift)
  server  Start the LSP server. Alias: `lang-server`
  cst     Print the tree-sitter CST s-expression for a `.gcl` file (debug)
```

### `lint` — parse, type-check, lint

Walks every `.gcl` file under the project's directory (skipping `node_modules/`, `gcdata/`, `.git/`) and runs the full pipeline: tree-sitter parse → CST→HIR lower → name resolution → type analyzer → lint rules. Prints `path:line:col: severity: message` per finding and exits non-zero when any are produced.

```sh
greycat-analyzer lint examples/project.gcl
greycat-analyzer lint examples/project.gcl --csv    # per-file timing summary
```

Diagnostic sources:
- `greycat-analyzer` / `parse-error` / `missing-token` — tree-sitter recovery emitted these.
- `greycat-analyzer` / `semantic` — return-type mismatch, condition-must-be-bool, unresolved name, etc.
- `lint` / `<rule>` — currently `unused-local`, `unused-param` (skips `_`-prefixed names and native fns).

### `fmt` — format a single file

```sh
greycat-analyzer fmt path/to/file.gcl            # rewrite in place
greycat-analyzer fmt path/to/file.gcl --check    # exit non-zero on drift
```

Foundational printer — output is guaranteed to re-parse cleanly and is idempotent on simple inputs. Byte-for-byte parity with the TS prettifier is the M5 follow-up milestone.

### `server` — language server

```sh
greycat-analyzer server          # canonical
greycat-analyzer lang-server     # legacy alias, still works
```

Speaks LSP over stdio. Capabilities advertised in `initialize`:

| Capability                  | Status |
|-----------------------------|--------|
| `textDocument/didOpen|Change|Save|Close` (incremental) | ✅ |
| Parse + semantic + lint diagnostics (`publishDiagnostics`) | ✅ |
| `textDocument/hover`        | ✅ — markdown popup with binding kind / inferred type |
| `textDocument/signatureHelp` | ✅ — function signature with active-param highlighting |
| `textDocument/definition`, `implementation` | ✅ |
| `textDocument/references`   | ✅ |
| `textDocument/documentHighlight` | ✅ |
| `textDocument/documentSymbol`, workspace symbols | ✅ |
| `textDocument/rename`, `prepareRename` | ✅ |
| `textDocument/foldingRange` | ✅ |
| `textDocument/selectionRange` | ✅ |
| `textDocument/codeAction`   | ✅ — quickfix-per-diagnostic placeholders |
| `textDocument/inlayHint`    | ✅ — `: <type>` after typeless `var` initializers |
| `textDocument/semanticTokens/full` | ✅ — typed FUNCTION / TYPE / ENUM / VARIABLE / PARAMETER |
| `textDocument/formatting`   | ✅ |
| `workspace/workspaceFolders` | ✅ — recursive `project.gcl` load |

Workspace folders trigger a recursive load of `project.gcl` → `@library` / `@include` resolution → diagnostics published for every reachable file.

### `cst` — debug

```sh
greycat-analyzer cst file.gcl
```

Prints the raw tree-sitter s-expression. Useful when chasing parse anomalies or filing grammar gaps.

## Editors

- **VS Code:** [editors/code/](editors/code/) ships an extension that activates on `.gcl` and calls `greycat-analyzer server` over stdio. After `cargo install`, `Reload Window` in VS Code.
- **Other editors:** any LSP client pointed at `greycat-analyzer server` works.

## Playground

[playground/](playground/) is an interactive analyzer testbed — Vite + TypeScript + Lit + WebAwesome + Monaco. The wasm crate ([`greycat-analyzer-wasm`](greycat-analyzer-wasm/)) exports every analyzer stage (`parse_tree`, `tokens`, `lower_hir`, `infer_types`, `diagnostics`, `format`); each is rendered in its own tab.

```sh
cd playground
pnpm wasm    # builds greycat-analyzer-wasm via wasm-pack with the emcc sysroot
pnpm dev     # opens the playground at http://localhost:5173
```

## Workspace

| Crate | Purpose |
|---|---|
| [greycat-analyzer-syntax](greycat-analyzer-syntax/) | Tree-sitter wrapper. Owns parsing via [tree-sitter-greycat](vendor/tree-sitter-greycat/) (vendored as a git submodule). Generated typed-node accessors. |
| [greycat-analyzer-core](greycat-analyzer-core/) | `Document`, `SourceManager`, project graph, module resolver, parse diagnostics. |
| [greycat-analyzer-hir](greycat-analyzer-hir/) | Arena-backed HIR + CST→HIR lowering. |
| [greycat-analyzer-types](greycat-analyzer-types/) | `Type` enum, interning arena, subtyping, generics. |
| [greycat-analyzer-analysis](greycat-analyzer-analysis/) | Resolver, analyzer, lint rules, stdlib ingestion. |
| [greycat-analyzer-fmt](greycat-analyzer-fmt/) | Tree-sitter-driven formatter. |
| [greycat-analyzer-ls](greycat-analyzer-ls/) | LSP server + capability handlers. |
| [greycat-analyzer-wasm](greycat-analyzer-wasm/) | WASM bindings exposing every stage to the playground. |
| [greycat-analyzer](greycat-analyzer/) | The `greycat-analyzer` CLI binary. |

## Dev

```sh
cargo build --workspace
cargo test  --workspace
cargo install --path greycat-analyzer --debug
```

Per-chunk verification loop (see [.claude/CLAUDE.md](.claude/CLAUDE.md)):

```sh
cargo fmt --all
cargo build --workspace
cargo clippy --workspace --all-targets
cargo test  --workspace
```

CI (`.github/workflows/ci.yml`) enforces all of these on push.

The roadmap is at [ROADMAP.md](ROADMAP.md). Items the original plan deferred are catalogued in [FOLLOWUP.md](FOLLOWUP.md).
