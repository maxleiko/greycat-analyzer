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

Loads the project entrypoint (`./project.gcl` by default, or any `.gcl` file / directory you pass in), walks its `@library` / `@include` pragmas to discover reachable modules, and runs the full pipeline on each: tree-sitter parse → CST→HIR lower → name resolution → type analyzer → lint rules. **There is no flat directory walk** — only modules reachable from the entrypoint are analyzed. Prints `path:line:col: severity: message` per finding plus a trailing severity-count summary; exits non-zero on errors or warnings (hints alone don't fail CI — they're advisory).

```sh
greycat-analyzer lint                                          # ./project.gcl, current dir
greycat-analyzer lint path/to/dir                              # dir / project.gcl
greycat-analyzer lint path/to/project.gcl                      # explicit entrypoint
greycat-analyzer lint path/to/standalone.gcl                   # single-file project
greycat-analyzer lint --fix                                    # apply auto-fixable lint suggestions in place
greycat-analyzer lint --list-rules                             # dump registered rule namespace and exit
greycat-analyzer lint --off=unused-local,non-exhaustive        # silence specific rules globally
greycat-analyzer lint --no-suppressions                        # CI: re-emit every `// gcl-lint-off`-silenced diagnostic
greycat-analyzer lint --lint-libs                              # also surface lints from `lib/<name>/` modules
```

Diagnostic sources:
- `greycat-analyzer` / `parse-error` / `missing-token` — tree-sitter recovery emitted these.
- `greycat-analyzer` / `semantic` — return-type mismatch, condition-must-be-bool, unresolved name, etc.
- `lint` / `<rule>` — full rule list available via `lint --list-rules` (auto-generated from the registry, so it's always in sync with what the analyzer emits). Rules that flag "this code does nothing" — `unreachable`, `unused-{local,param,decl,suppression}`, `redundant-{nullable-access,non-null-assertion,coalesce,semicolon}` — carry LSP `DiagnosticTag::UNNECESSARY` so VS Code / Helix / Neovim dim the span.

#### Output formats

`--format` (default: `pretty` on a TTY, `compact` when piped):

| Format    | Per-diagnostic           | Trailing summary | Use case                                 |
|-----------|--------------------------|------------------|------------------------------------------|
| `compact` | `path:line:col: …`       | yes              | parity oracle / grep / scripts           |
| `pretty`  | `miette` snippet + caret | yes              | interactive shell, colored on TTY        |
| `csv`     | —                        | no               | per-file timing rows, pipe into `awk`    |
| `quiet`   | —                        | yes              | CI / pre-commit one-line pulse           |

`csv` and `quiet` are explicit-opt-in; the default never picks them. Exit code is the same across every format — `0` when there are no errors or warnings, `1` otherwise. Hints (return-type inference suggestions, style nudges) are advisory and never flip the exit code red.

#### Dead-code detection

The `unreachable` rule (severity Hint) flags two shapes:

- **Post-divergent siblings** — code that follows a `return` / `throw` / `break` / `continue` (or any block / if / try chain that recursively diverges). Conservative on loops: `while (cond) { return; } var _ = 0;` does NOT flag the post-loop `var _` since we can't prove the loop body executed.
- **Dead `else` arms on exhaustive enum chains** — `if (x == E::A) { … } else if (x == E::B) { … } else { … }` where `A` + `B` exhaust `E`. The trailing `else` is unreachable and gets greyed-out. When every variant arm also diverges (e.g., both return), the post-chain code is flagged too.

Contiguous dead siblings inside one block coalesce into a single diagnostic spanning the whole island. `lint --fix` removes the dead range as one edit; for the dead-`else` shape the fix also swallows the leading `else` keyword so the result parses cleanly. Suppress via `// gcl-lint-next-off unreachable` or the file/range variants below.

#### Silencing rules

Three knobs, increasing in scope:

