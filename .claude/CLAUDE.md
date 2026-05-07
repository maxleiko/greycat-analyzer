# greycat-analyzer

Rust port of the [GreyCat](https://greycat.io) language frontend: static analyzer, LSP server, formatter, linter. Targets `.gcl` source. Distributed as a CLI binary, an LSP server, a WASM build, and library crates.

The reference implementation is the TypeScript monorepo at `/home/leiko/dev/datathings/greycat/lang`. The Rust port matches its frontend; no runtime/VM is in scope.

**Long-arc plan:** [ROADMAP.md](../ROADMAP.md). Phases P0–P5, milestones M1–M5. Read it before non-trivial work — architectural decisions are locked there.

Rust edition 2024. Workspace resolver `"3"`.

## Workspace layout

| Crate | Purpose |
|---|---|
| [greycat-analyzer-syntax](../greycat-analyzer-syntax/) | Tree-sitter wrapper. Owns parsing via [tree-sitter-greycat](https://github.com/maxleiko/tree-sitter-greycat). |
| [greycat-analyzer-core](../greycat-analyzer-core/) | `Document`, `Manager`, `span`, project graph, module resolver. |
| [greycat-analyzer-ls](../greycat-analyzer-ls/) | LSP server (`lsp-server` + `crossbeam-channel`). |
| [greycat-analyzer](../greycat-analyzer/) | CLI binary (`clap` subcommands in [src/cmd/](../greycat-analyzer/src/cmd/)). |
| [greycat-analyzer-wasm](../greycat-analyzer-wasm/) | `cdylib` + `rlib`, `wasm-bindgen` bridge. |
| [greycat-analyzer-playground/](../greycat-analyzer-playground/) | Vite/TS UI consuming the wasm pkg. **Not** a workspace member; gitignored. |

Future crates (per ROADMAP P2.2): `greycat-analyzer-hir`, `-types`, `-analysis`, `-fmt`.

Dependency direction: `syntax → core → (hir → types → analysis) → {ls, cli, wasm, fmt}`.

## Parsing

Always parse via `greycat-analyzer-syntax::parse(source)`. The tree-sitter grammar is at ABI v15, so the host `tree-sitter` crate must be ≥ `0.26`.

Tree-sitter owns scanning; do not add a separate Rust lexer. The retired hand-rolled CST/AST/lexer in `greycat-analyzer-core/src/{cst,ast,lexer}/` is being deleted in P0.4 — do not extend it.

## Common commands

```sh
cargo build --workspace                              # build everything
cargo test  --workspace                              # run tests
cargo install --path greycat-analyzer --debug        # install CLI on host

cargo run -p greycat-analyzer -- lint <args>
cargo run -p greycat-analyzer -- lang-server
cargo run -p greycat-analyzer -- cst <file>
cargo run -p greycat-analyzer -- lex <file>          # retired in P0.4

# wasm
cd greycat-analyzer-wasm && wasm-pack build --target web
./serve.sh                                           # miniserve on 127.0.0.1:8080

# playground (separate npm project)
cd greycat-analyzer-playground && pnpm install && pnpm dev
```

## Conventions

- **Arena lifetimes:** legacy `bumpalo::Bump`-backed types in `core` (e.g. `Document<'arena>`, `Manager<'arena>`) carry an `'arena` lifetime. P0.3 replaces this with tree-sitter `Tree`; new code should not introduce new `'arena` parameters.
- **`lsp_types` is re-exported from `greycat-analyzer-core`** — depend on `greycat_analyzer_core::lsp_types` from downstream crates so versions stay in lockstep.
- **Gitignored at repo root:** `/target`, `/project.gcl`, `/gcdata`, `/files`, `/lib`, `/webroot`, `greycat-analyzer-playground`. The root `project.gcl` is local scratch; do not commit.
- **Examples** for ad-hoc parsing live in [examples/](../examples/). Use these as inputs when smoke-testing parser changes.

## Commit cadence (ROADMAP execution)

While executing [ROADMAP.md](../ROADMAP.md), **one commit per chunk** (the `[ ]` items inside each phase). This keeps the history bisectable and lets the user review the port one workpackage at a time.

Standing authorization: commit per chunk without asking each time.

Per-chunk checklist:

1. `cargo build --workspace` clean.
2. `cargo test --workspace` clean (or, for chunks that intentionally don't add tests, the existing tests still pass).
3. Tick the chunk in ROADMAP.md (`[ ]` → `[x]`) in the same commit.
4. Stage only the files relevant to the chunk; never `git add -A`.
5. Don't skip hooks, don't amend prior commits.

Commit message format — match the existing log style (short, lowercase, area-prefixed):

```
P<phase>.<chunk>: <terse summary>
```

Examples:
- `P0.1: workspace re-shape, add greycat-analyzer-syntax crate`
- `P0.3: port Document/Manager to tree-sitter Tree`
- `P2.4: type system core — Type enum, subtyping, generics`

Body is optional; add one when the *why* isn't obvious from the diff. No `Co-Authored-By` footer — repo history doesn't use them.

If a chunk is too big for one commit, split it in ROADMAP.md first, then commit per sub-chunk. Do not bundle multiple chunks.

Non-chunk work (workflow setup, dependency bumps, doc fixes) uses a `chore:` or area prefix instead of `P<phase>.<chunk>:`.

## GreyCat language

When reading or generating `.gcl`, invoke the `/greycat:greycat` skill — it has the full language reference (nodes, node collections, nullability, type system, annotations, common pitfalls). Don't re-derive language rules from the codebase.

Quick reminders for analyzer work:
- Field access on inner type: `n->name`. Method on the node itself: `n.resolve()`.
- Native types (`geo`, `time`, `duration`) have no fields — methods only.
- `Array<T>{}`, not `Array<T>::new()`. No ternary. No `void` keyword.
- `function` parameter type is opaque; calls return `any?` and require casting.
