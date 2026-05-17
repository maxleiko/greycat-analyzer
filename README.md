# greycat-analyzer

Static analyzer, language server, formatter, and linter for [GreyCat](https://greycat.io) (`.gcl`).

One binary, `greycat-analyzer`, ships every tool. Editors point at `greycat-analyzer server`; CI points at `greycat-analyzer lint` / `fmt`.

## Install

```sh
cargo install --git https://github.com/maxleiko/greycat-analyzer greycat-analyzer
greycat-analyzer --version
```

Pre-built binaries are attached to each [GitHub release](https://github.com/maxleiko/greycat-analyzer/releases).

## Usage

```sh
greycat-analyzer lint     # parse + type-check + lint a project
greycat-analyzer fmt      # format a project
greycat-analyzer server   # start the LSP server (stdio)
greycat-analyzer cst      # print the CST of a file (debug)
```

All subcommands take a path to a `project.gcl` (or a directory containing one). With no argument they use `./project.gcl`. Modules are discovered through the entrypoint's `@library` / `@include` pragmas — there is no flat directory walk.

### Lint

```sh
greycat-analyzer lint                          # ./project.gcl
greycat-analyzer lint path/to/project.gcl      # explicit entrypoint
greycat-analyzer lint --fix                    # apply auto-fixable suggestions
greycat-analyzer lint --list-rules             # show all rule names
greycat-analyzer lint --off=unused-local       # silence one or more rules
```

Exit code is `0` when there are no errors or warnings, `1` otherwise. Hints are advisory and never fail the build.

Output format auto-detects: pretty (with snippet + caret) on a terminal, compact (`path:line:col: severity: message`) when piped. Override with `--format={pretty,compact,csv,quiet}`.

#### Silencing rules

In source, scoped to a region:

```gcl
// gcl-lint-next-off unused-decl
private fn _scratch() { }

// gcl-lint-off possibly-null
fn explore(x: Foo?) { x.bar(); x.baz(); }
// gcl-lint-on possibly-null
```

| Directive | Scope |
|---|---|
| `// gcl-lint-off <rule>...` / `// gcl-lint-on <rule>...` | Between matching markers |
| `// gcl-lint-next-off <rule>...` | Next declaration or statement |
| `// gcl-lint-file-off <rule>...` | Whole file (must be at module head) |
| `// gcl-fmt-off` / `// gcl-fmt-on` / `// gcl-fmt-skip` / `// gcl-fmt-file-off` | Same shapes for the formatter |

Project-wide, from the CLI: `--off=<rule>,...` silences rules; `--on=<rule>,...` enables default-off rules; `--no-suppressions` ignores every in-source directive (audit mode).

### Format

```sh
greycat-analyzer fmt                          # in-place
greycat-analyzer fmt --mode=check             # CI: exit non-zero on drift
greycat-analyzer fmt --mode=diff              # unified diff per file
greycat-analyzer fmt --mode=stdout            # print entrypoint only
```

Output is guaranteed to re-parse and is idempotent. Library files under `lib/<name>/` are skipped unless you pass `--fmt-libs`.

### Language server

```sh
greycat-analyzer server
```

Stdio LSP. Supported capabilities: diagnostics, hover, completion, signature help, goto definition / declaration / implementation, references, document & workspace symbols, document highlight, rename + prepare-rename, folding ranges, selection ranges, code actions (quickfixes mirror `lint --fix`), inlay hints, semantic tokens, formatting.

A single session can host multiple GreyCat projects: each workspace folder with a `project.gcl` is loaded eagerly; nested `project.gcl`s load lazily when you open a file under them. Projects are isolated closures — they don't see each other's symbols, matching the runtime.

## Editors

- **VS Code** — install [editors/code/](editors/code/) (or grab the `.vsix` from the [releases page](https://github.com/maxleiko/greycat-analyzer/releases)). Activates on `.gcl`.
- **Zed** — install the [GreyCat extension](https://zed.dev/extensions/greycat) from Zed's extension registry. ([GitHub](https://github.com/maxleiko/zed-greycat-extension))
- **Any LSP client** — point it at `greycat-analyzer server`.

## Playground

[playground/](playground/) is a browser-based analyzer testbed (Vite + Lit + Monaco) backed by a WASM build of the analyzer. Each stage (parse, lower, infer, diagnostics, format) gets its own tab.

```sh
cd playground
pnpm install
pnpm wasm    # build the wasm crate via wasm-pack
pnpm dev
```

---

## Development

The tree-sitter grammar and editors live in submodules, so clone with `--recurse-submodules`:

```sh
git clone --recurse-submodules https://github.com/maxleiko/greycat-analyzer.git
```

Or if you already cloned without it:

```sh
git submodule update --init --recursive
```

### Build & test

```sh
cargo build --workspace
cargo test  --workspace
cargo install --path greycat-analyzer --debug
```

Per-commit verification loop:

```sh
cargo fmt --all
cargo build --workspace
cargo clippy --workspace --all-targets
cargo test  --workspace
```

CI enforces all four on every push.

### Workspace layout

| Crate | Purpose |
|---|---|
| [greycat-analyzer-syntax](greycat-analyzer-syntax/) | Tree-sitter wrapper. Parsing via [tree-sitter-greycat](tree-sitter-greycat/) (submodule). |
| [greycat-analyzer-core](greycat-analyzer-core/) | `Document`, `SourceManager`, project graph, type arena, subtyping. |
| [greycat-analyzer-hir](greycat-analyzer-hir/) | Arena-backed HIR + CST→HIR lowering. |
| [greycat-analyzer-analysis](greycat-analyzer-analysis/) | Resolver, type analyzer, lint rules, capability services (`ide/`). |
| [greycat-analyzer-fmt](greycat-analyzer-fmt/) | Formatter. |
| [greycat-analyzer-server](greycat-analyzer-server/) | LSP server. |
| [greycat-analyzer-wasm](greycat-analyzer-wasm/) | WASM bindings for the playground. |
| [greycat-analyzer](greycat-analyzer/) | The CLI binary. |

Dependency direction: `syntax → core → hir → analysis → {fmt, server, cli, wasm}`.

Repo conventions, grammar edit loop, and ROADMAP execution rules live in [.claude/CLAUDE.md](.claude/CLAUDE.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
