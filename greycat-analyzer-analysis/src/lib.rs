//! Semantic analysis for greycat — resolver, analyzer, narrowing,
//! exhaustiveness, member resolution, lints, and the project-level
//! analyzer driver.
//!
//! Stages, in pipeline order:
//!
//! 1. [`resolver`] — name binding. Maps every ident-use
//!    site to a `Definition` (Decl / Local / Param / Generic /
//!    Project) via lexical scope and the shared [`index::ProjectIndex`].
//! 2. [`index`] — cross-module index. Holds shared `TypeArena` /
//!    `TypeRegistry` / `NativeRegistry` plus value-level decl names
//!    so cross-module name resolution works.
//! 3. [`analyzer`] — type inference per
//!    expression, mismatch diagnostics, member resolution, null /
//!    `is`-flow narrowing, enum-chain exhaustiveness.
//! 4. [`lint`] — rule-based lints (unused-local,
//!    unused-param, unused-decl, etc.) on top of HIR + `Resolutions`.
//!    Emissions route through [`lint::LintCx::emit`] so per-region
//!    suppressions (`// gcl-lint-off <rule>`) configured via
//!    [`directives::Directives`] silence them at the source.
//! 5. [`directives`] — comment-driven user opt-out. Walks every
//!    `line_comment` for `gcl-lint-off` / `gcl-lint-next-off` /
//!    `gcl-lint-file-off` / `gcl-lint-on` / `gcl-fmt-…` directives and
//!    builds the suppression / fmt-skip tables that downstream stages
//!    consult. Misspellings emit `unknown-suppression-rule`; dead
//!    toggles emit `unused-suppression`.
//! 6. [`reachability`] — pure-HIR divergence analysis +
//!    analyzer-aware exhaustive-chain promotion. The `unreachable`
//!    lint rule consumes [`reachability::stmt_diverges_with_analysis`]
//!    and [`reachability::dead_else_range_for_exhaustive_chain`] to
//!    flag dead islands; the LSP layer translates the lint's
//!    [`lint::DiagTag::Unnecessary`] into `DiagnosticTag::UNNECESSARY`
//!    for editor dimming.
//! 7. [`project`] — `ProjectAnalysis::analyze(&SourceManager)`
//!    runs steps 1-6 over every doc in one pass, sharing the index
//!    and caching per-module `ModuleAnalysis` for LSP / CLI
//!    consumers.
//! 8. [`ide`] — capability-shaped services consumed by editor-style
//!    clients: `ide::actions` (`CodeActionCategory` vocabulary),
//!    `ide::quickfix` (edit synthesis for `lint --fix` and LSP code
//!    actions), `ide::rename` (rename / find-references target
//!    discovery). Decoupled from `lsp_types` so the LSP server and the
//!    CLI both consume them.

pub mod analyzer;
pub mod annotation_validate;
pub mod conv;
pub mod directives;
pub mod display;
pub mod erasure;
pub mod ide;
pub mod lint;
/// Analyzer meta-pragmas: `@lint_off` / `@lint_on` suppression
/// directives. CST-based, runs pre-HIR, extracts the project's lint
/// policy. Distinct from [`pragmas`], which validates *GreyCat
/// language* pragmas (`@permission`, …) against the lowered HIR.
pub mod meta_pragmas;
// P27.1
pub mod parallel;
/// GreyCat language-pragma contract validation (`@permission`, …).
/// HIR-based, runs post-lowering with the full `ProjectIndex`.
pub mod pragmas;
pub mod project;
pub mod reachability;
pub mod resolver;
/// Shared return-type inference helpers consumed by both the
/// `infer-return-type` lint and the lambda body-typing arm. The
/// "lambda is a top-level fn in a scope" mental model demands one
/// implementation for both — see `return_inference::inferred_return_from_body`.
pub mod return_inference;
pub mod index;
// P35.1
pub mod well_known;

pub use display::display_fqn;
