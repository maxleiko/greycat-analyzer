//! Semantic analysis for greycat — resolver, analyzer, narrowing,
//! exhaustiveness, member resolution, lints, and the project-level
//! analyzer driver.
//!
//! Stages, in pipeline order:
//!
//! 1. [`resolver`] (P2.3 → P6.2) — name binding. Maps every ident-use
//!    site to a `Definition` (Decl / Local / Param / Generic /
//!    Project) via lexical scope and the shared [`stdlib::ProjectIndex`].
//! 2. [`stdlib`] — cross-module index. Holds shared `TypeArena` /
//!    `TypeRegistry` / `NativeRegistry` plus value-level decl names
//!    so cross-module name resolution works.
//! 3. [`analyzer`] (P2.5 → P6.3-P6.6) — type inference per
//!    expression, mismatch diagnostics, member resolution, null /
//!    `is`-flow narrowing, enum-chain exhaustiveness.
//! 4. [`lint`] (P4.2 → P6.7) — rule-based lints (unused-local,
//!    unused-param, unused-decl) on top of HIR + `Resolutions`.
//! 5. [`project`] (P6.1) — `ProjectAnalysis::analyze(&SourceManager)`
//!    runs steps 1-4 over every doc in one pass, sharing the index
//!    and caching per-module `ModuleAnalysis` for LSP / CLI
//!    consumers.
//! 6. [`actions`] (P6.8) — `CodeActionCategory` vocabulary the LSP
//!    layer consumes for code-action proposals.

pub mod actions;
pub mod analyzer;
pub mod lint;
pub mod project;
pub mod quickfix;
pub mod resolver;
pub mod stdlib;