- **Per-region directives** (source-level, fine-grained). Comment forms with the `gcl-` prefix so they sort together in completion and don't collide with prose. Apply to one AST item, one toggle range, or one whole file — see the table and examples below.
- **`--off=<rule>[,<rule>...]`** (CLI, project-wide). Repeatable / comma-list; same effect as `// gcl-lint-file-off <rule>` in every file at once, without source edits. For CI invocations that need to silence a noisy rule across the whole project. Unknown rule names print a fail-soft stderr warning and the lint continues.
- **`--on=<rule>[,<rule>...]`** (CLI, project-wide). Repeatable / comma-list; flips advisory rules that ship default-off (today: `no-breakpoint`). Same fail-soft validation as `--off`.
- **`--no-suppressions`** (CLI, nuclear). Ignores every `// gcl-lint-off`-style directive and re-emits the underlying diagnostics. For auditing "what's hidden behind suppressions in this codebase."

| Directive                                | Scope                                  |
|------------------------------------------|----------------------------------------|
| `// gcl-lint-off <rule0> <rule1> ...`    | Until matching `gcl-lint-on` (or EOF)  |
| `// gcl-lint-on <rule0> <rule1> ...`     | Closes a prior `gcl-lint-off`          |
| `// gcl-lint-next-off <rule0> ...`       | Next AST item (decl / stmt) only       |
| `// gcl-lint-file-off <rule0> ...`       | Whole file (must be at module head)    |
| `// gcl-fmt-off`                         | Until matching `gcl-fmt-on` (or EOF)   |
| `// gcl-fmt-on`                          | Closes a prior `gcl-fmt-off`           |
| `// gcl-fmt-skip`                        | Next AST node only                     |
| `// gcl-fmt-file-off`                    | Whole file (must be at module head)    |

Worked examples:

```gcl
// gcl-lint-next-off unused-decl
private fn _scratch_pad() { }

// gcl-lint-off possibly-null
fn explore(x: Foo?) { x.bar(); x.baz(); }
// gcl-lint-on possibly-null

// gcl-fmt-skip
fn  weirdly_spaced(  a:int  ){ return  a;  }
```

Wildcards (`*`) aren't supported on purpose — explicit rule names only. Misspelled rule names surface `unknown-suppression-rule`; empty rule lists surface `empty-suppression`; toggles that didn't actually drop anything surface `unused-suppression` (per-rule granularity, so `gcl-lint-next-off A B C` where only A fired flags B and C separately). The LSP autocompletes the directive forms when you type `// gcl-…`, and the rule-name slots autocomplete from the same registry as `--list-rules` / `--off`.

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

Foundational printer — output is guaranteed to re-parse cleanly and is idempotent on simple inputs. Byte-for-byte parity with the TS prettifier is the M5 follow-up milestone. The `gcl-fmt-off` / `gcl-fmt-on` / `gcl-fmt-skip` / `gcl-fmt-file-off` directives above preserve marked regions verbatim through both `fmt` and `lint --fix`.

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
| `textDocument/codeAction`   | ✅ — quickfix per diagnostic via the shared `quickfix` module (same edits as `lint --fix`) |
| `textDocument/completion`   | ✅ — idents in scope, `@library` version completion, `// gcl-…` directive + rule-name completion |
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

### Multi-project workspaces

A single LSP session can host many independent GreyCat projects. Every workspace folder with a `project.gcl` at its root is loaded eagerly on `initialize`; a nested `project.gcl` deeper in the tree is loaded lazily the first time you open a file under it (the server walks parents from that file up to the enclosing workspace folder, picks the nearest `project.gcl`, and spins up an isolated `(SourceManager, ProjectAnalysis, TypeArena)` for it). Projects do not see each other's symbols — that matches the runtime model where each `project.gcl` is its own closure.

Two file-spanning advisory diagnostics may appear, both Information-severity and tagged "unnecessary" (so editors dim the whole file):

- `orphan-module` — a `.gcl` file inside a workspace folder with no `project.gcl` up-tree. Add a `project.gcl` to enable full analysis.
- `multi-project-owner` — a file reachable from two projects' `@include` closures. Almost always a design error; restructure your `@include` paths so only one project includes each file.

The CLI subcommands (`greycat-analyzer lint`, `greycat-analyzer fmt`) are unaffected — they always operate on one explicit entrypoint at a time.

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
