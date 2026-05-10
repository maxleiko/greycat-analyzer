# greycat-analyzer

Rust port of the [GreyCat](https://greycat.io) language frontend — a static analyzer, LSP server, formatter, and linter for `.gcl` source.

The CLI binary (`greycat-analyzer`) doubles as the LSP server, so editors only need one executable on `$PATH`.

## Clone

The tree-sitter grammar lives at [tree-sitter-greycat/](tree-sitter-greycat/) as a git submodule at the repo root, and the syntax crate's `build.rs` reads `node-types.json` directly from it — every build needs the submodule populated.

```sh
git clone --recurse-submodules https://github.com/maxleiko/greycat-analyzer.git

# or, if you already cloned without --recurse-submodules:
git submodule update --init --recursive
```

After pulling new commits that bump the submodule pointer, refresh with `git submodule update --init --recursive` (or set `git config submodule.recurse true` once to make `git pull` do it automatically).

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
  fmt     Format a GreyCat project (`--mode=write|check|stdout|diff`)
  server  Start the LSP server. Alias: `lang-server`
  cst     Print the tree-sitter CST s-expression for a `.gcl` file (debug)
```

### `lint` — parse, type-check, lint

Walks every `.gcl` file under the project's directory (skipping `node_modules/`, `gcdata/`, `.git/`) and runs the full pipeline: tree-sitter parse → CST→HIR lower → name resolution → type analyzer → lint rules. Prints `path:line:col: severity: message` per finding and exits non-zero when any are produced.

```sh
greycat-analyzer lint examples/project.gcl
greycat-analyzer lint examples/project.gcl --csv              # per-file timing summary
greycat-analyzer lint --list-rules                            # registered rule namespace
greycat-analyzer lint examples/project.gcl --no-suppressions  # CI: ignore // gcl-lint-off
```

Diagnostic sources:
- `greycat-analyzer` / `parse-error` / `missing-token` — tree-sitter recovery emitted these.
- `greycat-analyzer` / `semantic` — return-type mismatch, condition-must-be-bool, unresolved name, etc.
- `lint` / `<rule>` — full rule list available via `lint --list-rules` (auto-generated from the registry, so it's always in sync with what the analyzer emits). Rules that flag "this code does nothing" — `unreachable`, `unused-{local,param,decl,suppression}`, `redundant-{nullable-access,non-null-assertion,coalesce}` — carry LSP `DiagnosticTag::UNNECESSARY` so VS Code / Helix / Neovim dim the span.

#### Dead-code detection (P24)

The `unreachable` rule (severity Hint) flags two shapes:

- **Post-divergent siblings** — code that follows a `return` / `throw` / `break` / `continue` (or any block / if / try chain that recursively diverges). Conservative on loops: `while (cond) { return; } var _ = 0;` does NOT flag the post-loop `var _` since we can't prove the loop body executed.
- **Dead `else` arms on exhaustive enum chains** — `if (x == E::A) { … } else if (x == E::B) { … } else { … }` where `A` + `B` exhaust `E`. The trailing `else` is unreachable and gets greyed-out. When every variant arm also diverges (e.g., both return), the post-chain code is flagged too.

Contiguous dead siblings inside one block coalesce into a single diagnostic spanning the whole island. `lint --fix` removes the dead range as one edit; for the dead-`else` shape the fix also swallows the leading `else` keyword so the result parses cleanly. Suppress via `// gcl-lint-off-next unreachable` or the file/range variants.

#### Per-region opt-out (P23)

A few comment directives let you silence a specific rule (or skip the formatter) over a region without disabling lints workspace-wide. Every directive carries the `gcl-` prefix so they sort together in completion and don't collide with prose:

| Directive                                | Scope                                  |
|------------------------------------------|----------------------------------------|
| `// gcl-lint-off <rule0> <rule1> ...`    | Until matching `gcl-lint-on` (or EOF)  |
| `// gcl-lint-on <rule0> <rule1> ...`     | Closes a prior `gcl-lint-off`          |
| `// gcl-lint-off-next <rule0> ...`       | Next AST item (decl / stmt) only       |
| `// gcl-lint-off-file <rule0> ...`       | Whole file (must be at module head)    |
| `// gcl-fmt-off`                         | Until matching `gcl-fmt-on` (or EOF)   |
| `// gcl-fmt-on`                          | Closes a prior `gcl-fmt-off`           |
| `// gcl-fmt-skip`                        | Next AST node only                     |
| `// gcl-fmt-off-file`                    | Whole file (must be at module head)    |

Worked examples:

```gcl
// gcl-lint-off-next unused-decl
private fn _scratch_pad() { }

// gcl-lint-off possibly-null
fn explore(x: Foo?) { x.bar(); x.baz(); }
// gcl-lint-on possibly-null

// gcl-fmt-skip
fn  weirdly_spaced(  a:int  ){ return  a;  }
```

Wildcards (`*`) aren't supported on purpose — explicit rule names only. CI gets `--no-suppressions` for the nuclear option ("re-emit every silenced diagnostic"). Misspelled rule names surface `unknown-suppression-rule`; empty rule lists surface `empty-suppression`; toggles that didn't actually drop anything surface `unused-suppression` (per-rule granularity, so `gcl-lint-off-next A B C` where only A fired flags B and C separately). The LSP autocompletes the directive forms when you type `// gcl-…`, and the rule-name slots autocomplete from the same registry as `--list-rules`.

### `fmt` — format a project

Same project-shape as `lint`: optional positional accepting a `.gcl` entrypoint *or* a directory (auto-discovers `project.gcl`), defaulting to the cwd. The `@library` / `@include` closure is what gets formatted — never a flat directory walk.

```sh
greycat-analyzer fmt                                  # cwd / project.gcl, in-place
greycat-analyzer fmt path/to/dir                      # dir / project.gcl, in-place
greycat-analyzer fmt path/to/project.gcl              # explicit entrypoint
greycat-analyzer fmt path/to/standalone.gcl           # one-file project, in-place
greycat-analyzer fmt --mode=check                     # CI: list drifted files, exit non-zero on drift
greycat-analyzer fmt --mode=diff                      # unified diff per file (colored on TTY)
greycat-analyzer fmt --mode=stdout                    # format only the entrypoint, print to stdout
greycat-analyzer fmt --fmt-libs                       # also reformat files under `lib/<name>/` (off by default)
```

Modes are mutually exclusive (`--mode=write|check|stdout|diff`, default `write`). `stdout` is single-file by design — it formats only the entrypoint and ignores the closure. Files with parse errors are skipped + warned (the formatter would otherwise print recovered-but-garbage output) and contribute to a non-zero exit.

Library files (`module.lib != "project"`) are skipped by default; pass `--fmt-libs` to opt in. Mirrors `lint --lint-libs`.

Foundational printer — output is guaranteed to re-parse cleanly and is idempotent on simple inputs. Byte-for-byte parity with the TS prettifier is the M5 follow-up milestone. The `gcl-fmt-off` / `gcl-fmt-on` / `gcl-fmt-skip` / `gcl-fmt-off-file` directives above preserve marked regions verbatim through both `fmt` and `lint --fix`.

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
| [greycat-analyzer-syntax](greycat-analyzer-syntax/) | Tree-sitter wrapper. Owns parsing via [tree-sitter-greycat](tree-sitter-greycat/) (git submodule at the repo root). Generated typed-node accessors. |
| [greycat-analyzer-core](greycat-analyzer-core/) | `Document`, `SourceManager`, project graph, module resolver, parse diagnostics. |
| [greycat-analyzer-hir](greycat-analyzer-hir/) | Arena-backed HIR + CST→HIR lowering. |
| [greycat-analyzer-types](greycat-analyzer-types/) | `Type` enum, interning arena, subtyping, generics. |
| [greycat-analyzer-analysis](greycat-analyzer-analysis/) | Resolver, analyzer, lint rules, stdlib ingestion. |
| [greycat-analyzer-fmt](greycat-analyzer-fmt/) | Tree-sitter-driven formatter. |
| [greycat-analyzer-server](greycat-analyzer-server/) | LSP server + capability handlers. |
| [greycat-analyzer-wasm](greycat-analyzer-wasm/) | WASM bindings exposing every stage to the playground. |
| [greycat-analyzer](greycat-analyzer/) | The `greycat-analyzer` CLI binary. |

## Dev

```sh
cargo build --workspace
cargo test  --workspace
cargo install --path greycat-analyzer --debug
```

Verification loop (see [.claude/CLAUDE.md](.claude/CLAUDE.md)):

```sh
cargo fmt --all
cargo build --workspace
cargo clippy --workspace --all-targets
cargo test  --workspace
```

CI (`.github/workflows/ci.yml`) enforces all of these on push.
