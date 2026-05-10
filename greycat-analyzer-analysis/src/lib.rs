//! Semantic analysis for greycat — resolver, analyzer, narrowing,
//! exhaustiveness, member resolution, lints, and the project-level
//! analyzer driver.
//!
//! Stages, in pipeline order:
//!
//! 1. [`resolver`] — name binding. Maps every ident-use
//!    site to a `Definition` (Decl / Local / Param / Generic /
//!    Project) via lexical scope and the shared [`stdlib::ProjectIndex`].
//! 2. [`stdlib`] — cross-module index. Holds shared `TypeArena` /
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
//!    `line_comment` for `gcl-lint-off` / `gcl-lint-off-next` /
//!    `gcl-lint-off-file` / `gcl-lint-on` / `gcl-fmt-…` directives and
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
//! 8. [`actions`] — `CodeActionCategory` vocabulary the LSP
//!    layer consumes for code-action proposals.

pub mod actions;
pub mod analyzer;
pub mod directives;
pub mod lint;
// P27.1
pub mod parallel;
pub mod project;
pub mod quickfix;
pub mod reachability;
pub mod resolver;
pub mod stdlib;
