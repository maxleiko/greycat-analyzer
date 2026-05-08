# Porting from the TypeScript reference

This guide maps every TS module under `packages/lang/src/` (the
[upstream reference implementation](https://hub.datathings.com/greycat/lang))
to its target Rust crate. Use it as the lookup table when triaging
"where does this TS file live now?" questions.

## Overview

| TS subsystem | Path in `packages/lang/src/` | Target Rust crate |
|---|---|---|
| Lexer / tokenizer | `lexer/` | `tree-sitter-greycat` (external submodule) |
| Parser (AST + CST) | `parser/` | [`greycat-analyzer-syntax`](../greycat-analyzer-syntax/) |
| Type system | `analysis/types.ts` | [`greycat-analyzer-types`](../greycat-analyzer-types/) |
| Resolver (name binding) | `analysis/resolver.ts` | [`greycat-analyzer-analysis::resolver`](../greycat-analyzer-analysis/src/resolver.rs) |
| Analyzer (inference, narrowing, exhaustiveness) | `analysis/analyzer.ts` | [`greycat-analyzer-analysis::analyzer`](../greycat-analyzer-analysis/src/analyzer.rs) |
| Environments / scopes | `analysis/environment.ts` | folded into `analysis/resolver.rs` (Cx::scopes stack) |
| Visitors | `visitor/` | tree-sitter queries + HIR walks (no general visitor framework) |
| Pretty printer / formatter | `pp/` + `parser/cst/cst_format.ts` | [`greycat-analyzer-fmt`](../greycat-analyzer-fmt/) |
| Project manager (multi-module, dep graph) | `project/` | [`greycat-analyzer-core::SourceManager`](../greycat-analyzer-core/src/manager.rs) + [`greycat-analyzer-analysis::project::ProjectAnalysis`](../greycat-analyzer-analysis/src/project.rs) |
| LSP capability handlers | `lsp/` | [`greycat-analyzer-server::capabilities`](../greycat-analyzer-server/src/capabilities.rs) |
| LSP server transport | `packages/server/` | [`greycat-analyzer-server::server`](../greycat-analyzer-server/src/server.rs) |
| CLI driver | `packages/cli/` | [`greycat-analyzer`](../greycat-analyzer/) |
| Linter | `packages/cli/src/lint/` | [`greycat-analyzer-analysis::lint`](../greycat-analyzer-analysis/src/lint.rs) |
| Module resolver (`@library`, `@include`) | `packages/resolver/` | [`greycat-analyzer-core::resolver`](../greycat-analyzer-core/src/resolver.rs) |
| Error infrastructure | `errors.ts` | folded into `analysis::analyzer::SemanticDiagnostic` + `lint::LintDiagnostic` |
| Highlighter (semantic tokens) | `highlighter.ts` | [`greycat-analyzer-server::capabilities::semantic_tokens`](../greycat-analyzer-server/src/capabilities.rs) |
| Code-action vocabulary | `analysis/actions.ts` | [`greycat-analyzer-analysis::actions`](../greycat-analyzer-analysis/src/actions.rs) |
| Hinter (inlay hints) | `analysis/hinter.ts` | [`greycat-analyzer-server::capabilities::inlay_hints`](../greycat-analyzer-server/src/capabilities.rs) |
| Declarator | `analysis/declarator.ts` | folded into `analysis::analyzer::register_module_types` + `stdlib::ProjectIndex::ingest` |
| Stdlib (in GreyCat itself) | `lib/std/*.gcl` | vendored corpus (not ported — analyzed as ordinary modules) |

## Conventions that diverged

- **Hand-rolled lexer is gone.** Tree-sitter (`tree-sitter-greycat`) owns
  scanning. There is no `lexer/` equivalent in the Rust port.
- **No general AST visitor framework.** The TS `visitor/` directory
  encodes 8 visitor patterns; the Rust port walks the tree-sitter CST
  via [`greycat-analyzer-syntax::cst`](../greycat-analyzer-syntax/src/cst.rs)
  helpers (pre-order traversal with a continue/skip return) plus typed
  HIR walks.
- **`ExposedMap` cross-module exposure tracking** isn't yet a separate
  table. Annotation names are captured per-decl in
  `Modifiers::annotations` (P6.7), and the linter consumes them
  directly; the project-graph table arrives once cross-module decl
  pointers are populated.
- **Analyzer is a single typed pass.** The TS reference splits
  declarator → resolver → analyzer; the Rust port collapses declarator
  into `register_module_types` + `ProjectIndex::ingest` and threads the
  resolver / analyzer / lints through `ProjectAnalysis::analyze` in one
  pipeline.

## Where the chunks landed

The roadmap (`ROADMAP.md`) tracks every chunk by phase. Key entry
points:

- P0: `tree-sitter-greycat`, `greycat-analyzer-syntax`
- P1: `greycat-analyzer-core::SourceManager`,
  `greycat-analyzer-core::diagnostics`
- P2: `greycat-analyzer-hir`, `greycat-analyzer-types`,
  `greycat-analyzer-analysis::{resolver,analyzer,stdlib}`
- P3: `greycat-analyzer-server::capabilities` (every handler)
- P4: `greycat-analyzer-fmt`,
  `greycat-analyzer-analysis::lint`
- P5: `greycat-analyzer-wasm`, `playground/`
- P6: `greycat-analyzer-analysis::project` + per-feature deepening
- P7: `greycat-analyzer-types` (subtyping rules) + grammar / HIR
  drains
- P8: `greycat-analyzer-server` capability deepening + tests
- P9: `greycat-analyzer-fmt` byte-parity port
- P10: distribution + quality gates
