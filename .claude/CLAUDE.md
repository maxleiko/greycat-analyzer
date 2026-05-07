# greycat-analyzer

Rust port of the [GreyCat](https://greycat.io) language frontend: static analyzer, LSP server, formatter, linter. Targets `.gcl` source. Distributed as a CLI binary, an LSP server, a WASM build, and library crates.

The reference implementation is the TypeScript monorepo at `https://hub.datathings.com/greycat/lang`. The Rust port matches its frontend; no runtime/VM is in scope.

**Long-arc plan:** [ROADMAP.md](../ROADMAP.md). Phases P0–P5, milestones M1–M5. Read it before non-trivial work — architectural decisions are locked there.

Rust edition 2024. Workspace resolver `"3"`.

## Workspace layout

| Crate | Purpose |
|---|---|
| [greycat-analyzer-syntax](../greycat-analyzer-syntax/) | Tree-sitter wrapper. Owns parsing via [tree-sitter-greycat](../vendor/tree-sitter-greycat/) (vendored as a git submodule). |
| [greycat-analyzer-core](../greycat-analyzer-core/) | `Document`, `Manager`, `span`, project graph, module resolver. |
| [greycat-analyzer-ls](../greycat-analyzer-ls/) | LSP server (`lsp-server` + `crossbeam-channel`). |
| [greycat-analyzer](../greycat-analyzer/) | CLI binary (`clap` subcommands in [src/cmd/](../greycat-analyzer/src/cmd/)). |
| [greycat-analyzer-wasm](../greycat-analyzer-wasm/) | `cdylib` + `rlib`, `wasm-bindgen` bridge. |
| [playground/](playground/) | Vite/TS UI consuming the wasm pkg. **Not** a workspace member; gitignored. |

Future crates (per ROADMAP P2.2): `greycat-analyzer-hir`, `-types`, `-analysis`, `-fmt`.

Dependency direction: `syntax → core → (hir → types → analysis) → {ls, cli, wasm, fmt}`.

## Parsing

Always parse via `greycat-analyzer-syntax::parse(source)`. The tree-sitter grammar is at ABI v15, so the host `tree-sitter` crate must be ≥ `0.26`.

Tree-sitter owns scanning; do not add a separate Rust lexer.

### Grammar lives in this repo

`tree-sitter-greycat` is a **git submodule** at [vendor/tree-sitter-greycat/](../vendor/tree-sitter-greycat/), pulled in as a Cargo `path` dep. That means: when the parser disagrees with the TS reference, **edit the grammar locally and re-run `cargo test --workspace`** instead of allowlisting the divergence in the analyzer.

Grammar edit loop:

```sh
# 1. Edit vendor/tree-sitter-greycat/grammar.js
# 2. Regenerate parser.c + node-types.json
cd vendor/tree-sitter-greycat && npx tree-sitter generate && cd -
# 3. Re-run gauntlet
cargo test -p greycat-analyzer-syntax --test coverage
```

The syntax crate's `build.rs` reads `node-types.json` directly from the submodule; there is no vendored copy in `greycat-analyzer-syntax/`. The submodule SHA *is* the grammar pin.

When grammar fixes are ready, commit inside the submodule and push to `maxleiko/tree-sitter-greycat`, then bump the submodule pointer here. The submodule's own [CLAUDE.md](../vendor/tree-sitter-greycat/.claude/CLAUDE.md) has the full grammar workflow (scanner.c boundary, query updates, etc.).

**Hard rule:** when the gauntlet flags ERROR/MISSING, when CST shape diverges from TS reference, when typed-node accessors return `None` where the reference produces a value — pause and ask. Default answer is "fix the grammar," not "work around it." `KNOWN_GRAMMAR_GAPS` in `greycat-analyzer-syntax/tests/coverage.rs` is a temporary buffer between *gap discovered* and *grammar fixed*; it should be empty most of the time.

## Common commands

```sh
cargo build --workspace                              # build everything
cargo test  --workspace                              # run tests
cargo install --path greycat-analyzer --debug        # install CLI on host

cargo run -p greycat-analyzer -- lint <args>
cargo run -p greycat-analyzer -- lang-server
cargo run -p greycat-analyzer -- cst <file>

# stdlib coverage (optional — needs greycat installed)
greycat install                                      # populates lib/std/
cargo test -p greycat-analyzer-syntax --test coverage

# wasm
cd greycat-analyzer-wasm && wasm-pack build --target web
./serve.sh                                           # miniserve on 127.0.0.1:8080

# playground (separate npm project)
cd greycat-analyzer-playground && pnpm install && pnpm dev
```

## Conventions

- **`lsp_types` is re-exported from `greycat-analyzer-core`** — depend on `greycat_analyzer_core::lsp_types` from downstream crates so versions stay in lockstep.
- **Gitignored at repo root:** `/target`, `/gcdata`, `/files`, `/lib`, `/webroot`, `greycat-analyzer-playground`. The root [project.gcl](../project.gcl) IS committed — it pins the stdlib version via `@library("std", "...")` and drives `greycat install`.
- **Conformance corpus:** vendored TS reference parser/project fixtures live at [tests/corpus/](../tests/corpus/). Stdlib (`lib/std/*.gcl`) is *not* vendored — populate via `greycat install`. The coverage gauntlet handles both.
- **Examples** for ad-hoc parsing live in [examples/](../examples/). Use these as inputs when smoke-testing parser changes.

## Commit cadence (ROADMAP execution)

While executing [ROADMAP.md](../ROADMAP.md), **one commit per chunk** (the `[ ]` items inside each phase). This keeps the history bisectable and lets the user review the port one workpackage at a time.

Standing authorization: commit and **push** per chunk without asking each time. There's no reason to keep a chunk commit local once it's committed; CI / collaborators see it sooner.

Per-chunk checklist:

1. `cargo fmt --all` (no `--check` — just run it). CI runs `cargo fmt --all --check` and a single drifted file fails the build.
2. `cargo build --workspace` clean.
3. `cargo clippy --workspace --all-targets` clean (zero warnings, not just zero errors).
4. `cargo test --workspace` clean (or, for chunks that intentionally don't add tests, the existing tests still pass).
5. Tick the chunk in ROADMAP.md (`[ ]` → `[x]`) in the same commit.
6. Stage only the files relevant to the chunk; never `git add -A`.
7. Don't skip hooks, don't amend prior commits.
8. `git push origin main` after the commit lands. Same applies to `chore:` / area-prefixed non-chunk commits in this repo.

Submodule commits (inside `vendor/tree-sitter-greycat/` → `maxleiko/tree-sitter-greycat`) are also pushed immediately. After pushing, bump the parent's submodule pointer in a follow-up commit so the new SHA propagates downstream.

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
