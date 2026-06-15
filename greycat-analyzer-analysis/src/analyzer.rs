//! Foundational type analyzer.
//!
//! Walks an HIR module after [`crate::resolver::resolve`] has produced a
//! `Resolutions` table, infers a [`TypeId`] for every expression, and
//! produces a list of [`SemanticDiagnostic`]s along the way. Surfaces are:
//!
//! - Inference for literals, binary / unary expressions, calls, members
//!   (head-of-chain), and identifier uses (drawing from resolver).
//! - Mismatch diagnostics for assignment, return statements, and
//!   `if`/`while`/`do-while` conditions (must be `bool`-assignable).
//! - Use of unresolved names (carried over from resolver).
//!
//! Out of scope for the foundational pass — these arrive as the corpus
//! and future chunks demand them:
//! - Full control-flow narrowing (e.g. `if x != null { /* x is non-null */ }`).
//! - Exhaustiveness checking for enums / unions.
//! - Unused-decl warnings beyond resolver's "unresolved-name" axis.
//! - Type-method body checking against attribute types.
//!
//! The design follows TS `analysis/analyzer.ts`: a single recursive
//! visitor over HIR with an `Inference` table mutated as it goes.

use std::borrow::Cow;
use std::ops::Range;

use greycat_analyzer_core::lsp_types::Uri;
use rustc_hash::{FxHashMap, FxHashSet};

use greycat_analyzer_core::{
    GenericOwner, InferenceTable, ItemKey, Symbol, SymbolTable, Type, TypeArena, TypeId, TypeKind,
    TypeRegistry,
};
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::hir::{
    AssignStmt, AtStmt, BinOp, BinaryExpr, BlockStmt, CallExpr, Decl, DoWhileStmt, Expr, FnDecl,
    ForInStmt, ForStmt, Ident, IfStmt, LambdaExpr, LiteralExpr, LiteralKind, LocalVar, MemberExpr,
    ModVarDecl, ObjectExpr, ObjectField, OffsetExpr, ParseIssue, PositionalObjectExpr, Pragma,
    StaticExpr, Stmt, StringExpr, TryStmt, TypeAttr, TypeDecl, TypeRef, UnaryExpr, UnaryOp,
    WhileStmt,
};
use greycat_analyzer_hir::{DeclRegistry, Hir};

use crate::index::{FnSignature, Namespace, ProjectIndex};
use crate::lint::{LintDiagnostic, LintSeverity};
use crate::lower_type_ref::{self, TypeRefLowering};
use crate::reachability::block_breaks_current_loop;
use crate::resolver::{Definition, Resolutions};

/// Recover a *field-name* symbol from a named object field's key
/// expression. The grammar lowers `object_field.name` as a full
/// `_expr`, so a classic field name arrives as `Expr::Ident` (or a
/// quoted `Expr::String` for names that aren't valid bare idents, e.g.
/// `Foo { "hello world": 1 }`). Returns the interned symbol plus the
/// key's `Idx<Ident>` when it's a bare ident (so IDE binding can point
/// at it); `None` for any other key shape — which is only legal as a
/// `Map` key (an arbitrary value expression), never as a classic
/// field name.
pub(crate) fn object_field_key_name(
    hir: &Hir,
    symbols: &SymbolTable,
    key: Idx<Expr>,
) -> Option<(Symbol, Option<Idx<Ident>>)> {
    match &hir.exprs[key] {
        Expr::Ident { name, .. } => Some((hir.idents[*name].symbol, Some(*name))),
        // A quoted field name never interpolates; skip template
        // strings (they can't name an attr).
        Expr::String(s) if !s.has_interpolation() => Some((symbols.intern(&s.raw_value()), None)),
        _ => None,
    }
}

/// Does this statement always exit the enclosing control
/// flow (`return`, `throw`, `break`, `continue`)? `Block` recurses
/// into its last statement. `If` requires *both* branches to
/// terminate (no else → not terminal). Used by the analyzer to lift
/// the else-branch's narrowing into the post-if scope when the
/// then-branch always exits early — handles the `if (x == null)
/// { return; } use(x);` idiom.
fn stmt_terminates(hir: &Hir, stmt_id: Idx<Stmt>) -> bool {
    match &hir.stmts[stmt_id] {
        Stmt::Return(_) | Stmt::Throw(_) | Stmt::Break(_) | Stmt::Continue(_) => true,
        Stmt::Block(b) => block_terminates(hir, b),
        Stmt::If(IfStmt {
            then_branch,
            else_branch,
            ..
        }) => {
            block_terminates(hir, then_branch)
                && else_branch.is_some_and(|e| stmt_terminates(hir, e))
        }
        _ => false,
    }
}

/// `true` iff every reachable path through `block` always exits the
/// surrounding flow (return / throw / break / continue). Mirrors
/// [`stmt_terminates`]'s `Block` arm but takes a borrowed
/// [`BlockStmt`] — body-bearing fields hold the block inline now,
/// so going through `Idx<Stmt>` would require an extra arena round
/// trip just to re-pattern-match.
fn block_terminates(hir: &Hir, block: &BlockStmt) -> bool {
    block.stmts.last().is_some_and(|s| stmt_terminates(hir, *s))
}

/// Severity sketch for analyzer diagnostics. Maps onto `lsp_types::DiagnosticSeverity`
/// at the LSP boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

/// Where in the pipeline a diagnostic was produced. Lets the
/// `ProjectAnalysis` driver assert the architectural invariant
/// described on `validate_type_relations`: nothing earlier in the
/// pipeline may emit type-relation diagnostics — those see
/// un-settled `any`s for cross-module Calls and surface false
/// positives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagCategory {
    /// Resolver-time / structural failures (unresolved name,
    /// unsupported syntax, member-resolution dead-end). These can
    /// fire from anywhere — they don't depend on settled types.
    Structural,
    /// Type-relation comparison ("must be `T`, got `U`",
    /// "not assignable to"). MUST only be emitted by
    /// [`crate::project::ProjectAnalysis::validate_type_relations`]
    /// — every other pass would compare against pre-fixup `expr_types`
    /// and surface false positives for cross-module calls.
    TypeRelation,
}

#[derive(Debug, Clone)]
pub struct SemanticDiagnostic {
    pub severity: Severity,
    /// Stable kebab-case rule / check identifier used by the editor's
    /// `[code]` rendering, the cli's compact `severity[code]:` line,
    /// and the LSP `Diagnostic.code` field. Every emission site picks
    /// a fixed `&'static str` (not derived from `message`) so tooling
    /// can stably reference a check.
    pub code: &'static str,
    pub message: String,
    pub byte_range: Range<usize>,
    pub category: DiagCategory,
}

impl SemanticDiagnostic {
    /// Default-constructor for callers in the analyzer / resolver
    /// that emit non-type-relation diagnostics. Type-relation
    /// callers (only the project pipeline's validation pass) must
    /// build the struct literally so the category is explicit.
    pub fn structural(
        severity: Severity,
        code: &'static str,
        message: String,
        byte_range: Range<usize>,
    ) -> Self {
        Self {
            severity,
            code,
            message,
            byte_range,
            category: DiagCategory::Structural,
        }
    }
}

/// A non-exhaustive enum-eq if-chain detected in pass 2. Recorded into
/// [`AnalysisResult`] rather than emitted directly so the lint pipeline
/// can surface it as a real, suppressible `non-exhaustive` rule via
/// [`crate::lint::lint_non_exhaustive_with_directives`]. Mirrors the
/// existing record-then-emit pattern used by `exhaustive_enum_chains`.
#[derive(Debug, Clone)]
pub struct NonExhaustiveFinding {
    /// Head `if_stmt` HIR id of the chain. The lint key plus quickfix
    /// dispatch only need `byte_range`, but the id is kept so any
    /// future consumer can correlate against `exhaustive_enum_chains`.
    pub head_id: Idx<Stmt>,
    /// Enum that the chain dispatched on (e.g. `"Example"`).
    pub enum_name: Symbol,
    /// Variants the chain failed to cover, in declaration order.
    pub missing: Vec<Symbol>,
    /// Byte range of the head `if`, used as the diagnostic's range.
    pub byte_range: Range<usize>,
}

/// Output of semantic analysis for a single module.
///
/// The backing [`TypeArena`] is owned by
/// [`crate::project::ProjectAnalysis`].
#[derive(Debug, Default)]
pub struct AnalysisResult {
    pub registry: TypeRegistry,
    /// Inferred type for analyzed expressions.
    pub expr_types: FxHashMap<Idx<Expr>, TypeId>,
    /// Inferred type for defining identifiers.
    pub def_types: FxHashMap<Idx<Ident>, TypeId>,
    /// Maps declared type names to their defining declaration.
    pub type_decls: FxHashMap<Symbol, Idx<Decl>>,
    /// Resolved member references within the current module.
    pub member_uses: FxHashMap<Idx<Ident>, MemberDef>,
    /// Resolved member references to declarations in other modules.
    pub foreign_member_uses: FxHashMap<Idx<Ident>, ForeignMember>,
    /// Resolved segments of qualified-static references such as
    /// `runtime::Identity::create`.
    pub foreign_decl_uses: FxHashMap<Idx<Ident>, ForeignDecl>,
    /// Resolved field names in object construction expressions such as
    /// `Foo { name: value }`.
    pub object_field_uses: FxHashMap<Idx<Ident>, ObjectFieldBinding>,
    pub diagnostics: Vec<SemanticDiagnostic>,
    /// Head statements of enum-equality chains proven exhaustive.
    pub exhaustive_enum_chains: FxHashSet<Idx<Stmt>>,
    /// Non-exhaustive enum-equality chain findings.
    pub non_exhaustive_findings: Vec<NonExhaustiveFinding>,
    /// Lint diagnostics emitted directly by semantic analysis.
    pub surfaced_lints: Vec<LintDiagnostic>,
    /// Statements whose condition can be reduced to a constant boolean.
    pub decidable_conditions: FxHashMap<Idx<Stmt>, bool>,
    /// Runtime-erased type for expressions whose analyzed type is more
    /// specific than the type produced at runtime.
    ///
    /// For example, a generic call may infer `Array<User>` while the
    /// runtime only produces `Array<any>`.
    pub expr_runtime_types: FxHashMap<Idx<Expr>, TypeId>,
    /// Runtime-erased types propagated onto bindings so later uses
    /// inherit the same runtime view.
    pub def_runtime_types: FxHashMap<Idx<Ident>, TypeId>,
}

/// Where a member-access property name resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemberDef {
    /// An attribute declared inside a `type X { ... }` body.
    Attr(Idx<TypeAttr>),
    /// A method declared inside a `type X { ... }` body. The decl is
    /// always a `Decl::Fn` — capabilities consume it via the existing
    /// decl path.
    Method(Idx<Decl>),
}

/// A member-access binding that resolves into another module.
/// `uri` names the home module of the foreign type's declaration; the
/// `member` indices reference that module's HIR arenas, not the
/// analyzed module's.
#[derive(Debug, Clone)]
pub struct ForeignMember {
    pub uri: Uri,
    pub member: MemberDef,
}

/// A top-level decl reference resolved into another module.
/// Used for chain-segment bindings (`runtime::Identity::create`
/// chain[1] points at runtime.gcl's `type Identity` decl).
#[derive(Debug, Clone)]
pub struct ForeignDecl {
    pub uri: Uri,
    pub decl: Idx<Decl>,
}

/// Object-expression field-name binding. `declaring_type` names the
/// type that actually declares the attr — which may be a supertype
/// of the constructed type, since attrs can be inherited across
/// module boundaries. The `ItemKey` carries both the home module
/// symbol (mapped to a `Uri` via `ProjectIndex::module_names`) and
/// the type's leaf name, so IDE consumers can render `module::Type`
/// provenance without re-walking the supertype graph. `attr` is the
/// `TypeAttr` index into the home module's HIR.
#[derive(Debug, Clone, Copy)]
pub struct ObjectFieldBinding {
    pub declaring_type: ItemKey,
    pub attr: Idx<TypeAttr>,
}

impl AnalysisResult {
    pub fn type_of(&self, expr: Idx<Expr>) -> Option<TypeId> {
        self.expr_types.get(&expr).copied()
    }

    /// Look up a member-access ident's binding. Returns the
    /// declaring `TypeAttr` or method `Decl` if member resolution
    /// succeeded for this ident.
    pub fn member_lookup(&self, ident: Idx<Ident>) -> Option<MemberDef> {
        self.member_uses.get(&ident).copied()
    }

    /// Look up a cross-module member-access binding for `ident`.
    /// Falls back to `None` for members that are intra-module
    /// ([`Self::member_lookup`]) or unresolved.
    pub fn foreign_member_lookup(&self, ident: Idx<Ident>) -> Option<&ForeignMember> {
        self.foreign_member_uses.get(&ident)
    }

    /// Look up a chain-segment binding (e.g. `Identity` in
    /// `runtime::Identity::create` -> the foreign type decl).
    pub fn foreign_decl_lookup(&self, ident: Idx<Ident>) -> Option<&ForeignDecl> {
        self.foreign_decl_uses.get(&ident)
    }

    /// Look up an object-expression field-name binding. Returns the
    /// `(home module, TypeAttr)` pair declaring the attribute the
    /// field initialises. `None` for unknown / positional fields.
    pub fn object_field_lookup(&self, ident: Idx<Ident>) -> Option<&ObjectFieldBinding> {
        self.object_field_uses.get(&ident)
    }
}

/// Run the analyzer with a shared project index *and* a caller-owned
/// arena. The arena is shared across every module the project
/// pipeline analyzes so cross-module `TypeId`s point into the same
/// storage — no `mint_type_shape` / `read_type_shape` translation
/// needed at the boundary.
///
/// The index is read-only — it's only consulted when `lower_type_ref`
/// doesn't find a name in the per-module registry, so cross-module
/// type references (`p: Point` where `Point` is declared in another
/// module) lower to the right `Named` shape and `resolve_member` can
/// defer `(property, type_name)` for the project's cross-module
/// member post-pass.
pub fn analyze_with_index_into(
    hir: &Hir,
    res: &Resolutions,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    module_uri: &Uri,
    arena: &mut TypeArena,
) -> AnalysisResult {
    let mut out = AnalysisResult::default();
    register_module_types(hir, arena, &mut out, index, decl_registry, module_uri);

    let Some(module) = hir.module.as_ref() else {
        return out;
    };
    let module_sym = index
        .symbols
        .intern(crate::index::module_name_from_uri(module_uri).unwrap_or("module"));
    let mut cx = Cx {
        hir,
        res,
        out: &mut out,
        module_uri,
        module_sym,
        arena,
        index,
        decl_registry,
        narrows: Vec::new(),
        member_narrows: Vec::new(),
        member_typed_narrows: Vec::new(),
        enum_value_narrows: Vec::new(),
        chain_member_ifs: FxHashSet::default(),
        generics_in_scope: Vec::new(),
        this_stack: Vec::new(),
        inside_static_fn: false,
        static_generic_uses: FxHashSet::default(),
    };
    for d in &module.decls {
        cx.visit_decl(*d);
    }

    // Type-level generics used inside `static` methods — `static fn
    // make(): T`, `T {}`. Recorded during the walk (deduped by
    // `Idx<TypeRef>`); emitted now that `cx`'s `&mut out` borrow is
    // released. One error per source reference.
    let static_generic_uses = std::mem::take(&mut cx.static_generic_uses);
    for tr_idx in static_generic_uses {
        let tr = &hir.type_refs[tr_idx];
        let pname = &index.symbols[hir.idents[tr.name].symbol];
        out.diagnostics.push(SemanticDiagnostic::structural(
            Severity::Error,
            "generic-in-static-context",
            format!("generic type parameter `{pname}` cannot be used in a `static` method"),
            tr.byte_range.clone(),
        ));
    }

    // Surface resolver's unresolved-name list as analyzer diagnostics so
    // P2.7 (LSP publish) only needs one list per file.
    // P38.4 — idents flagged `ambiguous` or `private_cross_module` get
    // a richer diagnostic (with candidate modules + FQN quick-fixes);
    // skip them here to avoid a duplicate generic "unresolved name"
    // alongside the helpful one.
    for ident_idx in &res.unresolved {
        if res.ambiguous.contains_key(ident_idx) {
            continue;
        }
        if res.private_cross_module.contains_key(ident_idx) {
            continue;
        }
        let ident = &hir.idents[*ident_idx];
        // An empty symbol only comes from a MISSING token, which already
        // carries its own `missing-token` parse diagnostic. `unresolved
        // name `` ` is noise (e.g. the anonymous-object head, flagged
        // separately as `anonymous-object`).
        if index.symbols[ident.symbol].is_empty() {
            continue;
        }
        out.diagnostics.push(SemanticDiagnostic::structural(
            Severity::Error,
            "unresolved-name",
            format!("unresolved name `{}`", &index.symbols[ident.symbol]),
            ident.byte_range.clone(),
        ));
    }

    // Lambda captures — runtime rejects refs to locals/params from
    // enclosing scope with `unresolved identifier`, and segfaults on
    // `this`. Both surface here as `lambda-capture`.
    for ident_idx in &res.captured {
        let ident = &hir.idents[*ident_idx];
        out.diagnostics.push(SemanticDiagnostic::structural(
            Severity::Error,
            "lambda-capture",
            format!(
                "lambda cannot capture `{}` from enclosing scope",
                &index.symbols[ident.symbol]
            ),
            ident.byte_range.clone(),
        ));
    }
    for span in &res.this_in_lambda {
        out.diagnostics.push(SemanticDiagnostic::structural(
            Severity::Error,
            "lambda-capture",
            "lambda cannot capture `this` from enclosing type method".to_string(),
            span.clone(),
        ));
    }

    // Same-scope value-binding collisions — the runtime rejects `var x`
    // after a param `x` or earlier `var x` in the same block with
    // `already declared var` / `already declared param`. Nested-scope
    // shadowing is allowed and not recorded here.
    for ident_idx in &res.rebound {
        let ident = &hir.idents[*ident_idx];
        out.diagnostics.push(SemanticDiagnostic::structural(
            Severity::Error,
            "local-rebind",
            format!(
                "name `{}` is already declared in this scope",
                &index.symbols[ident.symbol]
            ),
            ident.byte_range.clone(),
        ));
    }

    // P38.4 — `ambiguous-symbol` Severity::Error: the bare name is
    // exported publicly by ≥2 distinct modules, with no local hit to
    // resolve it. Matches the GreyCat runtime's "unresolved function"
    // exit-2 outcome on the same shape, but names the candidates so
    // the user can pick an FQN. Quick-fix emission lives in
    // [`crate::quickfix`].
    for (ident_idx, candidates) in &res.ambiguous {
        let ident = &hir.idents[*ident_idx];
        let module_names: Vec<&str> = candidates
            .iter()
            .map(|(uri, _)| crate::index::module_name_from_uri(uri).unwrap_or("<unknown>"))
            .collect();
        let name = &index.symbols[ident.symbol];
        out.diagnostics.push(SemanticDiagnostic::structural(
            Severity::Error,
            "ambiguous-symbol",
            format!(
                "ambiguous `{name}` is exported by {} modules ({}); use a fully-qualified name like `{}::{name}`",
                module_names.len(),
                module_names.join(", "),
                module_names.first().copied().unwrap_or("<module>"),
            ),
            ident.byte_range.clone(),
        ));
    }

    // `private-cross-module-name` — the bare ident only matched
    // `private` decls in foreign modules, so the runtime rejects it as
    // "unresolved identifier" but the project closure *does* contain a
    // decl by that name reachable through its FQN. Surface the FQN so
    // the user (and the quickfix at
    // `ide::quickfix::private_cross_module_fix`) can rewrite the call
    // site. Lexicographic-min module pick keeps the message stable
    // when several modules happen to have a private namesake.
    for (ident_idx, candidates) in &res.private_cross_module {
        let ident = &hir.idents[*ident_idx];
        let name = &index.symbols[ident.symbol];
        let module = candidates
            .iter()
            .filter_map(|(uri, _)| crate::index::module_name_from_uri(uri))
            .min()
            .unwrap_or("<module>");
        out.diagnostics.push(SemanticDiagnostic::structural(
            Severity::Error,
            "private-cross-module-name",
            format!("`{name}` is private in `{module}`; use `{module}::{name}`"),
            ident.byte_range.clone(),
        ));
    }

    // Reject more generic parameters than the GreyCat runtime accepts.
    // A function takes exactly one (`fn f<T>`); two+ is a runtime
    // *syntax error*. A type takes two (`type Map<K, V>` is the widest).
    // The grammar accepts any arity (the `type_params` rule is
    // `sepBy1(",", ident)`), so enforcing the ceiling is the analyzer's
    // job. Point the diagnostic at the first over-limit generic so
    // quick-fix tooling can target it precisely.
    const MAX_FN_GENERICS: usize = 1;
    const MAX_TYPE_GENERICS: usize = 2;
    for d in &module.decls {
        let (name_ident, generics, kind_label, max) = match &hir.decls[*d] {
            Decl::Fn(fnd) => (fnd.name, &*fnd.generics, "function", MAX_FN_GENERICS),
            Decl::Type(td) => (td.name, &*td.generics, "type", MAX_TYPE_GENERICS),
            _ => continue,
        };
        if generics.len() > max {
            let first_over_limit = generics[max];
            out.diagnostics.push(SemanticDiagnostic::structural(
                Severity::Error,
                "too-many-generics",
                format!(
                    "{kind_label} `{}` has {} generic parameters; \
                     the GreyCat runtime supports at most {max}",
                    &index.symbols[hir.idents[name_ident].symbol],
                    generics.len(),
                ),
                hir.idents[first_over_limit].byte_range.clone(),
            ));
        }
    }

    // Surface hard parse failures recorded at HIR lowering time:
    // malformed char escape, malformed ISO-8601 shape. Numeric
    // overflow / float precision loss now route through the
    // `literal-overflow` lint rule (lint.rs) so users can suppress
    // them site-by-site — the analyzer's diagnostic vec is a one-way
    // surface that doesn't honour `// gcl-lint-off`.
    for (_, expr) in hir.exprs.iter() {
        if let Expr::Literal(LiteralExpr {
            kind,
            parse_issue: Some(issue),
            byte_range,
        }) = expr
        {
            let message = match (kind, issue) {
                (LiteralKind::Char(_), ParseIssue::Malformed) => {
                    "malformed char literal: unrecognised escape sequence"
                }
                (LiteralKind::Iso8601(_), ParseIssue::Malformed) => "malformed ISO-8601 literal",
                (_, ParseIssue::Suffix) => "unknown suffix",
                _ => continue,
            };

            out.diagnostics.push(SemanticDiagnostic::structural(
                Severity::Error,
                "invalid-literal",
                message.to_string(),
                byte_range.clone(),
            ));
        }
    }

    out
}

/// Build a TypeRegistry from the module's user declarations. Each
/// `type Foo {}` becomes a Named("Foo") TypeId; later phases can
/// elaborate the type's attribute list separately.
///
/// Also populates [`AnalysisResult::type_decls`] (name → HIR
/// `TypeDecl` index) so  member resolution can navigate from a
/// receiver's `TypeId` back to the declaring node.
fn register_module_types(
    hir: &Hir,
    arena: &mut TypeArena,
    out: &mut AnalysisResult,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    module_uri: &Uri,
) {
    let Some(module) = hir.module.as_ref() else {
        return;
    };
    for d in &module.decls {
        let decl = &hir.decls[*d];
        match decl {
            Decl::Type(td) => {
                let name = hir.idents[td.name].symbol;
                // Mint `TypeKind::Type(handle)` for the local decl.
                // The project pipeline's `ingest` always pre-pop'd
                // the registry; the standalone `analyze` /
                // `analyze_with_index` test entries do the same
                // pre-walk. A missing handle here would mean the
                // caller bypassed both — `Unresolved` is the
                // safest fallback (behaves like `any?`).
                let id = match index
                    .item_id_for(module_uri, name)
                    .filter(|item| decl_registry.lookup(*item).is_some())
                {
                    Some(item) => arena.alloc_type(item),
                    None => arena.unresolved(name, (0, 0)),
                };
                out.registry.register(name, id);
                out.type_decls.insert(name, *d);
            }
            Decl::Enum(ed) => {
                let name = hir.idents[ed.name].symbol;
                let variants: Box<[greycat_analyzer_core::Symbol]> = ed
                    .fields
                    .iter()
                    .map(|f| hir.idents[hir.enum_fields[*f].name].symbol)
                    .collect();
                let id = arena.alloc(Type {
                    kind: TypeKind::Enum { name, variants },
                    nullable: false,
                });
                out.registry.register(name, id);
                out.type_decls.insert(name, *d);
            }
            _ => {}
        }
    }
}

/// Narrowings derived from an `if` condition. Each list
/// holds *binding* idents (from `Resolutions`) and the override type to
/// install in the matching branch — `None` means "strip nullable from
/// the current type", `Some(ty)` means "set to this concrete type"
/// (used by `is` type guards).
#[derive(Default)]
struct CondNarrows {
    then_non_null: Vec<Idx<Ident>>,
    else_non_null: Vec<Idx<Ident>>,
    /// `(binding, type)` pairs from `x is T` — narrow x to T in then.
    then_typed: Vec<(Idx<Ident>, Idx<TypeRef>)>,
    /// Non-null narrows for member-access *paths*
    /// produced by `foo.bar != null` style guards. Same semantics as
    /// `then_non_null` / `else_non_null`, just keyed by a string path
    /// rather than an ident handle. The path is built from
    /// `Cx::member_path` and only shapes that root in an Ident /
    /// `this` literal participate.
    then_member_non_null: Vec<String>,
    else_member_non_null: Vec<String>,
    /// `(path, type)` pairs from `foo.bar is T` — narrow the member-access
    /// path to T in the then-branch. Mirrors `then_typed` for member paths.
    then_member_typed: Vec<(String, Idx<TypeRef>)>,
    /// `(binding, type)` pairs that hold on the *else* branch. Populated
    /// only via negation (`!(x is T)`); the post-if `then_terminates`
    /// path uses these to lift `is`-narrows past an early-throw guard
    /// like `if (!(x is T)) { throw }; use(x as T);`.
    else_typed: Vec<(Idx<Ident>, Idx<TypeRef>)>,
    /// Same as `then_member_typed` but for the else branch (under `!`).
    else_member_typed: Vec<(String, Idx<TypeRef>)>,
    /// Disjunctive `is`-narrows: `x is T1 || x is T2` narrows `x` to
    /// `T1 | T2` in the then-branch. Each entry is `(ident, type-refs)`
    /// applied as a `TypeKind::Union` narrow. Populated by the `||`
    /// arm when the same ident is `is`-narrowed on both sides.
    then_typed_union: Vec<(Idx<Ident>, Vec<Idx<TypeRef>>)>,
    /// Same as `then_typed_union` but for the else branch (populated
    /// only via negation).
    else_typed_union: Vec<(Idx<Ident>, Vec<Idx<TypeRef>>)>,
    /// Pre-lowered narrows — same `(binding, type)` shape as
    /// `then_typed` / `else_typed`, but the type is a [`TypeId`]
    /// computed by the analyzer (not a syntactic `Idx<TypeRef>` the
    /// user wrote). Used for the `narrow_complement` dispatcher, which
    /// returns the type the else-branch should narrow to when `is T`
    /// rules out arms of a union (P41) or concrete derivatives of an
    /// abstract sealed hierarchy (P42). Both sides surface here:
    /// `else_typed_id` is the primary; `then_typed_id` only fills via
    /// the `!` swap so `if (!(x is T)) { return; }` lifts the same
    /// way `if (x is T) { } else { return; }` would.
    then_typed_id: Vec<(Idx<Ident>, TypeId)>,
    else_typed_id: Vec<(Idx<Ident>, TypeId)>,
    /// Bindings narrowed to `TypeKind::Null` on the then-branch.
    /// Populated by `x == null` / `null == x`. Materialized via
    /// `arena.null()` in `apply_then_narrows` / the `Stmt::If`
    /// then-entry path.
    then_null: Vec<Idx<Ident>>,
    /// Mirror for the else-branch. Populated by `x != null` /
    /// `null != x`. Materialized via `arena.null()` in
    /// `apply_else_narrows` / the `Stmt::If` else-entry path.
    else_null: Vec<Idx<Ident>>,
    /// Member-path version of `then_null`. Populated by
    /// `a.b == null` / `null == a.b`.
    then_member_null: Vec<String>,
    /// Member-path version of `else_null`. Populated by
    /// `a.b != null` / `null != a.b`.
    else_member_null: Vec<String>,
    /// `true` iff the condition is a single atomic `Expr::Is` (possibly
    /// wrapped in `Expr::Paren` or negated by `Expr::Unary(Not, …)`).
    /// The complement narrow is only sound on atomic conditions —
    /// inside `A && B`, the else-branch holds when "at least one
    /// failed", and we can't tell which; inside `A || B`, the
    /// complement is sound *per-shape* but disjunctive narrowing is
    /// owned by P42.4. Gate the `narrow_complement` call on this flag.
    is_atomic_is: bool,
    /// Enum-value-set narrows: `(binding, enum_sym, allowed_variants)`
    /// triples. The binding is constrained to one of `allowed_variants`
    /// of the named enum on the matching branch. Populated by
    /// `x == E::V` (a singleton set) and chains of those joined by
    /// `||` (which union the sets per (binding, enum)). On apply, the
    /// `Stmt::If` then/else entry intersects with any narrow already
    /// on the stack — so an inner `if (c == Red)` inside an outer
    /// `if (c == Red || c == Green) { ... }` lands at `{Red}`.
    then_enum_values: Vec<(Idx<Ident>, Symbol, Vec<Symbol>)>,
    else_enum_values: Vec<(Idx<Ident>, Symbol, Vec<Symbol>)>,
}

/// One arm in an enum-equality chain.
struct EnumChainArm {
    if_stmt_id: Idx<Stmt>,
    variant: Symbol,
}

/// Enum-value narrow value: which enum the binding is restricted to,
/// and the set of allowed variants. Stored per binding ident in the
/// `enum_value_narrows` stack frames.
type EnumValueNarrow = (Symbol, FxHashSet<Symbol>);

/// An `if (x == E::A) else if (x == E::B) ...` chain.
struct EnumChain {
    /// The shared binding ident the chain dispatches on. Used by the
    /// exhaustiveness check to consult any enum-value narrow already
    /// on the stack — an outer `if (x == E::A || x == E::B) { ... }`
    /// shrinks the set of expected variants the chain has to cover.
    binding: Idx<Ident>,
    enum_name: Symbol,
    arms: Vec<EnumChainArm>,
    /// `true` when the chain ends with a final `else { ... }` or with
    /// a non-conforming `else if` — both act as catch-alls.
    has_final_else: bool,
}

struct Cx<'a> {
    hir: &'a Hir,
    res: &'a Resolutions,
    out: &'a mut AnalysisResult,
    /// URI of the module currently being analyzed. Lets cross-module
    /// resolution paths (`resolve_decl_handle_from`) prefer same-
    /// module candidates and reach private decls in their own module.
    module_uri: &'a Uri,
    /// Symbol for the module currently being analyzed — i.e.
    /// `module_name_from_uri(module_uri)` interned. Lets the body
    /// walker mint `ItemKey { module: self.module_sym, name }` for
    /// any same-module type lookup without going through
    /// `index.item_id_for`.
    module_sym: Symbol,
    /// Project-wide type arena. Owned by `ProjectAnalysis`, so
    /// every module's analyzer mints into the same `TypeArena` and
    /// `TypeId`s are comparable across module boundaries.
    arena: &'a mut TypeArena,
    /// Cross-module project index. Per-file callers pass an
    /// empty [`ProjectIndex::new`]; the project pipeline passes the
    /// index it just rebuilt. Used by `lower_type_ref` to recognize
    /// type names that aren't declared in this module.
    index: &'a ProjectIndex,
    /// Project-wide decl handle registry. Used by sites that resolve
    /// foreign types — they look up `decl_registry.lookup(uri, idx)`
    /// and mint `arena.alloc_type(handle)`. Per-file callers pass an
    /// empty registry; sites then fall back to `Unresolved`.
    decl_registry: &'a DeclRegistry,
    /// Null-flow narrowing stack. Each frame is a binding ident
    /// → temporary `TypeId` override. Frames are pushed on block /
    /// then-branch / else-branch entry and popped on exit, so a
    /// narrowing introduced inside a block stays alive for the rest
    /// of that block but doesn't leak to siblings.
    narrows: Vec<FxHashMap<Idx<Ident>, TypeId>>,
    /// Parallel narrow stack keyed by member-access
    /// *path* (e.g. `"this.matchingNormalisation"`,
    /// `"foo.bar.baz"`). A path's presence in any frame means the
    /// member access at that path is *guaranteed non-null* in the
    /// current scope. Frames are pushed / popped in lockstep with
    /// `narrows`. Lets `if (foo.bar != null) { use(foo.bar); }`
    /// narrow the second `foo.bar` to its non-null form, mirroring
    /// the ident-level narrow flow but across structural member
    /// chains. Best-effort — `foo[i].bar` or `getThing().bar` have
    /// no stable path and skip narrowing.
    member_narrows: Vec<FxHashSet<String>>,
    /// Parallel typed-narrow stack for member-access paths from
    /// `foo.bar is T` guards. A path's presence in any frame means the
    /// member access at that path is *guaranteed of type T* in the
    /// current scope. Frames are pushed / popped in lockstep with
    /// `narrows`. Mirrors `then_typed` for member paths.
    member_typed_narrows: Vec<FxHashMap<String, TypeId>>,
    /// Parallel enum-value-set narrow stack. Each frame maps a binding
    /// ident to `(enum_sym, allowed_variants)` — "this binding is one
    /// of these variants of this enum in the current scope." Populated
    /// from `CondNarrows::{then,else}_enum_values` on `Stmt::If`
    /// branch entry. Read by `check_enum_exhaustiveness` to scope the
    /// "expected" variant set to whatever an enclosing guard already
    /// allowed (so an outer `if (c == Red || c == Green) { ... }`
    /// makes a contained `if (c == Red) else if (c == Green) ...` chain
    /// exhaustive).
    enum_value_narrows: Vec<FxHashMap<Idx<Ident>, EnumValueNarrow>>,
    /// `Stmt::If` ids already accounted for as nested members of an
    /// enclosing exhaustiveness chain. Suppresses duplicate
    /// "non-exhaustive" diagnostics on inner `else if` arms.
    chain_member_ifs: FxHashSet<Idx<Stmt>>,
    /// Generic-context stack: type-parameter names visible at the
    /// current scope, mapped to their declaring [`GenericOwner`].
    /// Entered on `fn f<T>(...)` / `type Foo<T> {}`. The stack is a
    /// `Vec<HashMap>` so nested fns inside a generic type see both
    /// outer and inner names.
    generics_in_scope: Vec<FxHashMap<Symbol, GenericOwner>>,
    /// `this` typing stack. Pushed on entry to a
    /// type's method body (in `visit_type_decl`), popped on exit.
    /// `LiteralKind::This` returns the top of the stack so a
    /// reference to `this` inside `type Foo<T> { fn m() { this } }`
    /// types as `Generic { name: "Foo", args: [GenericParam(T)] }`
    /// — matches what an external `node<Foo<int>>` deref would see.
    /// Empty outside method bodies (top-level fns / lambdas).
    this_stack: Vec<TypeId>,
    /// `true` while the body of a `static fn` is being visited.
    /// `this_stack` alone can't tell static methods from instance
    /// ones — both are walked under the type-level `this_stack` push.
    /// `check_private_attr_write` uses this flag to keep `static fn`
    /// bodies out of the "writes from inside the owning type's body
    /// are allowed" rule — the runtime accepts private writes only
    /// from non-static methods (and the constructor).
    inside_static_fn: bool,
    /// Type-level generic params (`type Foo<T>`) referenced from inside a
    /// `static` method — `static fn make(): T`, `T {}`, etc. A static
    /// carries no instance, so the type parameter is unbound: GreyCat has
    /// no bounded generics, the runtime can't construct or dispatch on
    /// `T`, and rejects any use of it. Recorded here during
    /// `lower_type_ref` (deduped by `Idx<TypeRef>`) and surfaced once
    /// after the walk as `generic-in-static-context` — `lower_type_ref`
    /// is a multi-call helper, so emitting inline would double-report.
    static_generic_uses: FxHashSet<Idx<TypeRef>>,
}

/// Body-walker view of [`Cx`]'s fields for the shared lowering ladder.
/// Split-borrowed from `Cx` so the ladder gets `&mut arena` separately
/// from the rest (incl. the `&mut static_generic_uses` sink).
struct CxLowerEnv<'a> {
    hir: &'a Hir,
    index: &'a ProjectIndex,
    decl_registry: &'a DeclRegistry,
    module_uri: &'a Uri,
    out_registry: &'a TypeRegistry,
    type_decls: &'a FxHashMap<Symbol, Idx<Decl>>,
    generics_in_scope: &'a [FxHashMap<Symbol, GenericOwner>],
    inside_static_fn: bool,
    static_generic_uses: &'a mut FxHashSet<Idx<TypeRef>>,
}

impl TypeRefLowering for CxLowerEnv<'_> {
    fn hir(&self) -> &Hir {
        self.hir
    }
    fn index(&self) -> &ProjectIndex {
        self.index
    }
    fn decl_registry(&self) -> &DeclRegistry {
        self.decl_registry
    }
    fn current_uri(&self) -> Option<&Uri> {
        Some(self.module_uri)
    }
    fn lookup_local(&self, name: Symbol) -> Option<TypeId> {
        self.out_registry.lookup(name)
    }
    fn lookup_generic(&self, name: Symbol) -> Option<GenericOwner> {
        lower_type_ref::lookup_generic_in(self.generics_in_scope, name)
    }
    fn generic_arity_for(&self, name: Symbol) -> Option<usize> {
        lower_type_ref::generic_arity_for(name, self.hir, self.type_decls, self.index)
    }
    fn inside_static_fn(&self) -> bool {
        self.inside_static_fn
    }
    fn note_static_generic_use(&mut self, idx: Idx<TypeRef>) {
        self.static_generic_uses.insert(idx);
    }
}

impl<'a> Cx<'a> {
    #[inline]
    fn any_nullable(&mut self) -> TypeId {
        self.arena.any_nullable()
    }

    #[inline]
    fn any(&mut self) -> TypeId {
        self.arena.any()
    }

    #[inline]
    fn null(&mut self) -> TypeId {
        self.arena.null()
    }

    /// Emit "always (true|false)" diagnostic on a loop condition
    /// (`while` / `for`) and record the decided outcome for the
    /// `unreachable` lint. `while` and `for` with always-false
    /// conditions have an unreachable body; with always-true they
    /// are intentional infinite loops (no dead code).
    fn diagnose_decidable_loop_condition(
        &mut self,
        stmt_id: Idx<Stmt>,
        condition: Idx<Expr>,
        kind: &str,
    ) {
        let Some(b) = self.trivially_decidable(condition) else {
            return;
        };
        // A written bool constant (`while (true)`) is provably decidable
        // but reveals nothing the author didn't type — skip the warning.
        if !self.condition_is_bool_literal(condition) {
            let range = self.hir.exprs[condition].byte_range();
            let msg = if b {
                format!("{kind} condition is always true")
            } else {
                format!("{kind} condition is always false")
            };
            self.surface_lint("decidable-condition", LintSeverity::Warning, msg, range);
        }
        // Only record always-false: the `unreachable` lint only
        // flags dead bodies. Always-true loops are intentional.
        if !b {
            self.out.decidable_conditions.insert(stmt_id, b);
        }
    }

    /// `true` when the condition is a written boolean constant (modulo
    /// `( )` and `!`). Such a condition is provably decidable yet carries
    /// no information the author didn't type, so the `decidable-condition`
    /// warning is suppressed for it. Type-derived decidability (`x != null`
    /// on a non-nullable `x`, `is`-contradictions) does not bottom out in a
    /// literal and still warns.
    fn condition_is_bool_literal(&self, cond_id: Idx<Expr>) -> bool {
        match &self.hir.exprs[cond_id] {
            Expr::Literal(LiteralExpr {
                kind: LiteralKind::Bool(_),
                ..
            }) => true,
            Expr::Paren(inner, _) => self.condition_is_bool_literal(*inner),
            Expr::Unary(UnaryExpr {
                op: UnaryOp::Not,
                operand,
                ..
            }) => self.condition_is_bool_literal(*operand),
            _ => false,
        }
    }

    /// Compositional "is this condition trivially decidable?" pass.
    /// Returns `Some(true)` / `Some(false)` when the condition's truth
    /// value is statically known from the declared types of its
    /// operands; returns `None` otherwise.
    ///
    /// Recognized shapes:
    /// - Bool literals (`true` / `false`).
    /// - `x != null` / `x == null` against an ident whose declared
    ///   (or already-narrowed) type is non-nullable — always true /
    ///   always false respectively.
    /// - Parenthesized sub-expressions (transparent).
    /// - `!E` (invert).
    /// - `A && B`: false if either side is false; true if both sides
    ///   are true; otherwise undecidable.
    /// - `A || B`: true if either side is true; false if both sides
    ///   are false; otherwise undecidable.
    ///
    /// `is`-narrow contradictions (e.g. `x is int && x is float`) are
    /// handled separately in
    /// [`Self::diagnose_then_typed_contradictions`] — they need
    /// receiver-type intersection analysis that doesn't fit the
    /// compositional pattern here.
    fn trivially_decidable(&self, cond_id: Idx<Expr>) -> Option<bool> {
        match &self.hir.exprs[cond_id] {
            Expr::Paren(inner, _) => self.trivially_decidable(*inner),
            Expr::Literal(LiteralExpr {
                kind: LiteralKind::Bool(b),
                ..
            }) => Some(*b),
            Expr::Unary(UnaryExpr {
                op: UnaryOp::Not,
                operand,
                ..
            }) => self.trivially_decidable(*operand).map(|b| !b),
            Expr::Binary(BinaryExpr {
                op, left, right, ..
            }) => match op {
                BinOp::And => match (
                    self.trivially_decidable(*left),
                    self.trivially_decidable(*right),
                ) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                },
                BinOp::Or => match (
                    self.trivially_decidable(*left),
                    self.trivially_decidable(*right),
                ) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                },
                BinOp::Eq | BinOp::Neq => {
                    let name = self.ident_compared_to_null(*left, *right)?;
                    let def = match self.res.lookup(name)? {
                        Definition::Param(d) | Definition::Local(d) => d,
                        _ => return None,
                    };
                    let ty = self.lookup_def_type(def)?;
                    if self.arena.get(ty).nullable {
                        return None;
                    }
                    // x : T (non-nullable) vs null —
                    // `x != null` always true; `x == null` always false.
                    Some(matches!(op, BinOp::Neq))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Check `is`-narrows in the then-branch for trivially decidable
    /// outcomes:
    ///
    /// - **Always false (contradiction):** two `is`-checks on the
    ///   same ident with no common subtype, e.g.
    ///   `x is int && x is float`. The ident is re-narrowed to
    ///   `never` and the if condition flagged.
    /// - **Always false (vs declared type):** the asserted type is
    ///   disjoint from the binding's known type at the if site, e.g.
    ///   `x: int; if (x is float)`. The ident is re-narrowed to
    ///   `never` and the condition flagged.
    /// - **Always true:** the binding's known type is already
    ///   assignable to the asserted type, e.g.
    ///   `x: int; if (x is int)`. The narrow is left in place (it
    ///   matches the existing type) and the condition flagged as a
    ///   no-op check.
    ///
    /// Compatible groups (one asserted type assignable to another,
    /// e.g. `x is Animal && x is Cat` with `Cat <: Animal`) collapse
    /// to the most specific type without diagnostics.
    fn diagnose_then_typed_contradictions(
        &mut self,
        multi: &FxHashMap<Idx<Ident>, (Option<TypeId>, Vec<TypeId>)>,
        condition: Idx<Expr>,
        stmt_id: Idx<Stmt>,
    ) {
        let mut emitted = false;
        let mut never_id: Option<TypeId> = None;
        for (ident, (known_opt, tys)) in multi {
            // When `known` is the runtime-erased shape of a generic
            // result, the decidability verdict is *because* of erasure —
            // say so, so the user connects it to the value's runtime type
            // rather than reading it as a plain logic error.
            let erased_note = if self.out.def_runtime_types.contains_key(ident) {
                " (function generics are erased to `any?` at runtime)"
            } else {
                ""
            };
            // Find the most specific asserted type (one assignable to
            // all others). If none → the asserted types may be
            // pairwise disjoint *or* `is_assignable_to` lacks the
            // information to compare them (e.g. user types with
            // inheritance, not yet wired into the subtyping relation).
            // Only claim "contradiction" when every type in the group
            // is a primitive (`is_any_primitive`), where assignability is
            // pure decl identity and disjointness is therefore provable.
            let mut most_specific: Option<TypeId> = None;
            'outer: for &cand in tys {
                for &other in tys {
                    if cand == other {
                        continue;
                    }
                    if !self.arena.is_assignable_to(cand, other) {
                        continue 'outer;
                    }
                }
                most_specific = Some(cand);
                break;
            }
            let all_primitive = tys.iter().all(|t| self.arena.is_builtin(*t));

            let mk_never = |arena: &mut TypeArena, slot: &mut Option<TypeId>| -> TypeId {
                match *slot {
                    Some(id) => id,
                    None => {
                        let id = arena.never();
                        *slot = Some(id);
                        id
                    }
                }
            };

            match most_specific {
                None if all_primitive => {
                    let never = mk_never(self.arena, &mut never_id);
                    self.write_narrow(*ident, never);
                    if !emitted {
                        let name = self.ident_text(*ident).to_string();
                        let pretty: Vec<String> =
                            tys.iter().map(|t| self.display(*t).to_string()).collect();
                        let msg = format!(
                            "condition is always false: `{}` cannot simultaneously be {}",
                            name,
                            pretty.join(" and "),
                        );
                        let range = self.hir.exprs[condition].byte_range();
                        self.surface_lint("decidable-condition", LintSeverity::Warning, msg, range);
                        emitted = true;
                        self.out.decidable_conditions.insert(stmt_id, false);
                    }
                }
                None => {
                    // Asserted types are non-primitive and we can't
                    // prove pairwise disjointness. Skip the diagnostic
                    // and fall back to last-write-wins narrow (already
                    // applied by the caller).
                }
                Some(asserted) => {
                    if let Some(known) = *known_opt {
                        let known_kind = &self.arena.get(known).kind;
                        let known_is_top = matches!(known_kind, TypeKind::Any | TypeKind::Never);
                        // Known type is already a subtype of asserted
                        // → the check can't filter anything. Skip when
                        // known is `any` (top, every value passes
                        // trivially via raw-form widening, but the
                        // check is a meaningful runtime discriminator)
                        // and when known is `never` (downstream
                        // diagnostics already flag the unreachable
                        // scope). Uses the index-aware
                        // [`Cx::is_assignable`] so a `Sub: Super`
                        // `extends` chain is honored — without this
                        // hop the bare relation only knows decl-
                        // handle identity and would miss "Sub is
                        // already a Super" trivially.
                        if !known_is_top && self.is_assignable(known, asserted) {
                            if !emitted {
                                let name = self.ident_text(*ident).to_string();
                                let msg = format!(
                                    "condition is always true: `{}` is already of type `{}`{}",
                                    name,
                                    self.display(known),
                                    erased_note,
                                );
                                let range = self.hir.exprs[condition].byte_range();
                                self.surface_lint(
                                    "decidable-condition",
                                    LintSeverity::Warning,
                                    msg,
                                    range,
                                );
                                emitted = true;
                                self.out.decidable_conditions.insert(stmt_id, true);
                            }
                            self.write_narrow(*ident, asserted);
                            continue;
                        }
                        // Known and asserted are disjoint (neither
                        // assignable to the other) — the check can
                        // never pass. Skip when known is `any` (top)
                        // or `never` for the same reasons.
                        // Critical: the *forward* direction
                        // (`is_assignable(asserted, known)`) uses
                        // the project-aware extends walk. Without it
                        // `s: Shape; s is Rect` where `Rect extends
                        // Shape` would be flagged "always false" —
                        // core's bare relation can't see the chain.
                        if !known_is_top && !self.is_assignable(asserted, known) {
                            let never = mk_never(self.arena, &mut never_id);
                            self.write_narrow(*ident, never);
                            if !emitted {
                                let name = self.ident_text(*ident).to_string();
                                let msg = format!(
                                    "condition is always false: `{}` of type `{}` can never be `{}`{}",
                                    name,
                                    self.display(known),
                                    self.display(asserted),
                                    erased_note,
                                );
                                let range = self.hir.exprs[condition].byte_range();
                                self.surface_lint(
                                    "decidable-condition",
                                    LintSeverity::Warning,
                                    msg,
                                    range,
                                );
                                emitted = true;
                                self.out.decidable_conditions.insert(stmt_id, false);
                            }
                            continue;
                        }
                    }
                    self.write_narrow(*ident, asserted);
                }
            }
        }
    }

    /// Compute the complement of an `is`-guard: given the currently-
    /// known type `known` and the asserted-type `asserted`, return the
    /// `TypeId` that `known` should narrow to in the *else* branch
    /// (and in any continuation past a then-side early exit).
    ///
    /// Dispatches by `known`'s `TypeKind`:
    /// - `Union { alts }` — subtract alts assignable to
    ///   `asserted`, collapse single-survivor down to the lone alt,
    ///   otherwise return a fresh `Union { alts: survivors }`.
    /// - `Type(decl)` with `decl` abstract — sealed-hierarchy
    ///   subtraction over `index.subtype_closure`, with the mandatory
    ///   ancestor-collapse via `abstract_by_closure_set`.
    ///
    /// Nullability is preserved from `known` in every arm — `is T`
    /// rules out specific runtime tags, not the `null` value.
    ///
    /// Returns `None` when no rule applies (`known` isn't one of the
    /// supported kinds; `Generic { … }` roots fall out per P42.6) or
    /// when the subtraction leaves zero alts (vacuously-true
    /// if-condition; defer the diagnostic to P42.5's
    /// `exhaustive-is-check`).
    fn narrow_complement(&mut self, known: TypeId, asserted: TypeId) -> Option<TypeId> {
        let known_ty = self.arena.get(known).clone();
        let nullable = known_ty.nullable;
        let inner = match &known_ty.kind {
            TypeKind::Union { alts } => self.narrow_complement_union(alts, asserted)?,
            TypeKind::Type(decl) => self.narrow_complement_abstract(*decl, asserted)?,
            // P42.6 — out of scope: generic-root sealed hierarchies.
            //
            // A `TypeKind::Generic { decl, args }` (e.g. abstract
            // `Container<T>` with concrete `ArrayContainer<T>` /
            // `MapContainer<T>` derivatives) would in principle admit
            // the same closure-subtraction trick, but the closure
            // index `ProjectIndex::subtype_closure` (built by
            // `populate_subtype_indices` in `project.rs`) is keyed by
            // bare `Symbol` — it ignores type args entirely. Honoring
            // the receiver's instantiation would require either:
            //
            // 1. extending `subtype_closure` to map `(Symbol, args)`
            //    keys to leaf sets parameterized over the same args
            //    (most accurate, large change), or
            // 2. building closures per-monomorphization on demand at
            //    the call site (smaller change, slower per use).
            //
            // Bailing here keeps today's behavior at the
            // `Generic { … }` shape — no narrow lifted, the user
            // sees the original declared type in the continuation,
            // and the rest of the analyzer behaves as it did before
            // P41/P42. The two inner bail sites
            // (`narrow_complement_abstract` and `narrow_is_exhausted`)
            // refer back to this comment.
            _ => return None,
        };
        if nullable {
            Some(self.arena.nullable(inner))
        } else {
            Some(inner)
        }
    }

    fn narrow_complement_union(&mut self, alts: &[TypeId], asserted: TypeId) -> Option<TypeId> {
        let mut survivors: Vec<TypeId> = Vec::with_capacity(alts.len());
        for alt in alts.iter() {
            // An alt survives the `is T` test only when it is NOT
            // assignable to `T` — i.e. the runtime would NOT report
            // `is T` as true for a value of type `alt`. Strip
            // nullability before testing so `T?` and `T` line up.
            let alt_t = self.arena.get(*alt).clone();
            let alt_non_null = if alt_t.nullable {
                self.arena.alloc(Type {
                    kind: alt_t.kind,
                    nullable: false,
                })
            } else {
                *alt
            };
            let asserted_t = self.arena.get(asserted).clone();
            let asserted_non_null = if asserted_t.nullable {
                self.arena.alloc(Type {
                    kind: asserted_t.kind,
                    nullable: false,
                })
            } else {
                asserted
            };
            if crate::project::is_assignable_to_with_index(
                self.index,
                self.decl_registry,
                self.arena,
                alt_non_null,
                asserted_non_null,
            ) {
                continue;
            }
            survivors.push(*alt);
        }
        if survivors.is_empty() {
            // Exhausted — `exhaustive-is-check` owns the
            // diagnostic. Returning `None` keeps the apply sites
            // untouched (no narrow lifted into the unreachable
            // continuation).
            return None;
        }
        if survivors.len() == 1 {
            Some(survivors[0])
        } else {
            Some(self.arena.alloc(Type {
                kind: TypeKind::Union {
                    alts: survivors.into_boxed_slice(),
                },
                nullable: false,
            }))
        }
    }

    /// Sealed-hierarchy `is`-complement: subtract `closure(asserted)`
    /// from `closure(sup_decl)` and collapse the result.
    ///
    /// Both closures are canonically sorted (`subtype_closure` stores
    /// each entry sorted by `Symbol`'s `Ord` impl), so the subtraction
    /// is a linear merge over two sorted lists and the result is
    /// itself sorted — feeding `abstract_by_closure_set.get(...)`
    /// directly without re-canonicalization.
    ///
    /// Returns `None` when:
    /// - `sup_decl` is not abstract (a concrete `Sup` is itself a
    ///   runtime possibility, no subtraction is sound).
    /// - `asserted` is not a `TypeKind::Type(d)`: `Generic { … }` is
    ///   deferred per P42.6; other kinds (primitives, `Lambda`, …)
    ///   don't participate in the sealed hierarchy.
    /// - either side is missing from `subtype_closure` (sentinel /
    ///   half-loaded project).
    /// - the subtraction leaves zero leaves (exhausted; P42.5 owns
    ///   the diagnostic).
    fn narrow_complement_abstract(
        &mut self,
        sup_decl: ItemKey,
        asserted: TypeId,
    ) -> Option<TypeId> {
        if !self.index.is_abstract.contains(&sup_decl) {
            return None;
        }
        let asserted_ty = self.arena.get(asserted);
        let asserted_decl = match &asserted_ty.kind {
            TypeKind::Type(d) => *d,
            // P42.6 — generic-root asserts. The closure index is
            // keyed by bare `ItemKey`, doesn't carry type args; a
            // `TypeKind::Generic` assertion is the same chain-walker
            // gap from the asserted side.
            _ => return None,
        };

        let sup_closure = self.index.subtype_closure.get(&sup_decl)?;
        let asserted_closure = self
            .index
            .subtype_closure
            .get(&asserted_decl)
            .cloned()
            .unwrap_or_default();

        // Linear merge over two sorted `ItemKey` lists.
        let mut diff: Vec<ItemKey> = Vec::with_capacity(sup_closure.len());
        let mut j = 0usize;
        for &s in sup_closure.iter() {
            while j < asserted_closure.len() && asserted_closure[j] < s {
                j += 1;
            }
            if j < asserted_closure.len() && asserted_closure[j] == s {
                continue;
            }
            diff.push(s);
        }

        if diff.is_empty() {
            // Exhausted — `exhaustive-is-check` owns the
            // diagnostic. Returning `None` keeps the apply sites
            // untouched.
            return None;
        }

        // Mandatory ancestor-collapse: if the remaining concrete set
        // exactly equals `closure(A)` for some abstract `A`, the
        // narrow renders as the nominal `Type(A)` rather than the
        // explicit union. Applies only to TypeIds manufactured here
        // (user-written unions in source are preserved verbatim
        // elsewhere).
        let diff_slice: Box<[ItemKey]> = diff.clone().into_boxed_slice();
        if let Some(&abstract_id) = self.index.abstract_by_closure_set.get(&diff_slice) {
            return Some(self.arena.alloc_type(abstract_id));
        }

        // No collapse — build a Union (or singleton) over the
        // surviving concrete leaves.
        let mut alts: Vec<TypeId> = Vec::with_capacity(diff.len());
        for id in &diff {
            alts.push(self.arena.alloc_type(*id));
        }
        if alts.is_empty() {
            return None;
        }
        if alts.len() == 1 {
            return Some(alts[0]);
        }
        Some(self.arena.alloc(Type {
            kind: TypeKind::Union {
                alts: alts.into_boxed_slice(),
            },
            nullable: false,
        }))
    }

    /// Detect whether an `is`-check is *exhaustive*: every concrete
    /// runtime case of `known` is also a runtime case of (one alt of)
    /// `asserted`, so the negative side of the guard is unreachable.
    /// Used to emit the `exhaustive-is-check` diagnostic without
    /// having to distinguish "exhausted" from "no rule applies" in
    /// `narrow_complement`'s `None` return.
    ///
    /// `asserted` may be either a single type (`Type(d)`) or a union
    /// (`x is T1 || x is T2` lowers via `lower_typed_union` to
    /// `Union { alts }`). Both shapes funnel through the same arm
    /// here:
    /// - **Union `known`** (e.g. `var x = a ?? b`): every alt must
    ///   be assignable to *some* alt of `asserted`.
    /// - **Abstract `Type(decl)` `known`**: `closure(known)` must be
    ///   a subset of the union of every asserted alt's closure.
    ///
    /// Concrete `Type(decl)` `known` returns `false`: the existing
    /// per-`is` decidability pass in
    /// [`Self::diagnose_then_typed_contradictions`] already catches
    /// `x: Rect; if (x is Shape)` as "always true" via assignability.
    fn narrow_is_exhausted(&mut self, known: TypeId, asserted: TypeId) -> bool {
        let asserted_alts: Box<[TypeId]> = match &self.arena.get(asserted).kind {
            TypeKind::Union { alts } => alts.clone(),
            _ => Box::new([asserted]),
        };
        let known_ty = self.arena.get(known).clone();
        match &known_ty.kind {
            TypeKind::Union { alts } => {
                let alts = alts.clone();
                alts.iter().all(|alt| {
                    let alt_t = self.arena.get(*alt).clone();
                    let alt_non_null = if alt_t.nullable {
                        self.arena.alloc(Type {
                            kind: alt_t.kind,
                            nullable: false,
                        })
                    } else {
                        *alt
                    };
                    asserted_alts.iter().any(|a_alt| {
                        let a_t = self.arena.get(*a_alt).clone();
                        let a_non_null = if a_t.nullable {
                            self.arena.alloc(Type {
                                kind: a_t.kind,
                                nullable: false,
                            })
                        } else {
                            *a_alt
                        };
                        crate::project::is_assignable_to_with_index(
                            self.index,
                            self.decl_registry,
                            self.arena,
                            alt_non_null,
                            a_non_null,
                        )
                    })
                })
            }
            TypeKind::Type(sup_decl) => {
                if !self.index.is_abstract.contains(sup_decl) {
                    return false;
                }
                let Some(sup_closure) = self.index.subtype_closure.get(sup_decl).cloned() else {
                    return false;
                };
                if sup_closure.is_empty() {
                    return false;
                }
                let mut asserted_set: FxHashSet<ItemKey> = FxHashSet::default();
                for a_alt in &asserted_alts {
                    let a_ty = self.arena.get(*a_alt);
                    let TypeKind::Type(ad) = &a_ty.kind else {
                        // Generic-root asserts. See the
                        // out-of-scope block on
                        // `narrow_complement`'s `_` arm. Returning
                        // `false` here keeps today's behavior — no
                        // exhaustion diag fires when the asserted
                        // side is a `Generic { … }`.
                        return false;
                    };
                    let Some(ac) = self.index.subtype_closure.get(ad) else {
                        return false;
                    };
                    asserted_set.extend(ac.iter().copied());
                }
                sup_closure.iter().all(|s| asserted_set.contains(s))
            }
            _ => false,
        }
    }

    /// Chain `narrow_complement` over a list of asserted-type-refs,
    /// starting from `ident`'s currently-known type. Used by the
    /// disjunctive `(x is T1) || (x is T2)` shape — each asserted
    /// type subtracts from the running complement, equivalent to
    /// `closure(known) \ closure(T1) \ closure(T2) \ …`. Returns
    /// `None` when the helper had nothing to do at the first step
    /// (no rule applies for the binding's known shape) or when any
    /// step in the chain bails out — keeping the apply sites inert
    /// rather than committing a partially-subtracted narrow.
    fn chain_narrow_complement(
        &mut self,
        ident: Idx<Ident>,
        ty_refs: &[Idx<TypeRef>],
    ) -> Option<TypeId> {
        let known = self.lookup_def_type(ident)?;
        let mut complement = known;
        let mut changed = false;
        for ty_ref in ty_refs {
            let asserted = self.lower_type_ref(*ty_ref);
            match self.narrow_complement(complement, asserted) {
                Some(next) => {
                    complement = next;
                    changed = true;
                }
                // First-step bail: nothing to narrow here. Mid-chain
                // bail: either no rule applied for the partial
                // complement (e.g. it collapsed to a kind the helper
                // doesn't model) or the asserted type exhausted the
                // remaining set — P42.5 owns the exhaustion diag.
                // Either way, drop the partial narrow.
                None => return None,
            }
        }
        if changed { Some(complement) } else { None }
    }

    /// Lower a list of `is`-narrow type refs and join them into a
    /// single `TypeKind::Union`. Single-element lists collapse to
    /// the lone alt (no union wrapper). Used to apply disjunctive
    /// `is`-narrows from `x is T1 || x is T2` style guards.
    fn lower_typed_union(&mut self, ty_refs: &[Idx<TypeRef>]) -> TypeId {
        let mut alts: Vec<TypeId> = Vec::with_capacity(ty_refs.len());
        for ty_ref in ty_refs {
            let id = self.lower_type_ref(*ty_ref);
            if !alts.contains(&id) {
                alts.push(id);
            }
        }
        if alts.len() == 1 {
            return alts[0];
        }
        self.arena.alloc(Type {
            kind: TypeKind::Union {
                alts: alts.into_boxed_slice(),
            },
            nullable: false,
        })
    }

    // P35.4
    /// Mint a [`TypeKind::Type`] for a std/core native runtime
    /// sentinel (`function` / `type` / `field`). The `arena.builtins`
    /// keys are always seeded, so these resolve to the canonical
    /// `core::X` identity with or without `core.gcl` loaded.
    ///
    /// Each has its own thin wrapper (`function_ty`, `type_ty`,
    /// `field_ty`, …) so call sites read at a glance which std/core
    /// type they're minting.
    fn function_ty(&mut self) -> TypeId {
        self.arena.alloc_type(self.arena.builtins.function_key)
    }
    fn type_ty(&mut self) -> TypeId {
        self.arena.alloc_type(self.arena.builtins.type_key)
    }
    /// Mint the structural `TypeKind::Lambda` for a value-position
    /// reference to a top-level fn or `static` method. Generic fns
    /// fall back to the opaque `function` type — there's no GCL-
    /// expressible Lambda shape for `fn<T>(x: T): T` in value position
    /// (the user would have to instantiate `T` somehow at the use
    /// site, which isn't a thing in GCL).
    ///
    /// `sig.return_ty: Option<TypeId>` flows through verbatim: a fn
    /// declared without `:` return type produces a Lambda with `ret =
    /// None`, rendered `fn(P)`; with `: R` declared, `ret = Some(R)`,
    /// rendered `fn(P): R`.
    fn fn_ref_ty_from_sig(&mut self, sig: &FnSignature) -> TypeId {
        if !sig.generics.is_empty() {
            return self.function_ty();
        }
        self.arena.lambda(sig.params.clone(), sig.return_ty)
    }
    fn field_ty(&mut self) -> TypeId {
        self.arena.alloc_type(self.arena.builtins.field_key)
    }
    fn record(&mut self, expr: Idx<Expr>, ty: TypeId) {
        self.out.expr_types.insert(expr, ty);
    }
    /// Push a fully-formed [`LintDiagnostic`] into
    /// [`AnalysisResult::surfaced_lints`]. The project-level typed-lint
    /// runner pipes the buffer through `emit_typed`, so a
    /// `// gcl-lint-off <rule>` directive at the right scope will
    /// silence the emit just like any pure-HIR rule.
    fn surface_lint(
        &mut self,
        rule: &'static str,
        severity: LintSeverity,
        message: impl Into<String>,
        range: Range<usize>,
    ) {
        self.out.surfaced_lints.push(LintDiagnostic {
            rule,
            severity,
            message: message.into(),
            byte_range: range,
            tag: None,
        });
    }

    fn diag(
        &mut self,
        severity: Severity,
        code: &'static str,
        message: impl Into<String>,
        range: Range<usize>,
    ) {
        // The analyzer's first pass only emits structural diagnostics
        // (unresolved names, member-resolution failures, exhaustiveness,
        // …). Type-relation diagnostics live in the project pipeline's
        // `validate_type_relations` post-pass — see `DiagCategory`.
        self.out.diagnostics.push(SemanticDiagnostic::structural(
            severity,
            code,
            message.into(),
            range,
        ));
    }

    /// GreyCat has no anonymous objects — the type before `{` is
    /// mandatory. A missing head parses as an empty-symbol `TypeRef`;
    /// flag it over the whole object span and return whether it was
    /// anonymous (so callers skip head-dependent checks). Returns `true`
    /// when the head was anonymous.
    fn check_anonymous_object_head(&mut self, ty: Idx<TypeRef>, range: Range<usize>) -> bool {
        let name = self.hir.type_refs[ty].name;
        if !self.index.symbols[self.hir.idents[name].symbol].is_empty() {
            return false;
        }
        self.diag(
            Severity::Error,
            "anonymous-object",
            "anonymous objects are not supported; specify a type before `{`",
            range,
        );
        true
    }

    fn ident_text(&self, idx: Idx<Ident>) -> &str {
        &self.index.symbols[self.hir.idents[idx].symbol]
    }

    /// Registry-aware type-display wrapper. Renders `Foo` / `Map<int,
    /// String>` for decl-keyed types. Use this whenever a diagnostic
    /// or lint message surfaces a type to the user.
    fn display(&self, id: TypeId) -> crate::display::TypeWithDecls<'_> {
        crate::display::display_type(self.arena, self.decl_registry, &self.index.symbols, id)
    }

    /// Project-aware assignability check. Like
    /// [`is_assignable_to`] but walks the cross-module supertype
    /// chain via `index.is_subtype_of_decl(...)` so
    /// `Sub` assigns to `Sup` whenever `type Sub extends Sup` (and
    /// transitively). Use this for any decidability check on user
    /// types — the bare core relation only knows decl-handle identity.
    fn is_assignable(&mut self, from: TypeId, to: TypeId) -> bool {
        crate::project::is_assignable_to_with_index(
            self.index,
            self.decl_registry,
            self.arena,
            from,
            to,
        )
    }

    fn push_narrow(&mut self) {
        self.narrows.push(FxHashMap::default());
        self.member_narrows.push(FxHashSet::default());
        self.member_typed_narrows.push(FxHashMap::default());
        self.enum_value_narrows.push(FxHashMap::default());
    }

    fn pop_narrow(&mut self) {
        self.narrows.pop();
        self.member_narrows.pop();
        self.member_typed_narrows.pop();
        self.enum_value_narrows.pop();
    }

    fn write_narrow(&mut self, name: Idx<Ident>, ty: TypeId) {
        if let Some(top) = self.narrows.last_mut() {
            top.insert(name, ty);
        }
    }

    /// Record `path` as guaranteed non-null in the current
    /// scope. Subsequent `Expr::Member` evaluations at the same path
    /// strip the result's nullable bit.
    fn write_member_non_null(&mut self, path: Cow<'_, str>) {
        if let Some(top) = self.member_narrows.last_mut() {
            top.insert(path.into());
        }
    }

    /// Drop any non-null narrow recorded for `path`. Call when the
    /// path is reassigned to a value whose nullability is unknown,
    /// so a stale narrow doesn't outlive the assignment.
    fn drop_member_non_null(&mut self, path: &str) {
        for frame in self.member_narrows.iter_mut().rev() {
            frame.remove(path);
        }
    }

    /// `true` iff `path` is guaranteed non-null in the current scope
    /// (any frame on the member-narrow stack contains it).
    fn member_path_is_non_null(&self, path: &str) -> bool {
        self.member_narrows.iter().any(|f| f.contains(path))
    }

    /// Record `path` as guaranteed to be of type `ty` in the current
    /// scope. Subsequent `Expr::Member` / `Expr::Arrow` evaluations at
    /// the same path use this type instead of the declared one.
    fn write_member_typed(&mut self, path: Cow<'_, str>, ty: TypeId) {
        if let Some(top) = self.member_typed_narrows.last_mut() {
            top.insert(path.into(), ty);
        }
    }

    /// Drop any typed narrow recorded for `path`. Call when the path
    /// is reassigned to a value whose type is unknown.
    fn drop_member_typed(&mut self, path: &str) {
        for frame in self.member_typed_narrows.iter_mut().rev() {
            frame.remove(path);
        }
    }
    /// Innermost-first lookup of the narrowed type for a member path.
    fn lookup_member_typed(&self, path: &str) -> Option<TypeId> {
        for frame in self.member_typed_narrows.iter().rev() {
            if let Some(t) = frame.get(path) {
                return Some(*t);
            }
        }
        None
    }

    /// Record that `name` is constrained to one of `variants` of the
    /// enum named by `enum_sym` in the current scope. When a narrow
    /// already exists on the stack for the same `(name, enum_sym)`
    /// pair, the new set is intersected with it — so a `c == Red`
    /// inside an outer `c == Red || c == Green` lands at `{Red}`. A
    /// narrow naming a *different* enum than the existing one is
    /// silently dropped (the binding can only be of one type).
    fn write_enum_value_narrow(&mut self, name: Idx<Ident>, enum_sym: Symbol, variants: &[Symbol]) {
        let new_set: FxHashSet<Symbol> = variants.iter().copied().collect();
        let final_set = match self.lookup_enum_value_narrow(name) {
            Some((existing_enum, existing_set)) => {
                if existing_enum != enum_sym {
                    return;
                }
                existing_set.intersection(&new_set).copied().collect()
            }
            None => new_set,
        };
        if let Some(top) = self.enum_value_narrows.last_mut() {
            top.insert(name, (enum_sym, final_set));
        }
    }

    /// Innermost-first lookup of the enum-value narrow for `name`.
    /// Returns `(enum_sym, allowed_variants)` when a narrow exists in
    /// some enclosing frame.
    fn lookup_enum_value_narrow(&self, name: Idx<Ident>) -> Option<EnumValueNarrow> {
        for frame in self.enum_value_narrows.iter().rev() {
            if let Some((sym, set)) = frame.get(&name) {
                return Some((*sym, set.clone()));
            }
        }
        None
    }

    /// Innermost-first lookup of a binding ident's current type:
    /// narrowing frames win over `def_types`, mirroring the way TS
    /// `narrowing.ts` overlays branch-local strips on the env.
    fn lookup_def_type(&self, name: Idx<Ident>) -> Option<TypeId> {
        for frame in self.narrows.iter().rev() {
            if let Some(t) = frame.get(&name) {
                return Some(*t);
            }
        }
        self.out.def_types.get(&name).copied()
    }

    /// The operand type a *runtime* `is` check actually sees: the
    /// runtime-erased type when the binding holds an erased generic
    /// result ([`crate::erasure`]), else the normal known type. The
    /// runtime evaluates `is` against the erased value, so feeding the
    /// erased type into the `is`-decidability reasoning keeps it honest —
    /// `tuple is Tuple<Table<Person>, int>` is always *false* at runtime
    /// (the value is `Tuple<Table<any?>, int>`), not the "always true"
    /// the materialized type would wrongly imply. A live narrow still
    /// wins (explicit refinement within the analysis).
    fn lookup_def_type_for_is(&self, name: Idx<Ident>) -> Option<TypeId> {
        for frame in self.narrows.iter().rev() {
            if let Some(t) = frame.get(&name) {
                return Some(*t);
            }
        }
        self.out
            .def_runtime_types
            .get(&name)
            .copied()
            .or_else(|| self.out.def_types.get(&name).copied())
    }

    /// Build a string path key for an expression that's a
    /// chain of `Expr::Member` rooted at an `Expr::Ident` (the binding
    /// name) or `Expr::Literal(This)` (yielding `"this"` as the root).
    /// Returns `None` for any other shape (calls, parens of calls, an
    /// offset with a non-stable index, etc.) so we don't accidentally
    /// narrow paths whose receiver is a fresh computed value rather
    /// than a stable reference.
    ///
    /// `arr[N]` with a literal integer index participates as a stable
    /// path segment: `e.attrs[0]` keys distinctly from `e.attrs[1]`
    /// while sharing the receiver root. Non-literal indices (`arr[i]`,
    /// `arr[f()]`, slices `arr[0..3]`) return `None`. Offset's own
    /// `pre_optional` / `post_optional` markers don't enter the key —
    /// they affect nullability, not element identity.
    fn member_path(&self, expr_id: Idx<Expr>) -> Option<String> {
        match &self.hir.exprs[expr_id] {
            Expr::Ident { name: name_idx, .. } => Some(self.ident_text(*name_idx).to_string()),
            Expr::This { .. } => Some("this".to_string()),
            Expr::Member(MemberExpr {
                receiver, property, ..
            }) => {
                let recv_path = self.member_path(*receiver)?;
                let prop = self.ident_text(property.ident());
                Some(format!("{recv_path}.{prop}"))
            }
            // **P19.21** — `x->y` participates in member-narrowing the
            // same way `x.y` does. Distinct separator (`->`) keeps the
            // path keys disjoint from the dot-form so a same-named
            // field on the tag vs the inner type doesn't collide.
            Expr::Arrow(MemberExpr {
                receiver, property, ..
            }) => {
                let recv_path = self.member_path(*receiver)?;
                let prop = self.ident_text(property.ident());
                Some(format!("{recv_path}->{prop}"))
            }
            Expr::Offset(OffsetExpr {
                receiver, index, ..
            }) => {
                let recv_path = self.member_path(*receiver)?;
                if let Expr::Literal(LiteralExpr {
                    kind: LiteralKind::Int(n),
                    ..
                }) = &self.hir.exprs[*index]
                {
                    Some(format!("{recv_path}[{n}]"))
                } else {
                    None
                }
            }
            Expr::Paren(inner, _) => self.member_path(*inner),
            _ => None,
        }
    }

    /// Lift `narrows`'s then-side lists into the current narrow frame.
    /// Used by `Stmt::While` / `Stmt::For` to make loop bodies see the
    /// truthy implication of the loop condition at iteration entry.
    ///
    /// The narrow holds at *body entry of every iteration*; if the body
    /// reassigns the binding to a possibly-null value, subsequent reads
    /// in the same iteration correctly see the reassignment via the
    /// innermost-frame-wins lookup invariant. Mirrors `Stmt::If`'s
    /// then-side application but does not include the `is`-contradiction
    /// diagnostics or `multi_typed` capture (those are If-specific).
    fn apply_then_narrows(&mut self, narrows: &CondNarrows) {
        for ident in &narrows.then_non_null {
            if let Some(cur) = self.lookup_def_type(*ident) {
                let stripped = self.arena.strip_nullable(cur);
                self.write_narrow(*ident, stripped);
            }
        }
        for (ident, ty_ref) in &narrows.then_typed {
            let ty = self.lower_type_ref(*ty_ref);
            self.write_narrow(*ident, ty);
        }
        for (ident, ty_refs) in &narrows.then_typed_union {
            let ty = self.lower_typed_union(ty_refs);
            self.write_narrow(*ident, ty);
        }
        for (ident, ty) in &narrows.then_typed_id {
            self.write_narrow(*ident, *ty);
        }
        for path in &narrows.then_member_non_null {
            self.write_member_non_null(Cow::Borrowed(path));
        }
        for (path, ty_ref) in &narrows.then_member_typed {
            let ty = self.lower_type_ref(*ty_ref);
            self.write_member_typed(Cow::Borrowed(path), ty);
        }
        for ident in &narrows.then_null {
            let null_ty = self.null();
            self.write_narrow(*ident, null_ty);
        }
        for path in &narrows.then_member_null {
            let null_ty = self.null();
            self.write_member_typed(Cow::Borrowed(path), null_ty);
        }
    }

    /// Lift `narrows`'s else-side lists into the current narrow frame.
    /// Used by `Stmt::While` / `Stmt::For` / `Stmt::DoWhile` after the
    /// loop body to install the cond's negation in post-loop scope
    /// (sound when no `break` escapes the loop — the natural exit
    /// path is cond-false, so the cond's negation holds at the
    /// failing check, which IS the post-loop binding state). Mirrors
    /// `apply_then_narrows` but reads `else_*` fields; no
    /// `multi_typed` / contradiction interleaving (those are
    /// If-specific).
    fn apply_else_narrows(&mut self, narrows: &CondNarrows) {
        for ident in &narrows.else_non_null {
            if let Some(cur) = self.lookup_def_type(*ident) {
                let stripped = self.arena.strip_nullable(cur);
                self.write_narrow(*ident, stripped);
            }
        }
        for (ident, ty_ref) in &narrows.else_typed {
            let ty = self.lower_type_ref(*ty_ref);
            self.write_narrow(*ident, ty);
        }
        for (ident, ty_refs) in &narrows.else_typed_union {
            let ty = self.lower_typed_union(ty_refs);
            self.write_narrow(*ident, ty);
        }
        for (ident, ty) in &narrows.else_typed_id {
            self.write_narrow(*ident, *ty);
        }
        for path in &narrows.else_member_non_null {
            self.write_member_non_null(Cow::Borrowed(path));
        }
        for (path, ty_ref) in &narrows.else_member_typed {
            let ty = self.lower_type_ref(*ty_ref);
            self.write_member_typed(Cow::Borrowed(path), ty);
        }
        for ident in &narrows.else_null {
            let null_ty = self.null();
            self.write_narrow(*ident, null_ty);
        }
        for path in &narrows.else_member_null {
            let null_ty = self.null();
            self.write_member_typed(Cow::Borrowed(path), null_ty);
        }
    }

    // P19.16
    /// When an assignment's LHS is an `Ident` resolving
    /// to a Param/Local, narrow that binding to the RHS's type for
    /// the rest of the enclosing block. The `Stmt::If` post-pass
    /// then lifts narrows that hold along every path through the if.
    /// When the LHS is a member-access path (e.g.
    /// `this.matchingNormalisation = ...` or `arr[0]` with a literal
    /// index), record / clear the member-narrow for that path based on
    /// the RHS's nullability. Other LHS shapes (calls, dynamic-index
    /// offsets, etc.) don't have a stable identity and silently no-op.
    fn record_assign_narrow(&mut self, target: Idx<Expr>, value_ty: TypeId) {
        if let Expr::Ident { name: name_idx, .. } = &self.hir.exprs[target] {
            if let Some(Definition::Param(def) | Definition::Local(def)) =
                self.res.lookup(*name_idx)
            {
                self.write_narrow(def, value_ty);
            }
            return;
        }
        if matches!(
            self.hir.exprs[target],
            Expr::Member(_) | Expr::Arrow(_) | Expr::Offset(_)
        ) && let Some(path) = self.member_path(target)
        {
            // Re-assigning the path invalidates any prior `is`-narrowed type;
            // the new value's static type may be the supertype again.
            self.drop_member_typed(&path);
            if self.arena.get(value_ty).nullable {
                // RHS may be null → drop any prior non-null narrow
                // so subsequent reads see the declared (nullable)
                // type again.
                self.drop_member_non_null(&path);
            } else {
                self.write_member_non_null(Cow::Borrowed(&path));
            }
        }
    }

    // P19.21
    /// Narrow record for the `?=` (coalesce-assign)
    /// operator. Semantics: if LHS is null, assign RHS; otherwise
    /// leave LHS unchanged. The post-state is non-null when RHS is
    /// non-null (either LHS was already non-null, or we just wrote a
    /// non-null value). When RHS is itself nullable, we can't
    /// guarantee non-null after the op — but unlike `=` we also
    /// MUST NOT drop an existing non-null narrow, since `?=` only
    /// fires when LHS is null and a previously-non-null LHS stays
    /// non-null.
    fn record_coalesce_assign_narrow(&mut self, target: Idx<Expr>, value_ty: TypeId) {
        if self.arena.get(value_ty).nullable {
            // RHS nullable — `?=` may leave LHS null. Don't write a
            // narrow; don't drop one either.
            return;
        }
        if let Expr::Ident { name: name_idx, .. } = &self.hir.exprs[target] {
            if let Some(Definition::Param(def) | Definition::Local(def)) =
                self.res.lookup(*name_idx)
                && let Some(cur) = self.lookup_def_type(def)
            {
                // When the current narrow is the bottom `null` shape
                // (a prior `x == null` guard pinned it), stripping
                // `nullable` leaves a `Null` type with no values —
                // the post-`?=` reads then see `null` and downstream
                // typing falsely concludes the value is null. The
                // operator's semantics are "x = x ?? rhs"; when x is
                // null, the post-state is exactly `rhs`. So when the
                // current narrow is a `null` shape (with or without
                // the `nullable` flag), replace it with the RHS
                // wholesale instead of stripping nullability.
                let cur_kind = &self.arena.get(cur).kind;
                let cur_is_null_shape = matches!(cur_kind, TypeKind::Null);
                let new_ty = if cur_is_null_shape {
                    value_ty
                } else {
                    self.arena.strip_nullable(cur)
                };
                self.write_narrow(def, new_ty);
            }
            return;
        }
        if matches!(
            self.hir.exprs[target],
            Expr::Member(_) | Expr::Arrow(_) | Expr::Offset(_)
        ) && let Some(path) = self.member_path(target)
        {
            self.write_member_non_null(Cow::Owned(path));
        }
    }

    /// Resolve the deref-target type for an `Expr::Arrow` receiver:
    /// `n->field` desugars to `n.<deref_method>().field`, where the
    /// deref method is named by the `@deref("methodName")` annotation
    /// on the receiver's type declaration. The deref-target is the
    /// *return type* of that method, with the receiver's generic args
    /// substituted in.
    ///
    /// Returns `None` when:
    /// - the receiver's type has no `@deref` annotation,
    /// - the named method doesn't exist on the type, or
    /// - the receiver's type isn't a `Type` / `Generic` shape with
    ///   a discoverable name.
    ///
    /// Mirrors the compiler: `*n` / `n->m()` are syntactic sugar
    /// driven entirely by metadata on the type decl. There is no
    /// hard-coded list of "deref-able" types in the analyzer.
    fn arrow_deref_receiver(&mut self, recv_ty: TypeId) -> Option<TypeId> {
        // Pull the receiver's name + generic-arg instantiation. Two
        // shapes carry a discoverable type name:
        //   - `Type(decl)`           — non-generic concrete decl.
        //   - `Generic { decl, args }` — handle-keyed generic.
        let (type_id, instantiation): (ItemKey, &[TypeId]) = {
            let ty = self.arena.get(recv_ty);
            match &ty.kind {
                TypeKind::Type(d) => (*d, &[]),
                TypeKind::Generic { tpl, args } => (*tpl, args.as_slice()),
                _ => return None,
            }
        };
        // Single lookup: signature-lowering's
        // `populate_deref_caches` already resolved the `@deref`
        // method's return TypeId (chain-walked through supertypes if
        // needed) and stashed it on `TypeMembers::deref_return_ty`.
        // The cached `TypeId` is in abstract `GenericParam(T, …)`
        // form — substitute the receiver's instantiation here.
        let members = self.index.type_members.get(&type_id)?;
        let method_ret = members.deref_return_ty?;
        if instantiation.is_empty() {
            return Some(method_ret);
        }
        let mut subst: FxHashMap<Symbol, TypeId> = FxHashMap::default();
        for (i, gp_sym) in members.generics.iter().enumerate() {
            if let Some(arg) = instantiation.get(i) {
                subst.insert(*gp_sym, *arg);
            }
        }
        Some(self.arena.substitute(method_ret, &subst))
    }

    /// Member resolution: bind the property ident in `a.b` /
    /// `a->b` to the matching `TypeAttr` or method `Decl` whenever the
    /// receiver's type names a `TypeDecl` declared in this module.
    /// Anonymous types and primitives stay no-binding; cross-module
    /// receivers consult [`crate::index::ProjectIndex::type_members`]
    /// directly and write into `foreign_member_uses` inline
    /// no `deferred_member_uses` deferral.
    /// `instance_access` is `true` for `recv.prop` / `recv->prop` (Member /
    /// Arrow) and `false` for `Type::prop` (Static). Instance access
    /// skips `static` methods so a `static fn from(...)` declared on the
    /// same type as an inherited `from: time` attr doesn't shadow the
    /// attr — the runtime resolves `this.from` to the attr, not the
    /// static method.
    fn resolve_member_with(
        &mut self,
        recv_ty: TypeId,
        property: Idx<Ident>,
        instance_access: bool,
    ) {
        let ty = self.arena.get(recv_ty);
        // Enums have no instance members in GreyCat — fields are
        // queried via `Enum::field` (static_expr). Two cases:
        //   - instance access (`e.x` / `e->x`) → always wrong; the
        //     diagnostic points the user at the static_expr form.
        //   - static access (`E::x`) → valid only when `x` is a
        //     declared field; non-field names error here. (The
        //     Static arm in `infer_expr` separately type-checks the
        //     field lookup and produces the enum's `TypeId` for
        //     value-position use; this diag is purely the rejection
        //     side.)
        if let TypeKind::Enum { variants, .. } = &ty.kind {
            let prop_sym = self.hir.idents[property].symbol;
            if !instance_access && variants.contains(&prop_sym) {
                return;
            }
            let prop_text = self.ident_text(property);
            let prop_range = self.hir.idents[property].byte_range.clone();
            let recv_display = crate::display::display_type(
                self.arena,
                self.decl_registry,
                &self.index.symbols,
                recv_ty,
            );
            if instance_access {
                self.out.diagnostics.push(SemanticDiagnostic::structural(
                    Severity::Error,
                    "enum-no-instance-members",
                    format!(
                        "enum `{recv_display}` has no instance members; access fields via `{recv_display}::{prop_text}`"
                    ),
                    prop_range,
                ));
            } else {
                self.out.diagnostics.push(SemanticDiagnostic::structural(
                    Severity::Error,
                    "unknown-enum-field",
                    format!("enum `{recv_display}` has no field `{prop_text}`"),
                    prop_range,
                ));
            }
            return;
        }
        let type_id: Option<ItemKey> = match &ty.kind {
            TypeKind::Type(d) => Some(*d),
            TypeKind::Generic { tpl, .. } => Some(*tpl),
            // `Any` / `Unresolved` / `GenericParam` / `Lambda` / `Null`
            // / `Never` / `Union` — receivers where we either can't
            // know the member set (any is the escape hatch by design,
            // unresolved already errored upstream) or where a member
            // access doesn't apply. Silent so we don't pile false
            // positives onto unrelated upstream errors.
            _ => None,
        };
        let Some(recv_id) = type_id else {
            return;
        };
        let prop_sym = self.hir.idents[property].symbol;

        // Resolution order: attrs first (local then inherited), methods
        // second (local then inherited). Attrs always win over methods
        // of the same name — the runtime resolves `this.from` to an
        // inherited `from: time` attr even when a `static fn from(...)`
        // is declared on the receiver type.
        //
        // `out.type_decls` is per-module so it's keyed by the local
        // name `Symbol`; only the cross-module `type_members` map
        // needs the full `ItemKey`.
        let local_type_decl = self
            .out
            .type_decls
            .get(&recv_id.name)
            .copied()
            .and_then(|id| match &self.hir.decls[id] {
                Decl::Type(td) => Some(td),
                _ => None,
            });

        if let Some(type_decl) = local_type_decl {
            for attr_id in &type_decl.attrs {
                let attr = &self.hir.type_attrs[*attr_id];
                if self.hir.idents[attr.name].symbol == prop_sym {
                    self.out
                        .member_uses
                        .insert(property, MemberDef::Attr(*attr_id));
                    return;
                }
            }
        }
        if let Some((uri, attr_id)) = self.index.type_attr_id_chain(recv_id, prop_sym) {
            self.out.foreign_member_uses.insert(
                property,
                ForeignMember {
                    uri,
                    member: MemberDef::Attr(attr_id),
                },
            );
            return;
        }
        if let Some(type_decl) = local_type_decl {
            for method_id in &type_decl.methods {
                let Decl::Fn(m) = &self.hir.decls[*method_id] else {
                    continue;
                };
                if instance_access && m.modifiers.static_ {
                    continue;
                }
                if self.hir.idents[m.name].symbol == prop_sym {
                    self.out
                        .member_uses
                        .insert(property, MemberDef::Method(*method_id));
                    return;
                }
            }
        }
        let method_lookup = if instance_access {
            self.index.type_instance_method_id_chain(recv_id, prop_sym)
        } else {
            self.index.type_method_id_chain(recv_id, prop_sym)
        };
        if let Some((uri, method_id)) = method_lookup {
            self.out.foreign_member_uses.insert(
                property,
                ForeignMember {
                    uri,
                    member: MemberDef::Method(method_id),
                },
            );
            return;
        }
        // All four lookups exhausted. Before erroring, gate on
        // "we actually know this type's full member set" — if neither
        // the local module nor the project index has it, the type's
        // body hasn't been loaded (e.g. stdlib not present in a
        // single-file test harness, or a built-in name like `node`
        // whose body lives in `lib/std/core.gcl` that isn't on disk).
        // Claiming "no member" in that state would be a false positive.
        // Real CLI / LSP runs always load stdlib, so this gate only
        // silences synthetic-test cases.
        let known_to_project =
            local_type_decl.is_some() || self.index.type_members.contains_key(&recv_id);
        if !known_to_project {
            return;
        }
        // `display_type` renders generics (`node<String>` not `node`)
        // for clearer messages.
        let recv_display = crate::display::display_type(
            self.arena,
            self.decl_registry,
            &self.index.symbols,
            recv_ty,
        );
        let prop_text = self.ident_text(property);
        let prop_range = self.hir.idents[property].byte_range.clone();
        let (code, kind) = if instance_access {
            ("unknown-member", "member")
        } else {
            ("unknown-static-member", "static member")
        };
        self.out.diagnostics.push(SemanticDiagnostic::structural(
            Severity::Error,
            code,
            format!("type `{recv_display}` has no {kind} `{prop_text}`"),
            prop_range,
        ));
    }

    fn resolve_member(&mut self, recv_ty: TypeId, property: Idx<Ident>) {
        self.resolve_member_with(recv_ty, property, true);
    }

    /// Check the LHS of an `=` / `?=` for assignment to a `private`
    /// attr. GreyCat's `private` on an attr is read-public / write-
    /// private — direct assignment `obj.attr = x` is allowed only from
    /// inside the owning type's body via either the constructor
    /// (`Foo { attr: 1 }`, an `Expr::Object`, never reaches this
    /// function) or a **non-static** instance method of the owning
    /// type or any subtype that inherits the attr. Static methods of
    /// the owning type, and any code outside the owner's hierarchy,
    /// must use the constructor.
    fn check_private_attr_write(&mut self, lhs: Idx<Expr>) {
        let property = match &self.hir.exprs[lhs] {
            Expr::Member(m) => m.property.ident(),
            Expr::Arrow(m) => m.property.ident(),
            _ => return,
        };
        // Local attr — read `private` directly off the resolved HIR.
        let is_private_local = self
            .out
            .member_uses
            .get(&property)
            .and_then(|m| match m {
                MemberDef::Attr(attr_id) => Some(*attr_id),
                _ => None,
            })
            .is_some_and(|attr_id| self.hir.type_attrs[attr_id].modifiers.private);
        // Foreign attr — chain-walk `private_attrs` from the receiver's
        // ItemKey. The cross-module decl's HIR isn't directly reachable
        // from here; the per-type `private_attrs` set is the bridge.
        let recv_id = (|| {
            let recv = match &self.hir.exprs[lhs] {
                Expr::Member(m) => m.receiver,
                Expr::Arrow(m) => m.receiver,
                _ => return None,
            };
            let recv_ty = self.out.expr_types.get(&recv).copied()?;
            match &self.arena.get(recv_ty).kind {
                TypeKind::Type(d) => Some(*d),
                TypeKind::Generic { tpl, .. } => Some(*tpl),
                _ => None,
            }
        })();
        let prop_sym = self.hir.idents[property].symbol;
        let private_owner: Option<ItemKey> = recv_id.and_then(|id| {
            let (owner_id, members) = self.index.walk_chain_for_private_attr(id, prop_sym)?;
            members
                .private_attrs
                .contains(&prop_sym)
                .then_some(owner_id)
        });
        let is_private_foreign = self
            .out
            .foreign_member_uses
            .get(&property)
            .is_some_and(|f| matches!(f.member, MemberDef::Attr(_)))
            && private_owner.is_some();
        if !is_private_local && !is_private_foreign {
            return;
        }
        // Allow writes that originate inside the owning type's body
        // (non-static instance method of the owner or any subtype that
        // inherits the attr). `inside_static_fn` excludes `static fn`
        // bodies even when lexically inside the owning type. `this_stack`
        // being empty means a top-level fn — never inside any type body.
        if !self.inside_static_fn
            && let Some(enc_ty) = self.this_stack.last().copied()
            && let Some(enc_id) = match &self.arena.get(enc_ty).kind {
                TypeKind::Type(d) => Some(*d),
                TypeKind::Generic { tpl, .. } => Some(*tpl),
                _ => None,
            }
            && let Some(owner_id) = private_owner
            && self.index.type_is_descendant_or_self(enc_id, owner_id)
        {
            return;
        }
        let prop_text = self.ident_text(property);
        let span = self.hir.idents[property].byte_range.clone();
        self.out.diagnostics.push(SemanticDiagnostic::structural(
            Severity::Error,
            "private-attr-write",
            format!(
                "attribute `{prop_text}` is `private`; only the owner type's constructor or its \
                 (non-static) methods can write to it",
            ),
            span,
        ));
    }

    /// Hard error for direct assignment to a `static` attribute through
    /// a `Type::name` (or `module::Type::name`) path expression. GreyCat's
    /// runtime forbids `Counter::count = 42` / `Counter::count ?= 42`
    /// regardless of where the assignment lives. Reads of static attrs
    /// (`let x = Counter::count;`) are unaffected — `static_value_type`
    /// already types those at the attr's declared type.
    ///
    /// Counterpart to `check_private_attr_write`: that one covers
    /// instance-shape LHS (`Expr::Member` / `Expr::Arrow`); this one
    /// covers static-shape LHS (`Expr::Static` / `Expr::QualifiedStatic`).
    fn check_static_attr_write(&mut self, lhs: Idx<Expr>) {
        // Extract the property ident and (for the foreign-attr path)
        // enough info to recover the owner `ItemKey`. `Expr::Static`
        // carries a `TypeRef` we have to lower; `Expr::QualifiedStatic`
        // with a 3-segment chain carries the owner directly as
        // `module::Type` in the first two segments.
        let (property, owner_via_typeref) = match &self.hir.exprs[lhs] {
            Expr::Static(s) => (s.property.ident(), Some(s.ty)),
            Expr::QualifiedStatic { chain, .. } if chain.len() == 3 => {
                (*chain.last().expect("len==3"), None)
            }
            _ => return,
        };
        // In-module attr — `member_uses` carries the resolved `AttrId`
        // when the type lives in this module. Read `static_` straight
        // off the HIR.
        let is_static_local = self
            .out
            .member_uses
            .get(&property)
            .and_then(|m| match m {
                MemberDef::Attr(attr_id) => Some(*attr_id),
                _ => None,
            })
            .is_some_and(|attr_id| self.hir.type_attrs[attr_id].modifiers.static_);
        // Cross-module attr — consult the project index's `static_attrs`
        // set for the receiver's `ItemKey`. Two shapes feed the lookup:
        //  - `Expr::Static`: lower the receiver `TypeRef` to a `TypeId`,
        //    then map to `ItemKey` the same way `static_value_type` does.
        //  - `Expr::QualifiedStatic` (chain==3): the first two segments
        //    are `module::Type` — build the `ItemKey` directly.
        let is_static_foreign = (|| {
            let foreign = self.out.foreign_member_uses.get(&property)?;
            if !matches!(foreign.member, MemberDef::Attr(_)) {
                return None;
            }
            let prop_sym = self.hir.idents[property].symbol;
            let owner_id: ItemKey = if let Some(ty_ref) = owner_via_typeref {
                let recv_ty = self.lower_type_ref(ty_ref);
                match &self.arena.get(recv_ty).kind {
                    TypeKind::Type(d) => *d,
                    TypeKind::Generic { tpl, .. } => *tpl,
                    _ => return None,
                }
            } else {
                let Expr::QualifiedStatic { chain, .. } = &self.hir.exprs[lhs] else {
                    return None;
                };
                let module_sym = self.hir.idents[chain[0]].symbol;
                let type_sym = self.hir.idents[chain[1]].symbol;
                ItemKey::new(module_sym, type_sym)
            };
            let members = self.index.type_members.get(&owner_id)?;
            members.static_attrs.contains(&prop_sym).then_some(())
        })()
        .is_some();
        if !is_static_local && !is_static_foreign {
            return;
        }
        let prop_text = self.ident_text(property);
        let span = self.hir.idents[property].byte_range.clone();
        self.out.diagnostics.push(SemanticDiagnostic::structural(
            Severity::Error,
            "static-attr-assign",
            format!("attribute `{prop_text}` is `static`; static attributes cannot be assigned"),
            span,
        ));
    }

    /// Inline call-return typing for Member / Arrow /
    /// Static callees. Looks up the method's pre-lowered return
    /// `TypeId` in `index.type_members[type_name].method_returns` and
    /// applies `arena.substitute` against the receiver's
    /// instantiation. Returns `None` for callees this path doesn't
    /// handle (Ident / QualifiedStatic / Lambda / etc.) so the
    /// caller falls back to `any` until those branches land.
    fn try_member_call_typing(&mut self, callee: Idx<Expr>) -> Option<TypeId> {
        enum CalleeShape<'a> {
            Member {
                receiver: Idx<Expr>,
                property: Idx<Ident>,
                is_arrow: bool,
            },
            Static {
                ty: Idx<TypeRef>,
                property: Idx<Ident>,
            },
            Ident(Idx<Ident>),
            QualifiedStatic(&'a [Idx<Ident>]),
        }

        // Pull the small Copy / cheaply-borrowed bits out of the HIR
        // expression up front so we can drop the `&self.hir.exprs`
        // borrow before the recursive `&mut self` calls.
        let dispatch: CalleeShape = match &self.hir.exprs[callee] {
            Expr::Member(m) => CalleeShape::Member {
                receiver: m.receiver,
                property: m.property.ident(),
                is_arrow: false,
            },
            Expr::Arrow(m) => CalleeShape::Member {
                receiver: m.receiver,
                property: m.property.ident(),
                is_arrow: true,
            },
            Expr::Static(s) => CalleeShape::Static {
                ty: s.ty,
                property: s.property.ident(),
            },
            Expr::Ident { name, .. } => CalleeShape::Ident(*name),
            Expr::QualifiedStatic { chain, .. } => CalleeShape::QualifiedStatic(chain),
            _ => return None,
        };
        match dispatch {
            CalleeShape::Member {
                receiver,
                property,
                is_arrow,
            } => {
                let recv_ty = self.out.expr_types.get(&receiver).copied()?;
                // The receiver-nullability lift is applied
                // uniformly at the `Expr::Call` funnel
                // (`lift_call_result_for_nullable_receiver`) so the
                // generic-inference path and this one stay in lockstep;
                // return the bare method return type here.
                self.method_return_for(recv_ty, property, is_arrow)
            }
            CalleeShape::Static { ty, property } => {
                let recv_ty = self.lower_type_ref(ty);
                self.method_return_for(recv_ty, property, false)
            }
            CalleeShape::Ident(name_idx) => self.bare_fn_return(name_idx),
            CalleeShape::QualifiedStatic(chain) => self.qualified_static_call_return(chain),
        }
    }

    /// Lift a call expression's result type to nullable when the
    /// callee is an instance-member access (`recv.m()` / `recv->m()`)
    /// whose receiver type is nullable. The call shorts to null (or
    /// NPEs) when the receiver is null, so `recvExpr?.m()` and
    /// `recvExpr.m()` where `recvExpr: T?` both yield `Ret?` — the same
    /// lift the Member / Offset arms of `infer_expr` apply for value
    /// access.
    ///
    /// This is the single funnel for the call-result lift: it runs
    /// once at the `Expr::Call` site over whichever path computed the
    /// raw return type (generic inference, the non-generic
    /// member/static call path, or the lambda fallback), so a method
    /// whose return type is the receiver's own generic param (routed
    /// through `run_method_generic_inference`) lifts identically to a
    /// method with a concrete return.
    ///
    /// Static (`Type::m()`), qualified-static (`mod::Type::m()`),
    /// bare-fn and lambda-by-ident callees have no instance receiver,
    /// so the type passes through unchanged.
    fn lift_call_result_for_nullable_receiver(&mut self, callee: Idx<Expr>, ret: TypeId) -> TypeId {
        let receiver = match &self.hir.exprs[callee] {
            Expr::Member(m) | Expr::Arrow(m) => m.receiver,
            _ => return ret,
        };
        // The raw receiver's nullability drives the lift — for arrow
        // calls that's the node ref itself (`n->m()` shorts when `n` is
        // null), not the deref'd inner type.
        let recv_nullable = self
            .out
            .expr_types
            .get(&receiver)
            .is_some_and(|t| self.arena.get(*t).nullable);
        if recv_nullable {
            self.arena.nullable(ret)
        } else {
            ret
        }
    }

    /// Type a bare-Ident call (`foo()` / `module_fn()`) by
    /// looking up the fn's signature. Local fns lower the return
    /// `TypeRef` inline; cross-module fns consult the project
    /// signatures index. Generic fns aren't handled here — they
    /// route through [`Self::try_generic_call_inference`] which the
    /// caller tries first.
    ///
    /// The resolver's `Definition::Decl` binding takes precedence over
    /// the name-keyed `index.fn_signature_for` lookup: two modules
    /// can each declare `private fn process(...)` with different
    /// signatures, and `fn_signatures` is keyed by name with
    /// first-decl-wins — so falling back to it for a local binding
    /// would silently pick the wrong module's return type. Native fns
    /// have no `Definition::Decl` (they're `Definition::Project`), so
    /// the `fn_signatures` lookup is still reached for them.
    fn bare_fn_return(&mut self, name_idx: Idx<Ident>) -> Option<TypeId> {
        let def = self.res.lookup(name_idx)?;
        if let Definition::Decl(decl_id) = &def {
            let Decl::Fn(fnd) = &self.hir.decls[*decl_id] else {
                return None;
            };
            if !fnd.generics.is_empty() {
                return None;
            }
            let ret = fnd.return_type?;
            return Some(self.lower_type_ref(ret));
        }
        let fn_sym = self.hir.idents[name_idx].symbol;
        // Cross-module — the resolver already picked the specific
        // module that owns this fn, so we can construct the ItemKey
        // directly without re-walking the candidate set.
        if let Definition::ProjectDecl {
            uri: ref dec_uri, ..
        } = def
        {
            let fn_id = self.index.item_id_for(dec_uri, fn_sym)?;
            return self
                .index
                .fn_signatures
                .get(&fn_id)
                .and_then(|s| s.return_ty);
        }
        // `Definition::Project` fallback (no specific owning module):
        // walk non-private fn-ns candidates, first match wins.
        for (uri, decl) in self.index.locate_decl_in_ns(fn_sym, Namespace::Fn) {
            if self.index.is_decl_private(uri, decl) {
                continue;
            }
            let Some(fn_id) = self.index.item_id_for(uri, fn_sym) else {
                continue;
            };
            if let Some(sig) = self.index.fn_signatures.get(&fn_id) {
                return sig.return_ty;
            }
        }
        None
    }

    /// Type a `QualifiedStatic` callee. Two shapes:
    /// - `module::fn(...)` — chain has 2 segments. Look up
    ///   `chain[1]` in `index.fn_signatures`.
    /// - `module::Type::method(...)` — chain has 3 segments. Look
    ///   up `chain[1]` as a type, then `chain[2]` as one of its
    ///   methods.
    fn qualified_static_call_return(&mut self, chain: &[Idx<Ident>]) -> Option<TypeId> {
        match chain.len() {
            2 => {
                // `module::fn(...)` — `(chain[0], chain[1])` is the
                // fn's full `ItemKey` since module symbols are
                // project-wide unique.
                let fn_id = ItemKey::new(
                    self.hir.idents[chain[0]].symbol,
                    self.hir.idents[chain[1]].symbol,
                );
                let sig = self.index.fn_signatures.get(&fn_id)?;
                sig.return_ty
            }
            3 => {
                // `module::Type::method` — chain[0] is the module
                // symbol, chain[1] the type name, chain[2] the method
                // name. The receiver's `ItemKey` is just the (module,
                // type) pair since module symbols are unique project-
                // wide.
                let recv_id = ItemKey::new(
                    self.hir.idents[chain[0]].symbol,
                    self.hir.idents[chain[1]].symbol,
                );
                let method_sym = self.hir.idents[chain[2]].symbol;
                self.index.type_method_return_chain(recv_id, method_sym)
            }
            _ => None,
        }
    }

    /// Shared body of [`Self::try_member_call_typing`]: given the
    /// receiver's `TypeId` and the property `Ident`, look up the
    /// method's pre-lowered return type in the project signatures
    /// index, then substitute the receiver's generic args. Auto-derefs
    /// node-tag receivers when `is_arrow` (mirrors `arrow_deref_receiver`).
    fn method_return_for(
        &mut self,
        recv_ty: TypeId,
        property: Idx<Ident>,
        is_arrow: bool,
    ) -> Option<TypeId> {
        // Auto-deref node-tag receivers for arrow callees.
        let lookup_ty = if is_arrow {
            self.arrow_deref_receiver(recv_ty).unwrap_or(recv_ty)
        } else {
            recv_ty
        };
        let recv = self.arena.get(lookup_ty).clone();
        let (type_id, instantiation): (ItemKey, &[TypeId]) = match &recv.kind {
            // Handle-keyed variants already carry the decl's
            // full `(module, name)` identity.
            TypeKind::Type(decl) => (*decl, &[]),
            TypeKind::Generic { tpl, args } => (*tpl, args.as_slice()),
            _ => return None,
        };
        // Chain-walking lookup so methods declared on
        // a parent type resolve through a `Sub` receiver.
        let property_sym = self.hir.idents[property].symbol;
        let ret_ty = self.index.type_method_return_chain(type_id, property_sym)?;
        let members = self.index.type_members.get(&type_id)?;
        let mut subst: FxHashMap<Symbol, TypeId> = FxHashMap::default();
        for (i, gp_sym) in members.generics.iter().enumerate() {
            if let Some(arg) = instantiation.get(i) {
                subst.insert(*gp_sym, *arg);
            }
        }
        Some(self.arena.substitute(ret_ty, &subst))
    }

    /// Populate `foreign_decl_uses[chain[1]]` (the type
    /// segment) and `foreign_member_uses[chain[2]]` (the member
    /// segment, when present) for a `module::Type[::member]`
    /// QualifiedStatic. Lets hover / goto-def render the right thing
    /// on each chain segment without depending on the deleted pass
    /// 3.5 chain-segment writeback.
    fn bind_qualified_chain_segments(&mut self, chain: &[Idx<Ident>]) {
        if chain.len() < 2 {
            return;
        }
        let (host_uri, host_decl_id) = match self
            .index
            .locate_decl(self.hir.idents[chain[1]].symbol)
            .first()
        {
            Some(d) => (&d.uri, d.id),
            None => return,
        };
        self.out.foreign_decl_uses.insert(
            chain[1],
            ForeignDecl {
                uri: host_uri.clone(),
                decl: host_decl_id,
            },
        );
        if chain.len() == 3 {
            // Resolve the (uri, member) pair. The receiver's `ItemKey`
            // is (chain[0]=module, chain[1]=type) directly — module
            // symbols are project-wide unique.
            let type_id = ItemKey::new(
                self.hir.idents[chain[0]].symbol,
                self.hir.idents[chain[1]].symbol,
            );
            let resolved = self.index.type_members.get(&type_id).and_then(|members| {
                let prop = self.hir.idents[chain[2]].symbol;
                if let Some(attr_id) = members.attrs.get(&prop) {
                    Some((&members.home_uri, MemberDef::Attr(*attr_id)))
                } else {
                    members
                        .methods
                        .get(&prop)
                        .map(|decl_id| (&members.home_uri, MemberDef::Method(*decl_id)))
                }
            });
            if let Some((uri, member)) = resolved {
                self.out.foreign_member_uses.insert(
                    chain[2],
                    ForeignMember {
                        uri: uri.clone(),
                        member,
                    },
                );
            }
        }
    }

    // P23
    /// Type a `Type::name` / `Type::method` value-position
    /// Static expr. distinguishes static-attr value
    /// access (`type Foo { static path: String }` then `Foo::path`
    /// → `String`) from a non-static `Type::attr` reference (which
    /// is a runtime `field` handle). For methods, returns the
    /// runtime `function` named-type.
    ///
    /// In-module attrs read the `static_` modifier directly off
    /// the HIR's `TypeAttr`; cross-module attrs consult the
    /// project-wide `static_attrs` set populated at `ingest` time
    /// (the analyzer never crosses module boundaries during the
    /// body walk).
    fn static_value_type(&mut self, recv_ty: TypeId, property: Idx<Ident>) -> Option<TypeId> {
        let prop_sym = self.hir.idents[property].symbol;
        if let Some(MemberDef::Attr(attr_id)) = self.out.member_uses.get(&property).copied() {
            let attr = &self.hir.type_attrs[attr_id];
            if attr.modifiers.static_ {
                if let Some(tr) = attr.ty {
                    return Some(self.lower_type_ref(tr));
                }
                return Some(self.any_nullable());
            }
            return Some(self.field_ty());
        }
        if let Some(foreign) = self.out.foreign_member_uses.get(&property)
            && matches!(foreign.member, MemberDef::Attr(_))
        {
            // Cross-module attr — consult `static_attrs` for the
            // receiver's type. The receiver is the `Type::` part
            // of the static expr; we have its lowered TypeId.
            let owner_id: Option<ItemKey> = match &self.arena.get(recv_ty).kind {
                TypeKind::Type(d) => Some(*d),
                TypeKind::Generic { tpl, .. } => Some(*tpl),
                // P32 — enums are an ItemKey-keyed entry too. Their
                // `TypeKind::Enum.name` is just the bare name Symbol,
                // so we'd need the home module to mint the ItemKey.
                // Static-attr access doesn't apply to enums (their
                // members are variants, not attrs), so this branch
                // shouldn't fire for `Enum`-shaped receivers in
                // practice — return `None` to skip the static-attr
                // path and fall through to `self.field_ty()` below.
                TypeKind::Enum { .. } => None,
                // **P19.14** — primitives carry static methods /
                // attrs in stdlib (`time::max`, `int::max`, etc.);
                _ => None,
            };
            if let Some(id) = owner_id
                && let Some(members) = self.index.type_members.get(&id)
                && members.static_attrs.contains(&prop_sym)
            {
                if let Some(ty) = members.attr_types.get(&prop_sym).copied() {
                    return Some(ty);
                }
                return Some(self.any_nullable());
            }
            return Some(self.field_ty());
        }
        // Method reference (in-module or cross-module).
        let kind = self.out.member_uses.get(&property).copied().or_else(|| {
            self.out
                .foreign_member_uses
                .get(&property)
                .map(|f| f.member)
        })?;
        Some(match kind {
            MemberDef::Attr(_) => self.field_ty(),
            MemberDef::Method(_) => self.method_ref_ty(recv_ty, prop_sym),
        })
    }

    /// Mint the value-position type for a method reference. Static
    /// methods get a structural Lambda (built from the type's
    /// `method_signatures`); non-static methods keep the opaque
    /// `function` (instance-method value refs are caught separately by
    /// the `instance-method-value-ref` diagnostic in the validation
    /// phase — the type minted here doesn't affect that walk).
    fn method_ref_ty(&mut self, recv_ty: TypeId, method_sym: Symbol) -> TypeId {
        let owner_id: Option<ItemKey> = match &self.arena.get(recv_ty).kind {
            TypeKind::Type(d) => Some(*d),
            TypeKind::Generic { tpl, .. } => Some(*tpl),
            _ => None,
        };
        let sig_opt = owner_id
            .and_then(|id| self.index.type_members.get(&id))
            .and_then(|m| {
                m.static_methods
                    .contains(&method_sym)
                    .then(|| m.method_signatures.get(&method_sym).cloned())
                    .flatten()
            });
        match sig_opt {
            Some(sig) => self.fn_ref_ty_from_sig(&sig),
            None => self.function_ty(),
        }
    }

    /// Type a `module::name` / `module::Type::name`
    /// value-position QualifiedStatic expr. Two shapes:
    /// - 2-segment chain (`module::name`) — fn name resolves via
    ///   the project fn signatures index → `function`. Type name
    ///   resolves → `type`.
    /// - 3-segment chain (`module::Type::name`) — same as
    ///   `static_value_type` but routed through the cross-module
    ///   index. Attr → `field`, Method → `function`.
    fn qualified_static_value_type(&mut self, chain: &[Idx<Ident>]) -> Option<TypeId> {
        match chain.len() {
            2 => {
                // `module::name` — module symbol + item name → `ItemKey`.
                let module_sym = self.hir.idents[chain[0]].symbol;
                let name_sym = self.hir.idents[chain[1]].symbol;
                let item = ItemKey::new(module_sym, name_sym);
                if let Some(sig) = self.index.fn_signatures.get(&item) {
                    Some(self.fn_ref_ty_from_sig(sig))
                } else if self.index.fn_names.contains(&name_sym) {
                    Some(self.function_ty())
                } else if let Some(enum_id) = self.index.enum_types.get(&item).copied() {
                    // P-typeof — `module::EnumName` in value position
                    // is a type-literal value. Refine to `TypeOf(...)`
                    // so call-site inference can witness `T := EnumName`
                    // for `typeof T` parameters (e.g. `type::enum_by_name`).
                    Some(self.arena.type_of(enum_id))
                } else if self.index.type_members.contains_key(&item) {
                    // P-typeof — same rule for `module::TypeName`.
                    let inner = self.arena.alloc_type(item);
                    Some(self.arena.type_of(inner))
                } else if let Some(var_ty) = self.index.var_types.get(&item).copied() {
                    Some(var_ty)
                } else if self.index.has_name(name_sym) {
                    // Runtime-internal type the user can't author —
                    // keep the unrefined `type` shape so existing
                    // behavior for these stays put.
                    Some(self.type_ty())
                } else {
                    None
                }
            }
            3 => {
                // `module::Type::member` — module symbol + type name
                // → receiver's `ItemKey`.
                let module_sym = self.hir.idents[chain[0]].symbol;
                let type_sym = self.hir.idents[chain[1]].symbol;
                let type_id = ItemKey::new(module_sym, type_sym);
                let member_sym = self.hir.idents[chain[2]].symbol;
                // Enum variant: `module::Foo::a` types as `Foo` (the
                // enum), matching the analyzer's `Static` enum-variant
                // arm so call-arg validation against `_: Foo` passes.
                if let Some(ty_id) = self.index.enum_types.get(&type_id).copied()
                    && let TypeKind::Enum { variants, .. } = &self.arena.get(ty_id).kind
                    && variants.contains(&member_sym)
                {
                    return Some(ty_id);
                }
                let members = self.index.type_members.get(&type_id)?;
                if members.methods.contains_key(&member_sym) {
                    // Static method mints a structural Lambda from
                    // `method_signatures`; instance methods stay opaque
                    // — the `instance-method-value-ref` validation
                    // walk handles the value-position-error case.
                    if members.static_methods.contains(&member_sym)
                        && let Some(sig) = members.method_signatures.get(&member_sym).cloned()
                    {
                        Some(self.fn_ref_ty_from_sig(&sig))
                    } else {
                        Some(self.function_ty())
                    }
                } else if members.attrs.contains_key(&member_sym) {
                    // Static-attr value access from a
                    // `module::Type::name` chain. Returns the
                    // attr's declared type for static attrs;
                    // `field` handle otherwise.
                    if members.static_attrs.contains(&member_sym) {
                        Some(
                            members
                                .attr_types
                                .get(&member_sym)
                                .copied()
                                .unwrap_or_else(|| self.any_nullable()),
                        )
                    } else {
                        Some(self.field_ty())
                    }
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Type a `foreign_member_uses`-bound `recv.attr` /
    /// `recv.method()` shape inline by looking up the project
    /// signatures index. `recv_ty` is the resolution-side receiver
    /// (post-arrow-deref); the returned type already has the
    /// receiver's generic instantiation substituted in. Methods
    /// resolve to the `function` named-type the rest of the analyzer
    /// expects for method references.
    fn foreign_member_type(&mut self, recv_ty: TypeId, property: Idx<Ident>) -> Option<TypeId> {
        let foreign = self.out.foreign_member_uses.get(&property)?;
        // Always model method references as `function` — the actual
        // return-type substitution happens at the call site (P22's
        // call-typing path consults `method_returns` directly).
        if matches!(foreign.member, MemberDef::Method(_)) {
            return Some(self.function_ty());
        }
        // Attr — extract receiver shape (need owned name + args because
        // the arena entry borrow has to drop before we re-borrow as
        // mutable for `substitute`).
        // Own the args (Copy `TypeId`s) so the immutable arena borrow
        // drops before `substituted_attr_ty_chain` re-borrows `&mut self`.
        let (type_id, instantiation): (ItemKey, Vec<TypeId>) = match &self.arena.get(recv_ty).kind {
            // Handle-keyed variants carry the full identity.
            TypeKind::Type(d) => (*d, Vec::new()),
            TypeKind::Generic { tpl, args } => (*tpl, args.to_vec()),
            _ => return None,
        };
        // Chain-walking lookup so attrs declared on a parent type
        // (`type Sub extends Super { ... }`) resolve when accessed
        // through a `Sub` receiver, substituting the type parameter at
        // each `extends Base<X>` hop.
        let property_sym = self.hir.idents[property].symbol;
        self.substituted_attr_ty_chain(type_id, &instantiation, property_sym)
    }

    /// Resolve an attribute's declared type through the supertype chain,
    /// substituting each hop's generic arguments via the shared
    /// [`ProjectIndex::supertype_levels`] walk. Stops at the first level
    /// that declares `attr_name`, so an attr inherited from `extends
    /// Box<int>` resolves `T` to `int` even when the receiver type is
    /// itself non-generic.
    fn substituted_attr_ty_chain(
        &mut self,
        type_id: ItemKey,
        instantiation: &[TypeId],
        attr_name: Symbol,
    ) -> Option<TypeId> {
        let index = self.index;
        let levels = index.supertype_levels(self.arena, type_id, instantiation);
        for (level, subst) in &levels {
            if let Some(raw_ty) = index
                .type_members
                .get(level)
                .and_then(|m| m.attr_types.get(&attr_name).copied())
            {
                return Some(if subst.is_empty() {
                    raw_ty
                } else {
                    self.arena.substitute(raw_ty, subst)
                });
            }
        }
        None
    }

    // Lower a syntactic TypeRef to a TypeId.
    fn lower_type_ref(&mut self, idx: Idx<TypeRef>) -> TypeId {
        let mut env = CxLowerEnv {
            hir: self.hir,
            index: self.index,
            decl_registry: self.decl_registry,
            module_uri: self.module_uri,
            out_registry: &self.out.registry,
            type_decls: &self.out.type_decls,
            generics_in_scope: &self.generics_in_scope,
            inside_static_fn: self.inside_static_fn,
            static_generic_uses: &mut self.static_generic_uses,
        };
        lower_type_ref::lower_type_ref_with(&mut env, self.arena, idx)
    }

    /// `true` when `ty`'s head decl is the std-core `Map` — the one
    /// named-construction head whose `{ k: v }` entries are key/value
    /// expressions rather than field names. Handle-keyed via
    /// `arena.builtins.map_key`, so a user-declared `type Map` doesn't match.
    fn head_decl_is_map(&self, ty: TypeId) -> bool {
        let map_type = self.arena.builtins.map_key;
        match &self.arena.get(ty).kind {
            TypeKind::Generic { tpl, .. } => *tpl == map_type,
            TypeKind::Type(decl) => *decl == map_type,
            _ => false,
        }
    }

    /// Object-expr completeness check: `Foo {}` / `Foo { a: 1 }`
    /// against `type Foo { a: int; c: String? }` should require `a`
    /// (non-nullable) and skip `c` (nullable — runtime auto-initializes
    /// to `null`). Static attrs are skipped: they aren't part of
    /// instantiation, and they're the only attrs that can carry an
    /// initializer in GreyCat anyway.
    ///
    /// Walks the supertype chain (cross-module) so a `type Sub extends
    /// Super` inherits `Super`'s required attrs. Driven by
    /// [`ProjectIndex::type_members`] so foreign types from
    /// `@library` / `@include` modules participate the same way as
    /// local ones — the previous module-local path silently skipped
    /// any object expression whose head lived in `std` or any included
    /// module.
    ///
    /// Skipped when:
    /// - the type ref is qualified (`b::Foo {}`) — the qualified leaf
    ///   resolves through a different path that isn't wired in here yet;
    /// - the head name isn't in [`ProjectIndex::type_members`] (so it's
    ///   a primitive / runtime-generic / unresolved typo — nothing to
    ///   check against).
    ///
    /// Positional construction never reaches here — it's a separate HIR
    /// variant (`Expr::PositionalObject`) with its own validator.
    fn check_object_required_attrs(
        &mut self,
        type_ref: Idx<TypeRef>,
        head_ty: TypeId,
        fields: &[ObjectField],
    ) {
        // Construction soundness is a function of the head's member shape
        // alone — provenance (same-module / `@library` / `@include`,
        // bare or `mod::`-qualified) is irrelevant. Take the head decl
        // from the *settled* object type, which `lower_type_ref` already
        // resolved uniformly for every spelling, and key the project-wide
        // `type_members` index off it. (The old code resolved a bare
        // same-module name and bailed on any qualifier, so it silently
        // skipped the check for every cross-module / qualified head.)
        let head_id = match &self.arena.get(head_ty).kind {
            TypeKind::Generic { tpl, .. } => *tpl,
            TypeKind::Type(decl) => *decl,
            // Unresolved / primitive head — no member shape to check.
            _ => return,
        };
        if !self.index.type_members.contains_key(&head_id) {
            return;
        }
        let tr = &self.hir.type_refs[type_ref];
        let head_sym = self.hir.idents[tr.name].symbol;
        // Positional construction is a separate HIR variant
        // (`Expr::PositionalObject`) and never reaches this named-only
        // validator. Each field's key is the field name — a bare ident
        // or a quoted string; any other key shape is invalid on a
        // (non-`Map`) user type and is flagged below.
        let supplied: FxHashSet<Symbol> = fields
            .iter()
            .filter_map(|f| {
                object_field_key_name(self.hir, &self.index.symbols, f.name).map(|(sym, _)| sym)
            })
            .collect();

        // Walk Sub → Super → SuperSuper via `members.supertype`,
        // collecting:
        // - `missing`: required (non-nullable, non-static) attrs that
        //   weren't supplied.
        // - `known`: every instance attr the user *could* legitimately
        //   supply — drives the converse unknown-field check below.
        // Cap depth as a cheap cycle guard — the analyzer rejects
        // cycles elsewhere, but the walk here mustn't loop forever
        // even if one slips through.
        let mut missing: Vec<Symbol> = Vec::new();
        let mut known: FxHashSet<Symbol> = FxHashSet::default();
        // Tracks where each known attr is declared so the IDE-side
        // `object_field_uses` binding below can point at the right
        // declaring type (inherited attrs live on a supertype, which
        // may be in a different module).
        let mut declarers: FxHashMap<Symbol, (ItemKey, Idx<TypeAttr>)> = FxHashMap::default();
        let mut seen: FxHashSet<ItemKey> = FxHashSet::default();
        let mut cursor = Some(head_id);
        let mut hops = 0usize;
        while let Some(cur_id) = cursor {
            if !seen.insert(cur_id) {
                break;
            }
            if hops > 32 {
                break;
            }
            hops += 1;
            let Some(members) = self.index.type_members.get(&cur_id) else {
                break;
            };
            for attr_sym in members.attr_order.iter() {
                if members.static_attrs.contains(attr_sym) {
                    continue;
                }
                known.insert(*attr_sym);
                if let Some(attr_idx) = members.attrs.get(attr_sym) {
                    // Child level wins on shadowed attr names.
                    declarers.entry(*attr_sym).or_insert((cur_id, *attr_idx));
                }
                let nullable = members
                    .attr_types
                    .get(attr_sym)
                    .is_some_and(|ty| self.arena.get(*ty).nullable);
                if nullable {
                    continue;
                }
                if supplied.contains(attr_sym) {
                    continue;
                }
                if !missing.iter().any(|m| m == attr_sym) {
                    missing.push(*attr_sym);
                }
            }
            cursor = members.supertype;
        }

        // Bind each known field-name ident to its declaring attr for
        // IDE consumers (hover, goto-def, rename). Unknown fields are
        // intentionally skipped — they get an `unknown-field`
        // diagnostic below and have no attr to point at.
        for f in fields {
            // IDE binding needs an `Idx<Ident>` to key on; only bare-ident
            // keys carry one (string-literal field names have no ident).
            let Some((attr_sym, Some(name_id))) =
                object_field_key_name(self.hir, &self.index.symbols, f.name)
            else {
                continue;
            };
            if let Some((declaring_type, attr_idx)) = declarers.get(&attr_sym) {
                self.out.object_field_uses.insert(
                    name_id,
                    ObjectFieldBinding {
                        declaring_type: *declaring_type,
                        attr: *attr_idx,
                    },
                );
            }
        }

        let type_name: &str = &self.index.symbols[head_sym];

        // Unknown-field check: any supplied name that isn't an
        // instance attr anywhere on the chain. Emit once per
        // occurrence so each red squiggly points at the actual
        // mistake (two `oops: 1, oops: 2` get two diagnostics).
        // Static attrs intentionally aren't in `known` — they can't
        // be assigned via object syntax, so naming one here is
        // unknown in the assignment sense.
        for f in fields {
            // A key that isn't an ident / quoted string can't name a
            // field. On a (non-`Map`) user type that's a malformed
            // construction — the runtime rejects it as "unresolved
            // field". Flag it at the key's span.
            let Some((attr_sym, _)) = object_field_key_name(self.hir, &self.index.symbols, f.name)
            else {
                self.diag(
                    Severity::Error,
                    "unknown-field",
                    format!(
                        "object field name must be an identifier or string literal naming an attribute of `{type_name}`"
                    ),
                    self.hir.exprs[f.name].byte_range(),
                );
                continue;
            };
            if known.contains(&attr_sym) {
                continue;
            }
            let attr_name: &str = &self.index.symbols[attr_sym];
            let span = self.hir.exprs[f.name].byte_range();
            self.diag(
                Severity::Error,
                "unknown-field",
                format!("unknown field `{attr_name}` on type `{type_name}`"),
                span,
            );
        }

        if missing.is_empty() {
            return;
        }
        let plural = if missing.len() > 1 { "s" } else { "" };
        let mut names = String::new();
        {
            use std::fmt::Write;
            for (i, name) in missing.into_iter().enumerate() {
                if i != 0 {
                    names.push_str(", ");
                }
                write!(&mut names, "`{}`", &self.index.symbols[name]).unwrap();
            }
        }
        self.diag(
            Severity::Error,
            "missing-required-fields",
            format!("missing required field{plural} for `{type_name}`: {names} (non-nullable)"),
            tr.byte_range.clone(),
        );
    }

    /// Call-site generic inference. Returns `Some(return_ty)`
    /// when `callee` resolves to a non-native fn decl with `generics`
    /// declared; the witnesses come from each `(declared_param,
    /// arg_ty)` pair via [`Self::collect_witnesses`]. Returns `None`
    /// for non-fn callees (lambdas, member calls, cross-module decls
    /// not yet wired into the analyzer's HIR cache, etc.) so the
    /// caller falls back to `any`.
    /// Returns `(materialized, runtime_erased)`: the analyzer's
    /// monomorphized return type, plus — when the callee erases its
    /// `T`-bearing container result at runtime ([`crate::erasure`]) —
    /// the all-`any?` shape the runtime actually produces. The second
    /// element is `Some` only for erasing callees; consumers record it
    /// in `expr_runtime_types` to drive the `generic-erasure` diagnostic.
    fn try_generic_call_inference(
        &mut self,
        callee: Idx<Expr>,
        arg_tys: &[TypeId],
        call_range: Range<usize>,
    ) -> Option<(TypeId, Option<TypeId>)> {
        // P-typeof-inference — Static / QualifiedStatic / Member /
        // Arrow callees route through the pre-lowered
        // `TypeMembers::method_signatures` storage. The witness +
        // substitute loop is the same as the bare-Ident path, only
        // the way we *find* the method signature differs.
        match &self.hir.exprs[callee] {
            Expr::Static(s) => {
                let recv_ty = self.lower_type_ref(s.ty);
                let property = s.property.ident();
                return self.run_method_generic_inference(recv_ty, property, arg_tys, &call_range);
            }
            Expr::Member(m) => {
                let property = m.property.ident();
                let recv_ty = self.out.expr_types.get(&m.receiver).copied()?;
                return self.run_method_generic_inference(recv_ty, property, arg_tys, &call_range);
            }
            Expr::Arrow(m) => {
                let property = m.property.ident();
                let recv_ty = self.out.expr_types.get(&m.receiver).copied()?;
                let recv_ty = self.arrow_deref_receiver(recv_ty).unwrap_or(recv_ty);
                return self.run_method_generic_inference(recv_ty, property, arg_tys, &call_range);
            }
            Expr::QualifiedStatic { chain, .. } if chain.len() == 3 => {
                let module_sym = self.hir.idents[chain[0]].symbol;
                let type_sym = self.hir.idents[chain[1]].symbol;
                let property = chain[2];
                let type_id_item = ItemKey::new(module_sym, type_sym);
                let recv_ty = self.arena.alloc_type(type_id_item);
                return self.run_method_generic_inference(recv_ty, property, arg_tys, &call_range);
            }
            // 2-segment QualifiedStatic shapes (`module::fn`) fall
            // through to the Definition-based lookup below; the
            // resolver binds them to `Definition::ProjectDecl` via the
            // leaf, and the existing arm picks up the FnSignature.
            _ => {}
        }
        // P19.8: peek without cloning the whole `Expr` — `name_idx`
        // is a `Copy` `Idx<Ident>`, no allocation.
        let name_idx = match &self.hir.exprs[callee] {
            Expr::Ident { name, .. } => *name,
            _ => return None,
        };
        let def = self.res.lookup(name_idx)?;
        match def {
            Definition::Decl(decl_id) => {
                // Pre-bind the fields we need from the FnDecl so we
                // can drop the `&self.hir.decls[..]` borrow before
                // the `&mut self` calls below. `params` / `generics`
                // are `Vec<Idx<_>>` — the clone copies indices, not
                // the underlying nodes.
                let (fn_name_idx, fn_generics, fn_params, fn_return_type, erases) =
                    match &self.hir.decls[decl_id] {
                        Decl::Fn(fnd) if !fnd.generics.is_empty() => (
                            fnd.name,
                            &fnd.generics,
                            &fnd.params,
                            fnd.return_type,
                            crate::erasure::fn_result_erases(self.hir, fnd),
                        ),
                        _ => return None,
                    };
                // Lower the declared signature with the fn's generics in scope.
                self.push_generic_scope(
                    fn_generics,
                    GenericOwner::Function(self.hir.idents[fn_name_idx].symbol),
                );
                let declared_params: Vec<TypeId> = fn_params
                    .iter()
                    .map(|p_id| {
                        self.hir.fn_params[*p_id]
                            .ty
                            .map(|t| self.lower_type_ref(t))
                            .unwrap_or_else(|| self.any_nullable())
                    })
                    .collect();
                let declared_return = fn_return_type
                    .map(|t| self.lower_type_ref(t))
                    .unwrap_or_else(|| self.any_nullable());
                self.pop_generic_scope();

                let generic_syms: Vec<Symbol> = fn_generics
                    .iter()
                    .map(|g| self.hir.idents[*g].symbol)
                    .collect();
                let mut tbl = InferenceTable::new();
                let pair_count = declared_params.len().min(arg_tys.len());
                for i in 0..pair_count {
                    self.collect_witnesses(declared_params[i], arg_tys[i], &mut tbl, &call_range);
                }
                Some(self.materialize_with_erasure(declared_return, &generic_syms, erases, &tbl))
            }
            Definition::ProjectDecl {
                uri: ref dec_uri, ..
            } => {
                // **P19.15** — cross-module generic call inference.
                // The S7-S11 stage pre-lowered every fn's params and
                // return type into the shared arena (`FnSignature`);
                // we can run the same witness-driven inference
                // without crossing the module boundary at body-walk
                // time. Without this, generic stdlib fns like
                // `abs<T>(x: T): T` typed every call as `T`
                // (GenericParam) and downstream arithmetic on the
                // result fell through to `any`.
                let fn_sym = self.hir.idents[name_idx].symbol;
                let fn_id = self.index.item_id_for(dec_uri, fn_sym)?;
                let sig = self.index.fn_signatures.get(&fn_id)?;
                if sig.generics.is_empty() {
                    return None;
                }
                // No declared return → no shape to infer into. The
                // outer Expr::Call default (`any?`) is the right answer.
                let declared_return = sig.return_ty?;
                let erases = sig.return_erases;
                let mut tbl = InferenceTable::new();
                for (param, arg) in sig.params.iter().zip(arg_tys.iter()) {
                    self.collect_witnesses(*param, *arg, &mut tbl, &call_range);
                }
                Some(self.materialize_with_erasure(declared_return, &sig.generics, erases, &tbl))
            }
            _ => None,
        }
    }

    /// Compute a generic call's materialized return (witnesses
    /// substituted) and, when the callee erases its `T`-bearing container
    /// result at runtime ([`crate::erasure`]), the all-`any?` erased
    /// return the runtime actually produces. The erased form is returned
    /// only when it genuinely differs from the materialized one.
    fn materialize_with_erasure(
        &mut self,
        declared_return: TypeId,
        generic_syms: &[Symbol],
        erases: bool,
        tbl: &InferenceTable,
    ) -> (TypeId, Option<TypeId>) {
        let materialized = tbl.substitute(self.arena, declared_return);
        if !erases {
            return (materialized, None);
        }
        let any_q = self.any_nullable();
        let erase_map: FxHashMap<Symbol, TypeId> =
            generic_syms.iter().map(|g| (*g, any_q)).collect();
        let erased = self.arena.substitute(declared_return, &erase_map);
        (materialized, (erased != materialized).then_some(erased))
    }

    /// Shared body of the Static / Member / Arrow / QualifiedStatic
    /// branches of [`Self::try_generic_call_inference`]. Looks up
    /// the method's pre-lowered signature in
    /// [`crate::index::TypeMembers::method_signatures`], composes
    /// the receiver-side instantiation substitution with the call-
    /// site witness collection, and substitutes the result through
    /// the declared return type.
    ///
    /// Returns `None` when:
    /// - the receiver type doesn't resolve to a known type decl
    ///   (primitive without member storage, unresolved name, etc.);
    /// - the method isn't in `method_signatures` (no generic
    ///   params — the non-generic `method_returns` path handles
    ///   that case in [`Self::try_member_call_typing`]).
    fn run_method_generic_inference(
        &mut self,
        recv_ty: TypeId,
        property: Idx<Ident>,
        arg_tys: &[TypeId],
        call_range: &Range<usize>,
    ) -> Option<(TypeId, Option<TypeId>)> {
        let recv = self.arena.get(recv_ty);
        let (type_id, instantiation): (ItemKey, &[TypeId]) = match &recv.kind {
            TypeKind::Type(decl) => (*decl, &[]),
            TypeKind::Generic { tpl, args } => (*tpl, args.as_slice()),
            _ => return None,
        };
        let method_sym = self.hir.idents[property].symbol;
        let members = self.index.type_members.get(&type_id)?;
        let sig = members.method_signatures.get(&method_sym)?;
        // Receiver-side substitution. Maps the type decl's own generic
        // params (`Array<T>.push(x: T)`) to the receiver's concrete
        // args (`Array<int>` → `T := int`) so the method's params /
        // return show through with the right type *before* call-site
        // witness collection runs against `arg_tys`.
        let recv_subst = if instantiation.is_empty() {
            None
        } else {
            Some(
                members
                    .generics
                    .iter()
                    .copied()
                    .zip(instantiation.iter().copied())
                    .collect::<FxHashMap<_, _>>(),
            )
        };
        // No declared return → no shape to infer into. Outer caller
        // falls back to `any?` at the Expr::Call default.
        let mut tbl = InferenceTable::new();
        for (declared_param, arg) in sig.params.iter().zip(arg_tys.iter()) {
            let declared_param = match &recv_subst {
                Some(subst) => self.arena.substitute(*declared_param, subst),
                None => *declared_param,
            };
            self.collect_witnesses(declared_param, *arg, &mut tbl, call_range);
        }

        let sig_return = sig.return_ty?;
        let declared_return = match &recv_subst {
            Some(subst) => self.arena.substitute(sig_return, subst),
            None => sig_return,
        };
        Some(self.materialize_with_erasure(declared_return, &sig.generics, sig.return_erases, &tbl))
    }

    /// Walk `param_ty` (declared) against `arg_ty` (witness). When
    /// `param_ty` is a [`TypeKind::GenericParam`], record `arg_ty` as
    /// the witness; if a different witness was already recorded for
    /// the same name, emit a `cannot infer T: A conflicts with B`
    /// diagnostic. Recursively descends into matching `Generic` /
    /// `Tuple` shapes so nested generic params get bound (e.g.
    /// `Array<T>` against `Array<int>` binds `T → int`).
    fn collect_witnesses(
        &mut self,
        param_ty: TypeId,
        arg_ty: TypeId,
        tbl: &mut InferenceTable,
        call_range: &Range<usize>,
    ) {
        let pk = self.arena.get(param_ty).clone();
        // P-typeof — `typeof T` parameter form. The declared shape is
        // `TypeOf(p_inner)`; the argument must itself be a type-
        // literal value (`TypeOf(a_inner)`) for the binding to make
        // sense. Recurse on the inners so `TypeOf(GenericParam(T))`
        // ↔ `TypeOf(<some decl>)` lands the `T := <some decl>`
        // witness. A non-`TypeOf` argument is a type error — the
        // assignability check at the call site will surface it; we
        // just skip witness recording so substitution doesn't bind
        // `T` to a non-type value.
        if let TypeKind::TypeOf(p_inner) = pk.kind
            && let TypeKind::TypeOf(a_inner) = self.arena.get(arg_ty).kind
        {
            self.collect_witnesses(p_inner, a_inner, tbl, call_range);
            return;
        }
        if let TypeKind::GenericParam(name) = &pk.kind {
            // If the param is `T?`, the witness is whatever the arg
            // strips down to without nullable.
            let witness = if pk.nullable {
                self.arena.strip_nullable(arg_ty)
            } else {
                arg_ty
            };
            if let Some(prior) = tbl.lookup(name) {
                if prior != witness {
                    let msg = format!(
                        "cannot infer `{}`: `{}` conflicts with `{}`",
                        &self.index.symbols[*name],
                        self.display(prior),
                        self.display(witness),
                    );
                    self.diag(
                        Severity::Error,
                        "generic-inference-conflict",
                        msg,
                        call_range.clone(),
                    );
                }
                return;
            }
            tbl.bind(*name, witness);
            return;
        }
        let ak = self.arena.get(arg_ty).clone();
        if let (TypeKind::Generic { tpl: pd, args: pa }, TypeKind::Generic { tpl: ad, args: aa }) =
            (&pk.kind, &ak.kind)
            && pd == ad
            && pa.len() == aa.len()
        {
            // `Tuple<T, U>` falls under this arm — `(x, y)` literal
            // typing routes through `arena.tuple(decl, x, y)` which
            // mints `Generic(tuple_decl, [x, y])`, same shape as
            // source-level `Tuple<T, U>` produces.
            let pa = pa.clone();
            let aa = aa.clone();
            for (p, a) in pa.iter().zip(aa.iter()) {
                self.collect_witnesses(*p, *a, tbl, call_range);
            }
        }
    }

    fn visit_decl(&mut self, decl_id: Idx<Decl>) {
        match &self.hir.decls[decl_id] {
            Decl::Fn(d) => self.visit_fn_decl(d),
            Decl::Type(d) => self.visit_type_decl(d),
            Decl::Enum(_) => {}
            Decl::Var(d) => self.visit_modvar(d),
            Decl::Pragma(p) => self.visit_pragma(p),
        }
    }

    fn visit_fn_decl(&mut self, d: &FnDecl) {
        // Register the fn's generic params into scope so
        // `lower_type_ref` mints `GenericParam` for each `T` mention
        // instead of falling back to `any`.
        let owner = GenericOwner::Function(self.hir.idents[d.name].symbol);
        self.push_generic_scope(&d.generics, owner);
        let prev_static = std::mem::replace(&mut self.inside_static_fn, d.modifiers.static_);
        // Bind parameter types into def_types so identifier inference
        // produces real types instead of `any`.
        for p_id in &d.params {
            let p = &self.hir.fn_params[*p_id];
            let ty =
                p.ty.map(|t| self.lower_type_ref(t))
                    .unwrap_or_else(|| self.any_nullable());
            self.out.def_types.insert(p.name, ty);
        }
        let return_ty = d
            .return_type
            .map(|t| self.lower_type_ref(t))
            .unwrap_or_else(|| self.any_nullable());
        if let Some(body) = d.body {
            self.visit_stmt(body, Some(return_ty));
        }
        self.inside_static_fn = prev_static;
        self.pop_generic_scope();
    }

    fn visit_type_decl(&mut self, d: &TypeDecl) {
        // Type-level generics are visible in attrs + method
        // signatures.
        let type_name_sym = self.hir.idents[d.name].symbol;
        let type_name = &self.index.symbols[type_name_sym];
        // Inheritance-depth check: the runtime caps `extends` chains
        // at MAX_INHERITANCE_DEPTH types (including the leaf). A
        // declaration past that limit fails to build with
        // "too depth inheritance: <name>". Surface it as a structural
        // error at the type's declaration site so the user sees the
        // problem before they hit `greycat build`.
        let chain_len = self
            .index
            .supertype_chain_length(ItemKey::new(self.module_sym, type_name_sym));
        if chain_len > ProjectIndex::MAX_INHERITANCE_DEPTH {
            let span = d
                .supertype
                .map(|tr| self.hir.type_refs[tr].byte_range.clone())
                .unwrap_or_else(|| self.hir.idents[d.name].byte_range.clone());
            self.diag(
                Severity::Error,
                "inheritance-too-deep",
                format!(
                    "inheritance chain too deep: `{type_name}` is {chain_len} levels deep; \
                     greycat allows at most {limit}",
                    limit = ProjectIndex::MAX_INHERITANCE_DEPTH,
                ),
                span,
            );
        }
        let owner = GenericOwner::Type(type_name_sym);
        self.push_generic_scope(&d.generics, owner);
        // Build the `this` TypeId. For non-generic
        // types it's `Named { name }`; for generic types it's
        // `Generic { name, args: [GenericParam(g0), GenericParam(g1), ...] }`.
        // Push it on `this_stack` so `LiteralKind::This` inside
        // method bodies returns the right thing. Done *after* the
        // generic scope is pushed so generics resolve.
        let this_ty = if d.generics.is_empty() {
            // Reuse whatever `register_module_types` minted
            // for this decl (a `Type(handle)` when the registry has
            // it, otherwise an `Unresolved` sink). Avoids re-minting
            // and keeps `this` and outside references pointing at
            // the same `TypeId`.
            self.out.registry.lookup(type_name_sym).unwrap_or_else(|| {
                let span = self.hir.idents[d.name].byte_range.clone();
                self.arena.unresolved(type_name_sym, (span.start, span.end))
            })
        } else {
            // Mint the generic `this` against the decl handle
            // when we have one (so it interns equal to whatever
            // `lower_type_ref*` produces for the same source-level
            // reference). When the registry hasn't seen this decl yet
            // (no entry in `register_module_types` / pre-ingest path),
            // fall back to `Unresolved` so downstream type-relations
            // stay quiet rather than cascade.
            match self
                .index
                .resolve_type(self.decl_registry, Some(self.module_uri), type_name_sym)
            {
                Some(tpl) => {
                    let args: Vec<TypeId> = d
                        .generics
                        .iter()
                        .map(|g| {
                            let g_sym = self.hir.idents[*g].symbol;
                            self.arena.generic_param(g_sym)
                        })
                        .collect();
                    self.arena.alloc_generic(tpl, args)
                }
                None => {
                    let span = self.hir.idents[d.name].byte_range.clone();
                    self.arena.unresolved(type_name_sym, (span.start, span.end))
                }
            }
        };
        self.this_stack.push(this_ty);
        for attr_id in &d.attrs {
            self.visit_type_attr(&self.hir.type_attrs[*attr_id]);
        }
        for method_id in &d.methods {
            if let Decl::Fn(fnd) = &self.hir.decls[*method_id] {
                self.visit_fn_decl(fnd);
            }
        }
        self.this_stack.pop();
        self.pop_generic_scope();
    }

    fn push_generic_scope(&mut self, generics: &[Idx<Ident>], owner: GenericOwner) {
        let mut frame = FxHashMap::default();
        for g in generics {
            frame.insert(self.hir.idents[*g].symbol, owner);
        }
        self.generics_in_scope.push(frame);
    }

    fn pop_generic_scope(&mut self) {
        self.generics_in_scope.pop();
    }

    fn visit_type_attr(&mut self, a: &TypeAttr) {
        // Type relations are checked in `ProjectAnalysis::validate_type_relations`
        // (post-pass). Doing them here surfaces false positives for
        // any cross-module Call return whose type isn't settled until
        // `infer_cross_module_call_types` runs.
        let _ = a.ty.map(|t| self.lower_type_ref(t));
        if let Some(init) = a.init {
            let _ = self.visit_expr(init);
        }
    }

    fn visit_modvar(&mut self, d: &ModVarDecl) {
        let declared = d.ty.map(|t| self.lower_type_ref(t));
        let init_ty = d.init.map(|i| self.visit_expr(i));
        // Record the modvar's type in `def_types` keyed by
        // its binding ident. Mirrors what `Stmt::Var` does for
        // locals; lets capability code (e.g. `receiver_type_at`'s
        // text-based fallback for ERROR-recovery cases) look up a
        // modvar's type by name without re-running `lower_type_ref`.
        let var_ty = declared.or(init_ty).unwrap_or_else(|| self.any_nullable());
        self.out.def_types.insert(d.name, var_ty);
    }

    fn visit_pragma(&mut self, p: &Pragma) {
        for a in &p.args {
            let _ = self.visit_expr(*a);
        }
    }

    /// Walk a `BlockStmt` body in its own narrow-frame. Body-bearing
    /// statements (`If::then_branch`, `While::body`, `Try::try_block`,
    /// …) hold their block inline post-refactor, so we can't go
    /// through `visit_stmt(Idx<Stmt>)` for them.
    fn visit_block(&mut self, block: &BlockStmt, return_ty: Option<TypeId>) {
        self.push_narrow();
        for s in &block.stmts {
            self.visit_stmt(*s, return_ty);
        }
        self.pop_narrow();
    }

    fn visit_stmt(&mut self, stmt_id: Idx<Stmt>, return_ty: Option<TypeId>) {
        match &self.hir.stmts[stmt_id] {
            Stmt::Block(b) => {
                self.push_narrow();
                for s in &b.stmts {
                    self.visit_stmt(*s, return_ty);
                }
                self.pop_narrow();
            }
            Stmt::Expr(e) => {
                let _ = self.visit_expr(*e);
            }
            Stmt::Var(LocalVar { name, ty, init, .. }) => {
                let declared = ty.map(|t| self.lower_type_ref(t));
                let init_ty = init.map(|i| self.visit_expr(i));
                // Type-relation diagnostic deferred to
                // `ProjectAnalysis::validate_type_relations`.
                let var_ty = declared.or(init_ty).unwrap_or_else(|| self.any_nullable());
                self.out.def_types.insert(*name, var_ty);
                // P-erasure taint: a var initialized from an erasing
                // generic call holds the erased value at runtime even
                // when annotated with a more specific type (the
                // annotation is cosmetic — the runtime tag stays erased). Carry the runtime
                // shape to the binding so later references flag the
                // narrowing uses the runtime would throw on.
                if let Some(i) = init
                    && let Some(rt) = self.out.expr_runtime_types.get(i).copied()
                {
                    self.out.def_runtime_types.insert(*name, rt);
                }
            }
            Stmt::Assign(AssignStmt { target, value, .. }) => {
                // Lowering currently produces `=` as `Expr::Binary`
                // wrapped in `Stmt::Expr`, so this arm is effectively
                // dead — kept for exhaustiveness and any future
                // grammar shape that may revive it. Narrow logic
                // lives in `Expr::Binary` (op = "=").
                let _ = self.visit_expr(*target);
                let value_ty = self.visit_expr(*value);
                self.record_assign_narrow(*target, value_ty);
            }
            Stmt::If(s) => self.visit_if_stmt(stmt_id, s, return_ty),
            Stmt::While(s) => self.visit_while_stmt(stmt_id, s, return_ty),
            Stmt::DoWhile(s) => self.visit_do_while_stmt(stmt_id, s, return_ty),
            Stmt::For(s) => self.visit_for_stmt(stmt_id, s, return_ty),
            Stmt::ForIn(s) => self.visit_for_in_stmt(stmt_id, s, return_ty),
            Stmt::Return(r) => {
                if let Some(v) = r.value {
                    let _ = self.visit_expr(v);
                    // Type-relation diagnostic deferred to
                    // `ProjectAnalysis::validate_type_relations`.
                    let _ = return_ty;
                }
            }
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Breakpoint(_) => {}
            Stmt::Throw(t) => {
                let _ = self.visit_expr(t.value);
            }
            Stmt::Try(TryStmt {
                try_block,
                catch_block,
                ..
            }) => {
                self.visit_block(try_block, return_ty);
                self.visit_block(catch_block, return_ty);
            }
            Stmt::At(AtStmt { expr, block, .. }) => {
                let _ = self.visit_expr(*expr);
                self.visit_block(block, return_ty);
            }
        }
    }

    fn visit_if_stmt(&mut self, stmt_id: Idx<Stmt>, s: &IfStmt, return_ty: Option<TypeId>) {
        self.visit_expr(s.condition);
        // exhaustiveness: only run from a "head" if (i.e.
        // not already accounted for as a nested else-if).
        if !self.chain_member_ifs.contains(&stmt_id) {
            self.check_enum_exhaustiveness(stmt_id, s.byte_range.clone());
        }

        let CondNarrows {
            then_non_null,
            else_non_null,
            then_typed,
            then_member_non_null,
            else_member_non_null,
            then_member_typed,
            else_typed,
            else_member_typed,
            then_typed_union,
            else_typed_union,
            mut then_typed_id,
            mut else_typed_id,
            then_null,
            else_null,
            then_member_null,
            else_member_null,
            is_atomic_is,
            then_enum_values,
            else_enum_values,
        } = self.derive_cond_narrows(s.condition);

        // Compute the else-side complement
        // narrow for atomic `is`-conditions. `derive_cond_narrows`
        // runs in `&self` context (`lower_type_ref` is `&mut self`),
        // so the complement is computed here in `Stmt::If`
        // where we have mutable access. Gated on
        // `is_atomic_is`: atomic for `Expr::Is` (P41) and for
        // pure `||`-chains of `is`-checks (P42.4); never set
        // through `&&` (an `&&` else might be either operand
        // failing — per-ident complements would be unsound).
        if is_atomic_is {
            for (ident, ty_ref) in &then_typed {
                let Some(known) = self.lookup_def_type(*ident) else {
                    continue;
                };
                let asserted = self.lower_type_ref(*ty_ref);
                if let Some(complement) = self.narrow_complement(known, asserted) {
                    else_typed_id.push((*ident, complement));
                }
            }
            for (ident, ty_ref) in &else_typed {
                let Some(known) = self.lookup_def_type(*ident) else {
                    continue;
                };
                let asserted = self.lower_type_ref(*ty_ref);
                if let Some(complement) = self.narrow_complement(known, asserted) {
                    then_typed_id.push((*ident, complement));
                }
            }
            // Disjunctive complement for
            // `(x is T1) || (x is T2)` shape (`then_typed_union`
            // carries the merged asserted list). Chain
            // `narrow_complement` over each asserted type, so
            // `closure(known) \ closure(T1) \ closure(T2)` falls
            // out for free via repeated subtraction. Mirror for
            // `else_typed_union` (populated by the `!` swap).
            for (ident, ty_refs) in &then_typed_union {
                if let Some(complement) = self.chain_narrow_complement(*ident, ty_refs) {
                    else_typed_id.push((*ident, complement));
                }
            }
            for (ident, ty_refs) in &else_typed_union {
                if let Some(complement) = self.chain_narrow_complement(*ident, ty_refs) {
                    then_typed_id.push((*ident, complement));
                }
            }
        }

        // `exhaustive-is-check`. When every concrete
        // runtime case of a binding's known type is covered
        // by the asserted type(s), the negative side of the
        // guard is unreachable. Mirrors `non-exhaustive` (for
        // enum chains); this is its inverse-shape sibling.
        // Emit at most one warning per condition and mark
        // `decidable_conditions` so the existing per-`is`
        // contradiction pass doesn't also fire "is already
        // of type …" on the same span.
        if is_atomic_is {
            // (known_disp, asserted_disp, is_negated). The
            // asserted display is the actionable info — for an
            // abstract `known` with a single concrete subtype,
            // it names the type the value is guaranteed to be.
            let mut hit: Option<(String, String, bool)> = None;
            for (ident, ty_ref) in &then_typed {
                if hit.is_some() {
                    break;
                }
                let Some(known) = self.lookup_def_type(*ident) else {
                    continue;
                };
                let asserted = self.lower_type_ref(*ty_ref);
                if self.narrow_is_exhausted(known, asserted) {
                    let known_disp = self.display(known).to_string();
                    let asserted_disp = self.display(asserted).to_string();
                    hit = Some((known_disp, asserted_disp, false));
                }
            }
            for (ident, ty_refs) in &then_typed_union {
                if hit.is_some() {
                    break;
                }
                let Some(known) = self.lookup_def_type(*ident) else {
                    continue;
                };
                let asserted = self.lower_typed_union(ty_refs);
                if self.narrow_is_exhausted(known, asserted) {
                    let known_disp = self.display(known).to_string();
                    let asserted_disp = self.display(asserted).to_string();
                    hit = Some((known_disp, asserted_disp, false));
                }
            }
            // Else-side (under `!` swap): exhaustion means the
            // *then* branch is unreachable.
            for (ident, ty_ref) in &else_typed {
                if hit.is_some() {
                    break;
                }
                let Some(known) = self.lookup_def_type(*ident) else {
                    continue;
                };
                let asserted = self.lower_type_ref(*ty_ref);
                if self.narrow_is_exhausted(known, asserted) {
                    let known_disp = self.display(known).to_string();
                    let asserted_disp = self.display(asserted).to_string();
                    hit = Some((known_disp, asserted_disp, true));
                }
            }
            for (ident, ty_refs) in &else_typed_union {
                if hit.is_some() {
                    break;
                }
                let Some(known) = self.lookup_def_type(*ident) else {
                    continue;
                };
                let asserted = self.lower_typed_union(ty_refs);
                if self.narrow_is_exhausted(known, asserted) {
                    let known_disp = self.display(known).to_string();
                    let asserted_disp = self.display(asserted).to_string();
                    hit = Some((known_disp, asserted_disp, true));
                }
            }
            if let Some((known_disp, asserted_disp, is_negated)) = hit {
                let dead = if is_negated { "then" } else { "else" };
                // Only emit when the dead branch is actually
                // present in source. When `is_negated = false`,
                // the dead branch is `else`; if the user didn't
                // write one, there's nothing redundant about the
                // type test — they're using it to gate the
                // then-body, no implicit reliance on an
                // unreachable else. When `is_negated = true`, the
                // dead branch is `then`, which the grammar
                // requires — always emit.
                if is_negated || s.else_branch.is_some() {
                    let msg = format!(
                        "every value of `{known_disp}` matches `{asserted_disp}`: the {dead}-branch is unreachable",
                    );
                    let range = self.hir.exprs[s.condition].byte_range();
                    self.diag(Severity::Warning, "exhaustive-is-check", msg, range);
                }
                // Mark decidable regardless of whether we
                // surfaced the warning — downstream `unreachable`
                // / contradiction passes consume this independently.
                // `is_negated = false` means the if-cond is always
                // true (then-branch always runs). `is_negated = true`
                // means the cond is `!(…always-true…)` → always
                // false (else-branch runs).
                self.out.decidable_conditions.insert(stmt_id, !is_negated);
            }
        }

        // Decide triviality BEFORE pushing any narrows — the
        // null-strip and `is`-narrow application below would
        // shadow the bindings' declared types and make every
        // null / type check look trivially decidable against
        // itself.
        let decidable = self.trivially_decidable(s.condition);
        if let Some(b) = decidable {
            if !self.condition_is_bool_literal(s.condition) {
                let range = self.hir.exprs[s.condition].byte_range();
                let msg = if b {
                    "condition is always true"
                } else {
                    "condition is always false"
                };
                self.surface_lint("decidable-condition", LintSeverity::Warning, msg, range);
            }
            self.out.decidable_conditions.insert(stmt_id, b);
        }

        self.push_narrow();
        for ident in &then_non_null {
            if let Some(cur) = self.lookup_def_type(*ident) {
                let stripped = self.arena.strip_nullable(cur);
                self.write_narrow(*ident, stripped);
            }
        }
        // Capture each `is`-narrowed binding's *pre-narrow*
        // declared type first — the contradiction pass needs
        // to compare the asserted `is`-type against the
        // binding's known type at the if-condition's eval
        // site. If we wrote the narrows first, `lookup_def_type`
        // would return the asserted type itself and every
        // single `is`-check would falsely look "always true".
        let mut multi_typed: FxHashMap<Idx<Ident>, (Option<TypeId>, Vec<TypeId>)> =
            FxHashMap::default();
        for (ident, ty_ref) in &then_typed {
            let ty = self.lower_type_ref(*ty_ref);
            let entry = multi_typed
                .entry(*ident)
                .or_insert_with(|| (self.lookup_def_type_for_is(*ident), Vec::new()));
            entry.1.push(ty);
        }
        for (ident, group) in &multi_typed {
            for ty in &group.1 {
                self.write_narrow(*ident, *ty);
            }
        }
        for (ident, ty_refs) in &then_typed_union {
            let ty = self.lower_typed_union(ty_refs);
            self.write_narrow(*ident, ty);
        }
        // Pre-lowered narrows (today populated only
        // via the `!` swap; the `Expr::Is` arm of
        // `derive_cond_narrows` populates `else_typed_id` in
        // P41.2).
        for (ident, ty) in &then_typed_id {
            self.write_narrow(*ident, *ty);
        }
        // Skip the `is`-contradiction pass when the condition
        // is already trivially decidable — it would re-emit a
        // duplicate "always (true|false)" diag.
        if decidable.is_none() {
            self.diagnose_then_typed_contradictions(&multi_typed, s.condition, stmt_id);
        }
        for path in &then_member_non_null {
            self.write_member_non_null(Cow::Borrowed(path));
        }
        for (path, ty_ref) in &then_member_typed {
            let ty = self.lower_type_ref(*ty_ref);
            self.write_member_typed(Cow::Borrowed(path), ty);
        }
        for ident in &then_null {
            let null_ty = self.null();
            self.write_narrow(*ident, null_ty);
        }
        for path in &then_member_null {
            let null_ty = self.null();
            self.write_member_typed(Cow::Borrowed(path), null_ty);
        }
        // Enum-value narrows: each entry constrains a binding
        // to a set of variants of one enum. `write_enum_value_narrow`
        // intersects with any outer narrow already on the
        // stack, so nested guards compose (e.g. inner `c ==
        // Red` inside outer `c == Red || c == Green` lands at
        // `{Red}`).
        for (ident, enum_sym, variants) in &then_enum_values {
            self.write_enum_value_narrow(*ident, *enum_sym, variants);
        }
        // Inline the then-branch's stmts (instead
        // of `visit_block`) so the narrow frame we just pushed
        // captures any assignments inside the branch. The
        // post-if join then sees those narrows. `visit_block`
        // would push+pop its own frame, discarding them.
        for s in &s.then_branch.stmts {
            self.visit_stmt(*s, return_ty);
        }
        let then_branch_narrows: FxHashMap<Idx<Ident>, TypeId> =
            self.narrows.last().cloned().unwrap_or_default();
        let then_branch_member_narrows: FxHashSet<String> =
            self.member_narrows.last().cloned().unwrap_or_default();
        self.pop_narrow();
        let then_terminates = block_terminates(self.hir, &s.then_branch);

        let (else_terminates, else_branch_narrows, else_branch_member_narrows) =
            if let Some(eb) = s.else_branch {
                self.push_narrow();
                for ident in &else_non_null {
                    if let Some(cur) = self.lookup_def_type(*ident) {
                        let stripped = self.arena.strip_nullable(cur);
                        self.write_narrow(*ident, stripped);
                    }
                }
                for path in &else_member_non_null {
                    self.write_member_non_null(Cow::Borrowed(path));
                }
                for (ident, ty_ref) in &else_typed {
                    let ty = self.lower_type_ref(*ty_ref);
                    self.write_narrow(*ident, ty);
                }
                for (ident, ty_refs) in &else_typed_union {
                    let ty = self.lower_typed_union(ty_refs);
                    self.write_narrow(*ident, ty);
                }
                // P41.1 — apply pre-lowered else-side narrows.
                for (ident, ty) in &else_typed_id {
                    self.write_narrow(*ident, *ty);
                }
                for (path, ty_ref) in &else_member_typed {
                    let ty = self.lower_type_ref(*ty_ref);
                    self.write_member_typed(Cow::Borrowed(path), ty);
                }
                for ident in &else_null {
                    let null_ty = self.null();
                    self.write_narrow(*ident, null_ty);
                }
                for path in &else_member_null {
                    let null_ty = self.null();
                    self.write_member_typed(Cow::Borrowed(path), null_ty);
                }
                for (ident, enum_sym, variants) in &else_enum_values {
                    self.write_enum_value_narrow(*ident, *enum_sym, variants);
                }
                // Same inline pattern for the else
                // branch. `eb` may be a Block or a nested If
                // (`else if`); for the Block case we inline,
                // for the If case we still call visit_stmt
                // (an If handles its own narrows internally).
                if let Stmt::Block(eb_block) = &self.hir.stmts[eb] {
                    for s in &eb_block.stmts {
                        self.visit_stmt(*s, return_ty);
                    }
                } else {
                    self.visit_stmt(eb, return_ty);
                }
                let captured: FxHashMap<Idx<Ident>, TypeId> =
                    self.narrows.last().cloned().unwrap_or_default();
                let captured_members: FxHashSet<String> =
                    self.member_narrows.last().cloned().unwrap_or_default();
                self.pop_narrow();
                (stmt_terminates(self.hir, eb), captured, captured_members)
            } else {
                (false, FxHashMap::default(), FxHashSet::default())
            };

        // CFG-aware narrowing — early return / throw etc.
        // If the then-branch always exits the surrounding flow
        // (return / throw / break / continue), the post-if
        // scope inherits the *else* condition's narrowing
        // (e.g. `if (x == null) { return; } use(x);` — `x` is
        // non-null after the if). Mirrored for the else side.
        if then_terminates {
            for ident in &else_non_null {
                if let Some(cur) = self.lookup_def_type(*ident) {
                    let stripped = self.arena.strip_nullable(cur);
                    self.write_narrow(*ident, stripped);
                }
            }
            for path in &else_member_non_null {
                self.write_member_non_null(Cow::Borrowed(path));
            }
            for (ident, ty_ref) in &else_typed {
                let ty = self.lower_type_ref(*ty_ref);
                self.write_narrow(*ident, ty);
            }
            for (ident, ty_refs) in &else_typed_union {
                let ty = self.lower_typed_union(ty_refs);
                self.write_narrow(*ident, ty);
            }
            // Load-bearing for the union-complement
            // shape: `if (x is T) { return; } use(x);` lifts
            // the complement into the post-if scope.
            for (ident, ty) in &else_typed_id {
                self.write_narrow(*ident, *ty);
            }
            for (path, ty_ref) in &else_member_typed {
                let ty = self.lower_type_ref(*ty_ref);
                self.write_member_typed(Cow::Borrowed(path), ty);
            }
            for ident in &else_null {
                let null_ty = self.null();
                self.write_narrow(*ident, null_ty);
            }
            for path in &else_member_null {
                let null_ty = self.null();
                self.write_member_typed(Cow::Borrowed(path), null_ty);
            }
            // Reassignments inside the else block must win
            // over the condition-entry narrows above — the
            // captured `else_branch_narrows` is the actual
            // end-of-else state. Applied last so an else-
            // branch `x = node<...>{...}` overrides the
            // condition's `else_null` lift for `x`.
            if !else_terminates {
                for (ident, ty) in &else_branch_narrows {
                    self.write_narrow(*ident, *ty);
                }
                for path in &else_branch_member_narrows {
                    self.write_member_non_null(Cow::Borrowed(path));
                }
            }
        }
        if else_terminates {
            for ident in &then_non_null {
                if let Some(cur) = self.lookup_def_type(*ident) {
                    let stripped = self.arena.strip_nullable(cur);
                    self.write_narrow(*ident, stripped);
                }
            }
            for (ident, ty_ref) in &then_typed {
                let ty = self.lower_type_ref(*ty_ref);
                self.write_narrow(*ident, ty);
            }
            for (ident, ty_refs) in &then_typed_union {
                let ty = self.lower_typed_union(ty_refs);
                self.write_narrow(*ident, ty);
            }
            // `if (!(x is T)) { return; } use(x);` —
            // the `!` swap pushes the complement to
            // `then_typed_id`, the else-terminates path lifts
            // it.
            for (ident, ty) in &then_typed_id {
                self.write_narrow(*ident, *ty);
            }
            for path in &then_member_non_null {
                self.write_member_non_null(Cow::Borrowed(path));
            }
            for (path, ty_ref) in &then_member_typed {
                let ty = self.lower_type_ref(*ty_ref);
                self.write_member_typed(Cow::Borrowed(path), ty);
            }
            for ident in &then_null {
                let null_ty = self.null();
                self.write_narrow(*ident, null_ty);
            }
            for path in &then_member_null {
                let null_ty = self.null();
                self.write_member_typed(Cow::Borrowed(path), null_ty);
            }
            // Mirror of the then_terminates path: the captured
            // then-branch state has reassignments that should
            // override the condition-entry narrows.
            if !then_terminates {
                for (ident, ty) in &then_branch_narrows {
                    self.write_narrow(*ident, *ty);
                }
                for path in &then_branch_member_narrows {
                    self.write_member_non_null(Cow::Borrowed(path));
                }
            }
        }

        // Post-if assignment-narrow lift. For
        // each binding that's nullable before the if and
        // is non-null along *every* path through the if,
        // narrow the post-if scope to its non-null form.
        //
        // Two source paths to consider:
        // - then path: non-null iff (condition implied non-null
        //   on then-side, captured in `then_non_null`) OR
        //   (the then-branch assigned a non-null value to it,
        //   captured in `then_branch_narrows`) OR
        //   (the then-branch terminates, in which case this
        //   path "doesn't reach" the post-if).
        // - else path (or implicit fall-through when no else):
        //   non-null iff (condition implied non-null on
        //   else-side, captured in `else_non_null`) OR
        //   (else-branch assigned a non-null value, captured
        //   in `else_branch_narrows`) OR (else terminates).
        //
        // The cleanest representation: for each candidate
        // binding, look up its post-then and post-else
        // effective type and check if both are non-null.
        if !then_terminates && !else_terminates {
            let mut candidates: FxHashSet<Idx<Ident>> = FxHashSet::default();
            candidates.extend(then_branch_narrows.keys().copied());
            candidates.extend(else_branch_narrows.keys().copied());
            candidates.extend(then_non_null.iter().copied());
            candidates.extend(else_non_null.iter().copied());
            for ident in candidates {
                let pre = match self.lookup_def_type(ident) {
                    Some(t) => t,
                    None => continue,
                };
                if !self.arena.get(pre).nullable {
                    // Already non-null — nothing to lift.
                    continue;
                }
                // Effective type at the end of the then-path.
                let then_eff = then_branch_narrows
                    .get(&ident)
                    .copied()
                    .or_else(|| {
                        if then_non_null.contains(&ident) {
                            Some(self.arena.strip_nullable(pre))
                        } else {
                            None
                        }
                    })
                    .unwrap_or(pre);
                // Effective type at the end of the else-path
                // (or implicit fall-through).
                let else_eff = else_branch_narrows
                    .get(&ident)
                    .copied()
                    .or_else(|| {
                        if else_non_null.contains(&ident) {
                            Some(self.arena.strip_nullable(pre))
                        } else {
                            None
                        }
                    })
                    .unwrap_or(pre);
                if !self.arena.get(then_eff).nullable && !self.arena.get(else_eff).nullable {
                    // Pick the merged narrow. Default: strip
                    // nullable off `pre` (works when pre is
                    // `T?` — both paths land at `T`).
                    //
                    // Edge case — `pre` was already narrowed
                    // to the literal `Null` shape by an outer
                    // guard (e.g. inside `if (u == null) {
                    // ...; if (u == null) { u = nonNull } ...
                    // }`). `strip_nullable(Null)` yields a
                    // dead `Null` kind with `nullable=false`
                    // — which passes the per-side non-null
                    // check above but, written as a narrow,
                    // makes downstream reads see `null`. In
                    // that case prefer a side's concrete
                    // narrow when one exists, else fall back
                    // to the binding's declared type stripped
                    // of nullability.
                    let pre_is_null_shape = matches!(self.arena.get(pre).kind, TypeKind::Null);
                    let then_is_null_shape =
                        matches!(self.arena.get(then_eff).kind, TypeKind::Null);
                    let else_is_null_shape =
                        matches!(self.arena.get(else_eff).kind, TypeKind::Null);
                    let merged = if pre_is_null_shape {
                        if !then_is_null_shape {
                            then_eff
                        } else if !else_is_null_shape {
                            else_eff
                        } else {
                            self.out
                                .def_types
                                .get(&ident)
                                .copied()
                                .map(|d| self.arena.strip_nullable(d))
                                .unwrap_or_else(|| self.arena.strip_nullable(pre))
                        }
                    } else {
                        self.arena.strip_nullable(pre)
                    };
                    self.write_narrow(ident, merged);
                } else if then_branch_narrows.contains_key(&ident)
                    || else_branch_narrows.contains_key(&ident)
                {
                    // A reaching branch reassigned this binding but the
                    // join is still nullable, so the non-null lift above
                    // didn't fire. Write the nullable join anyway: a
                    // stale pre-if narrow (e.g. an outer guard left it at
                    // `null`) must not survive the reassignment. The
                    // carrier is the concrete (non-`Null`) side; skip
                    // when both paths are the dead `Null` shape.
                    let then_null_shape = matches!(self.arena.get(then_eff).kind, TypeKind::Null);
                    let carrier = if then_null_shape { else_eff } else { then_eff };
                    if !matches!(self.arena.get(carrier).kind, TypeKind::Null) {
                        let merged = self.arena.nullable(carrier);
                        self.write_narrow(ident, merged);
                    }
                }
            }
            // Same lift for member-access paths.
            // A path is non-null post-if iff every reaching
            // branch made it non-null. Reaching condition:
            // (in then_branch_member_narrows OR
            //  in then_member_non_null) AND
            // (in else_branch_member_narrows OR
            //  in else_member_non_null).
            // No "no else" implicit fall-through case here:
            // we don't track which paths *were* non-null
            // outside the if, so we conservatively require
            // the else side to either exist and narrow, or
            // the condition's else_member side to imply it.
            let mut member_candidates: FxHashSet<&String> = FxHashSet::default();
            member_candidates.extend(then_branch_member_narrows.iter());
            member_candidates.extend(else_branch_member_narrows.iter());
            for path in &then_member_non_null {
                member_candidates.insert(path);
            }
            for path in &else_member_non_null {
                member_candidates.insert(path);
            }
            member_candidates
                .iter()
                .filter(|&&p| {
                    let then_ok =
                        then_branch_member_narrows.contains(p) || then_member_non_null.contains(p);
                    let else_ok = if s.else_branch.is_some() {
                        else_branch_member_narrows.contains(p) || else_member_non_null.contains(p)
                    } else {
                        // No else branch — fall-through is
                        // the implicit else. Only path-side
                        // narrows from the *condition's*
                        // else side (`x == null`) carry
                        // through the implicit fall-through.
                        else_member_non_null.contains(p)
                    };
                    then_ok && else_ok
                })
                .for_each(|path| self.write_member_non_null(Cow::Borrowed(path)));
        }
    }

    fn visit_while_stmt(&mut self, stmt_id: Idx<Stmt>, s: &WhileStmt, return_ty: Option<TypeId>) {
        self.visit_expr(s.condition);
        self.diagnose_decidable_loop_condition(stmt_id, s.condition, "while");
        // Apply the condition's truthy narrows to the body so
        // `while (x != null) { use(x) }` sees `x` as non-null
        // inside the body, mirroring `if (x != null)`.
        // Inline the body stmts (instead of `visit_block`) so
        // the loop's narrow frame is the innermost at body
        // entry — matches `Stmt::If`'s pattern.
        let narrows = self.derive_cond_narrows(s.condition);
        self.push_narrow();
        self.apply_then_narrows(&narrows);
        for s in &s.body.stmts {
            self.visit_stmt(*s, return_ty);
        }
        self.pop_narrow();
        // Post-loop else-narrow lift: if no `break` targets
        // this loop, the only exit is via cond-false, so the
        // cond's negation holds at the failing check — which
        // IS the post-loop binding state, since no code runs
        // between the failing check and the loop exit.
        // `apply_else_narrows` writes to the now-innermost
        // frame (the surrounding scope).
        if !block_breaks_current_loop(self.hir, &s.body) {
            self.apply_else_narrows(&narrows);
        }
    }

    fn visit_do_while_stmt(
        &mut self,
        _stmt_id: Idx<Stmt>,
        s: &DoWhileStmt,
        return_ty: Option<TypeId>,
    ) {
        // Inline the body's stmts inside a dedicated narrow
        // frame so reassignments inside the body (`id =
        // generate();`) survive long enough for the condition
        // to see them — `visit_block` would push + pop its
        // own frame and discard them, leaving the cond
        // staring at the pre-loop narrow.
        self.push_narrow();
        for s in &s.body.stmts {
            self.visit_stmt(*s, return_ty);
        }
        self.visit_expr(s.condition);
        // Body runs once regardless of the condition, so a
        // decidable `do-while` is informational only — emit
        // the diagnostic but do NOT record it for the
        // `unreachable` lint (no dead code to delete).
        if let Some(b) = self.trivially_decidable(s.condition)
            && !self.condition_is_bool_literal(s.condition)
        {
            let range = self.hir.exprs[s.condition].byte_range();
            let msg = if b {
                "do-while condition is always true"
            } else {
                "do-while condition is always false"
            };
            self.surface_lint("decidable-condition", LintSeverity::Warning, msg, range);
        }
        // Capture the cond's narrows before popping the body
        // frame — `derive_cond_narrows` reads the AST, not
        // the narrow stack, so the captured `CondNarrows`
        // outlives the pop unchanged.
        let narrows = self.derive_cond_narrows(s.condition);
        self.pop_narrow();
        // Post-loop else-narrow lift. Body always runs at
        // least once, so by the time we're past the loop the
        // cond *was* evaluated (and was false, since we're
        // past). No push/apply on the body side — that would
        // be unsound on iter 1, before the cond is checked.
        if !block_breaks_current_loop(self.hir, &s.body) {
            self.apply_else_narrows(&narrows);
        }
    }

    fn visit_for_stmt(&mut self, stmt_id: Idx<Stmt>, s: &ForStmt, return_ty: Option<TypeId>) {
        // Bind the C-style for loop's
        // `init_name` to its declared / inferred type so
        // uses of the loop var inside `condition` /
        // `increment` / `body` get a real type instead of
        // falling back to `any`. Order matters: visit the
        // init value FIRST (so its type is known), bind
        // `init_name` to declared-or-inferred, *then*
        // visit the rest.
        let init_value_ty = s.init_value.map(|v| self.visit_expr(v));
        if let Some(name) = s.init_name {
            let bound_ty = s
                .init_ty
                .map(|t| self.lower_type_ref(t))
                .or(init_value_ty)
                .unwrap_or_else(|| self.any_nullable());
            self.out.def_types.insert(name, bound_ty);
        }
        // Apply the condition's truthy narrows to body and
        // increment so `for (var p = init; p != null; p = next(p))`
        // sees `p` as non-null inside both the body and the
        // increment expression. Increment runs after body
        // inside the same frame, matching runtime order
        // (cond → body → incr → cond).
        let narrows = if let Some(c) = s.condition {
            self.visit_expr(c);
            self.diagnose_decidable_loop_condition(stmt_id, c, "for");
            self.derive_cond_narrows(c)
        } else {
            CondNarrows::default()
        };
        self.push_narrow();
        self.apply_then_narrows(&narrows);
        for s in &s.body.stmts {
            self.visit_stmt(*s, return_ty);
        }
        if let Some(i) = s.increment {
            let _ = self.visit_expr(i);
        }
        self.pop_narrow();
        // Post-loop else-narrow lift. Skip when there's no
        // condition (no narrow to derive) or when `break` can
        // escape (exit may not have been via cond-false).
        if s.condition.is_some() && !block_breaks_current_loop(self.hir, &s.body) {
            self.apply_else_narrows(&narrows);
        }
    }

    fn visit_for_in_stmt(&mut self, _stmt_id: Idx<Stmt>, s: &ForInStmt, return_ty: Option<TypeId>) {
        let range_ty = self.visit_expr(s.iterator);
        // Type the slice bounds so `from` / `to` get `expr_types`
        // entries and the resolver sees their ident uses.
        if let Some(w) = s.window {
            let _ = self.visit_expr(w);
        }
        // Bind each iterator param's def_type from
        // the iterable's element type. Iterability in
        // GreyCat is gated by the `@iterable` type-pragma;
        // in stdlib that covers six native types — Array,
        // Map, nodeList, nodeIndex, nodeTime, nodeGeo. All
        // six have stable `arena.builtins` decl handles, so the
        // dispatch is decl-handle identity (same pattern as
        // `TypeArena::is_node_tag` and the `Expr::Offset`
        // rewrite). Tuple shapes:
        //   - Array<T>     / nodeList<T>      -> (int,  T)
        //   - Map<K, V>    / nodeIndex<K, V>  -> (K,    V)
        //   - nodeTime<T>                     -> (time, T)
        //   - nodeGeo<T>                      -> (geo,  T)
        // Strict-null rule for unknown / missing parts:
        // keys fall back to `any` (non-null — runtime never
        // yields a null key during iteration); values fall
        // back to `any?` (could legitimately be nullable).
        let any_nn = self.any();
        let any_nl = self.any_nullable();
        let int_id = self.arena.builtins.int;
        let time_id = self.arena.builtins.time;
        let geo_id = self.arena.builtins.geo;
        // Receiver is nullable iterables propagate through
        // here too — `for (i, v in arr?)` is valid GreyCat.
        // Strip the optional before pattern-matching the
        // kind so the binding logic is the same with or
        // without the `?` marker.
        let underlying_ty = self.arena.strip_nullable(range_ty);
        // Both `Generic { tpl, args }` (e.g. `Array<int>`)
        // and bare `Type(decl)` (e.g. raw `Array` without
        // args — **P19.15**) carry the same decl handle.
        // Fold them so the dispatch is one pass with the
        // args slice empty in the bare case.
        let decl_and_args: Option<(ItemKey, &[TypeId])> = match &self.arena.get(underlying_ty).kind
        {
            TypeKind::Generic { tpl, args } => Some((*tpl, args.as_slice())),
            TypeKind::Type(decl) => Some((*decl, &[])),
            _ => None,
        };
        let (key_ty, val_ty) = if let Some((decl, args)) = decl_and_args {
            if decl == self.arena.builtins.array_key || self.arena.is_node_list(decl) {
                (int_id, args.first().copied().unwrap_or(any_nl))
            } else if decl == self.arena.builtins.map_key || self.arena.is_node_index(decl) {
                (
                    args.first().copied().unwrap_or(any_nn),
                    args.get(1).copied().unwrap_or(any_nl),
                )
            } else if self.arena.is_node_time(decl) {
                (time_id, args.first().copied().unwrap_or(any_nl))
            } else if self.arena.is_node_geo(decl) {
                (geo_id, args.first().copied().unwrap_or(any_nl))
            } else {
                // Not a known iterable. A follow-up chunk
                // can consult `TypeFlags.iterable` for
                // user-tagged `@iterable` decls once a
                // per-decl tuple shape is defined.
                (any_nn, any_nl)
            }
        } else {
            (any_nn, any_nl)
        };
        let inferred: Vec<TypeId> = if s.params.len() == 2 {
            vec![key_ty, val_ty]
        } else {
            // Defensive — grammar guarantees `>= 2`, but
            // keep slot 0 as the key (non-null) and the
            // rest as values (nullable). Old code returned
            // `any?` for all slots here, which was wrong
            // for slot 0 under strict-null semantics.
            let mut v = Vec::with_capacity(s.params.len());
            v.push(key_ty);
            for _ in 1..s.params.len() {
                v.push(any_nl);
            }
            v
        };
        for (p, inf_ty) in s.params.iter().zip(inferred.iter()) {
            let bound_ty = match p.ty {
                Some(t) => self.lower_type_ref(t),
                None => *inf_ty,
            };
            self.out.def_types.insert(p.name, bound_ty);
        }
        self.visit_block(&s.body, return_ty);
    }

    /// Narrowing analyzer for if-conditions.
    ///
    /// Recognizes `x != null` / `x == null` and `x is T`,
    /// plus conjunctive / disjunctive combinations:
    /// - `A && B` then-branch: union of both narrowings (both held).
    /// - `A || B` else-branch: union of both `else` narrowings (both
    ///   inverses held). Mixed forms can't safely narrow either side.
    fn derive_cond_narrows(&self, cond_id: Idx<Expr>) -> CondNarrows {
        let mut out = CondNarrows::default();
        match &self.hir.exprs[cond_id] {
            Expr::Binary(BinaryExpr {
                op, left, right, ..
            }) => match *op {
                BinOp::And => {
                    let l = self.derive_cond_narrows(*left);
                    let r = self.derive_cond_narrows(*right);
                    // Then: both A and B held — union both narrows.
                    out.then_non_null.extend(l.then_non_null);
                    out.then_non_null.extend(r.then_non_null);
                    out.then_typed.extend(l.then_typed);
                    out.then_typed.extend(r.then_typed);
                    out.then_typed_union.extend(l.then_typed_union);
                    out.then_typed_union.extend(r.then_typed_union);
                    out.then_member_non_null.extend(l.then_member_non_null);
                    out.then_member_non_null.extend(r.then_member_non_null);
                    out.then_member_typed.extend(l.then_member_typed);
                    out.then_member_typed.extend(r.then_member_typed);
                    // Pre-lowered narrows propagate the same way.
                    out.then_typed_id.extend(l.then_typed_id);
                    out.then_typed_id.extend(r.then_typed_id);
                    // Enum-value narrows: both subtrees' then-side
                    // entries propagate. The apply step intersects
                    // per (binding, enum), so two entries against the
                    // same binding land at their intersection.
                    out.then_enum_values.extend(l.then_enum_values);
                    out.then_enum_values.extend(r.then_enum_values);
                    // Else: at least one failed — can't narrow confidently.
                }
                BinOp::Or => {
                    let l = self.derive_cond_narrows(*left);
                    let r = self.derive_cond_narrows(*right);
                    // Else: NOT(A || B) ≡ !A AND !B — union else narrows.
                    out.else_non_null.extend(l.else_non_null);
                    out.else_non_null.extend(r.else_non_null);
                    out.else_member_non_null.extend(l.else_member_non_null);
                    out.else_member_non_null.extend(r.else_member_non_null);
                    // Pre-lowered else-narrows are sound to
                    // union for `||`: NOT(A || B) ≡ !A AND !B holds
                    // both subtrees' else-side complements.
                    out.else_typed_id.extend(l.else_typed_id);
                    out.else_typed_id.extend(r.else_typed_id);
                    // A `||` of two pure-`is` subtrees stays
                    // "atomic" for the purposes of the else-side
                    // complement: NOT(A || B) ≡ !A AND !B propagates
                    // both atomicity guarantees. Any `&&` underneath
                    // would have left its subtree's `is_atomic_is = false`
                    // (the `&&` arm never sets it), so the flag here
                    // only stays true for pure `||`-chains of `is`-checks.
                    out.is_atomic_is = l.is_atomic_is && r.is_atomic_is;
                    // Then: at least one side held. For `is`-narrows on
                    // the same ident across both sides (e.g.
                    // `x is int || x is float`), narrow `x` to the union
                    // of those types in the then-branch. Idents narrowed
                    // on only one side stay unnarrowed (we don't know
                    // which side held).
                    let mut lm: FxHashMap<Idx<Ident>, Vec<Idx<TypeRef>>> = FxHashMap::default();
                    for (id, ty) in l.then_typed {
                        lm.entry(id).or_default().push(ty);
                    }
                    for (id, tys) in l.then_typed_union {
                        lm.entry(id).or_default().extend(tys);
                    }
                    let mut rm: FxHashMap<Idx<Ident>, Vec<Idx<TypeRef>>> = FxHashMap::default();
                    for (id, ty) in r.then_typed {
                        rm.entry(id).or_default().push(ty);
                    }
                    for (id, tys) in r.then_typed_union {
                        rm.entry(id).or_default().extend(tys);
                    }
                    for (ident, mut ltys) in lm {
                        if let Some(rtys) = rm.get(&ident) {
                            ltys.extend(rtys.iter().copied());
                            out.then_typed_union.push((ident, ltys));
                        }
                    }
                    // Enum-value narrows under `||`. `(x == A) || (x ==
                    // B)` on then narrows x to `{A, B}` when both sides
                    // name the same binding+enum. Idents narrowed on
                    // only one side stay unnarrowed (could be true via
                    // either side; we don't know which set holds). The
                    // else side has NOT(A || B) ≡ !A AND !B — both
                    // subtrees' else_enum_values propagate.
                    let mut lm_ev: FxHashMap<(Idx<Ident>, Symbol), Vec<Symbol>> =
                        FxHashMap::default();
                    for (id, en, vs) in l.then_enum_values {
                        lm_ev.entry((id, en)).or_default().extend(vs);
                    }
                    let mut rm_ev: FxHashMap<(Idx<Ident>, Symbol), Vec<Symbol>> =
                        FxHashMap::default();
                    for (id, en, vs) in r.then_enum_values {
                        rm_ev.entry((id, en)).or_default().extend(vs);
                    }
                    for ((id, en), mut lvs) in lm_ev {
                        if let Some(rvs) = rm_ev.get(&(id, en)) {
                            lvs.extend(rvs.iter().copied());
                            out.then_enum_values.push((id, en, lvs));
                        }
                    }
                    out.else_enum_values.extend(l.else_enum_values);
                    out.else_enum_values.extend(r.else_enum_values);
                }
                BinOp::Eq | BinOp::Neq => {
                    // Ident-vs-null path
                    if let Some(name_idx) = self.ident_compared_to_null(*left, *right)
                        && let Some(Definition::Param(def) | Definition::Local(def)) =
                            self.res.lookup(name_idx)
                    {
                        match *op {
                            BinOp::Neq => {
                                out.then_non_null.push(def);
                                out.else_null.push(def);
                            }
                            BinOp::Eq => {
                                out.else_non_null.push(def);
                                out.then_null.push(def);
                            }
                            _ => {}
                        }
                        return out;
                    }
                    // `x == E::V` / `E::V == x` — singleton enum-value
                    // narrow. The match helper accepts either operand
                    // order and gates on `op ∈ {Eq, Neq}`. The else
                    // side of `==` (and the then side of `!=`) names
                    // the variant being *excluded*, but with no enum
                    // arity in scope we'd produce a "x ∈ E \ {V}" set
                    // we can't expand here. Skip the else-of-Eq /
                    // then-of-Neq populates for now — the
                    // exhaustiveness check only consults the *positive*
                    // side anyway.
                    if let Some((def, enum_sym, variant_sym)) = self.match_enum_cmp_syms(cond_id) {
                        match *op {
                            BinOp::Eq => {
                                out.then_enum_values
                                    .push((def, enum_sym, vec![variant_sym]));
                            }
                            BinOp::Neq => {
                                out.else_enum_values
                                    .push((def, enum_sym, vec![variant_sym]));
                            }
                            _ => {}
                        }
                    }
                    // Member-access path null comparison.
                    // `foo.bar != null` / `null != foo.bar` (and `==`)
                    // narrow the path on the matching side. Skips
                    // shapes that don't root in an Ident / `this` —
                    // those have no stable identity.
                    if let Some(path) = self.member_compared_to_null(*left, *right) {
                        match *op {
                            BinOp::Neq => {
                                out.then_member_non_null.push(path.clone());
                                out.else_member_null.push(path);
                            }
                            BinOp::Eq => {
                                out.else_member_non_null.push(path.clone());
                                out.then_member_null.push(path);
                            }
                            _ => {}
                        }
                    }
                    // Chained optional access (`a?->b?->c`) on one side
                    // of the comparison: if the comparison implies the
                    // chain is non-null on some branch, every `?->` /
                    // `?.` receiver in the chain must also be non-null
                    // on that branch (since `?->` short-circuits to null
                    // iff the receiver is null).
                    //   chain != null         → then: receivers non-null
                    //   null != chain         → then: receivers non-null
                    //   chain == null         → else: receivers non-null
                    //   null == chain         → else: receivers non-null
                    //   chain == <non-null>   → then: receivers non-null
                    //   <non-null> == chain   → then: receivers non-null
                    let is_null = |id: Idx<Expr>| matches!(&self.hir.exprs[id], Expr::Null { .. });
                    let is_chain = |id: Idx<Expr>| {
                        matches!(self.hir.exprs[id], Expr::Member(_) | Expr::Arrow(_))
                    };
                    let chain_other = if is_chain(*left) {
                        Some((*left, *right))
                    } else if is_chain(*right) {
                        Some((*right, *left))
                    } else {
                        None
                    };
                    if let Some((chain, other)) = chain_other {
                        let (then_side, else_side) = match *op {
                            BinOp::Neq if is_null(other) => (true, false),
                            BinOp::Eq if is_null(other) => (false, true),
                            BinOp::Eq if self.is_syntactically_non_null(other) => (true, false),
                            _ => (false, false),
                        };
                        if then_side {
                            self.collect_optional_chain_receivers(
                                chain,
                                &mut out.then_non_null,
                                &mut out.then_member_non_null,
                            );
                        }
                        if else_side {
                            self.collect_optional_chain_receivers(
                                chain,
                                &mut out.else_non_null,
                                &mut out.else_member_non_null,
                            );
                        }
                    }
                }
                _ => {}
            },
            // P6.5: `x is T` narrows x to T in the then-branch.
            // Also: `foo.bar is T` / `foo->bar is T` / `arr[0] is T` (with
            // a literal index) narrows the member path the same way
            // (record by path string).
            Expr::Is { value, ty, .. } => {
                if let Expr::Ident { name: name_idx, .. } = &self.hir.exprs[*value]
                    && let Some(Definition::Param(def) | Definition::Local(def)) =
                        self.res.lookup(*name_idx)
                {
                    out.then_typed.push((def, *ty));
                } else if matches!(
                    self.hir.exprs[*value],
                    Expr::Member(_) | Expr::Arrow(_) | Expr::Offset(_)
                ) && let Some(path) = self.member_path(*value)
                {
                    out.then_member_typed.push((path, *ty));
                }
                // P41.2 — mark the condition as atomic so `Stmt::If`
                // knows it's safe to compute the else-side complement.
                out.is_atomic_is = true;
            }
            // Strip parens before re-deriving.
            Expr::Paren(inner, _) => return self.derive_cond_narrows(*inner),
            // `!A` swaps then↔else. Note: `&&` / `||` already merge
            // safely, but a *raw* `!` on a conjunction can't generally
            // swap (De Morgan would turn it into `||`), so we only
            // swap atomic narrows. The common `if (!(x is T)) { throw }`
            // pattern is covered by the swap.
            Expr::Unary(UnaryExpr {
                op: UnaryOp::Not,
                operand,
                ..
            }) => {
                let inner = self.derive_cond_narrows(*operand);
                out.then_non_null = inner.else_non_null;
                out.else_non_null = inner.then_non_null;
                out.then_member_non_null = inner.else_member_non_null;
                out.else_member_non_null = inner.then_member_non_null;
                out.then_typed = inner.else_typed;
                out.else_typed = inner.then_typed;
                out.then_member_typed = inner.else_member_typed;
                out.else_member_typed = inner.then_member_typed;
                out.then_typed_union = inner.else_typed_union;
                out.else_typed_union = inner.then_typed_union;
                // P41.1 — swap pre-lowered narrows alongside the
                // existing pairs.
                out.then_typed_id = inner.else_typed_id;
                out.else_typed_id = inner.then_typed_id;
                // Swap enum-value narrows. `!(c == Red)` narrows c
                // away from Red on the then side, toward Red on else.
                out.then_enum_values = inner.else_enum_values;
                out.else_enum_values = inner.then_enum_values;
                // P41.2 — `!`-of-atomic stays atomic. `!(x is T)`
                // should still produce a complement narrow.
                out.is_atomic_is = inner.is_atomic_is;
            }
            _ => {}
        }
        out
    }

    /// Exhaustiveness: if `head_id` is the start of an
    /// `if (x == E::A) { ... } else if (x == E::B) { ... }` chain (no
    /// final `else`), check that every variant of `E` is covered. Emit
    /// a `non-exhaustive match over E (missing: …)` diagnostic if not.
    /// Records every if in the chain in `chain_member_ifs` so nested
    /// `else if` arms don't re-trigger the analysis.
    fn check_enum_exhaustiveness(&mut self, head_id: Idx<Stmt>, head_range: Range<usize>) {
        let Some(chain) = self.extract_enum_chain(head_id) else {
            return;
        };
        // Mark every if in the chain — even non-exhaustive ones —
        // as already accounted for so nested arms don't re-analyze.
        for arm in &chain.arms {
            self.chain_member_ifs.insert(arm.if_stmt_id);
        }
        // **P20.3** — a lone `if (x == E::V) { ... }` (no `else if`,
        // no final `else`) is *not* a match-like dispatch.
        if chain.arms.len() < 2 {
            return;
        }
        // Resolve the enum through the project index
        let Some(enum_id) = self
            .index
            .resolve_type(self.decl_registry, Some(self.module_uri), chain.enum_name)
            .and_then(|item| self.index.enum_types.get(&item).copied())
        else {
            return;
        };
        let TypeKind::Enum { variants, .. } = &self.arena.get(enum_id).kind else {
            return;
        };
        let declared: Vec<Symbol> = variants.to_vec();
        // Scope the "expected" variants by any enum-value narrow on
        // the chain's binding: an enclosing `if (x == E::A || x ==
        // E::B) { ... }` already restricts `x` to `{A, B}`, so the
        // inner chain only needs to cover that subset to be
        // exhaustive. The narrow's enum_sym must match the chain's
        // enum (different enums on the same binding shouldn't happen
        // in well-typed code, but we ignore the narrow rather than
        // crashing).
        let expected: Vec<Symbol> = match self.lookup_enum_value_narrow(chain.binding) {
            Some((narrow_enum, narrow_set)) if narrow_enum == chain.enum_name => declared
                .iter()
                .copied()
                .filter(|v| narrow_set.contains(v))
                .collect(),
            _ => declared,
        };
        let covered: FxHashSet<Symbol> = chain.arms.iter().map(|a| a.variant).collect();
        let missing: Vec<Symbol> = expected
            .iter()
            .copied()
            .filter(|v| !covered.contains(v))
            .collect();
        if missing.is_empty() {
            // **P24.2** — record exhaustive coverage even when the
            // chain has a trailing `else`: the dead-code lint uses
            // this to flag the trailing `else` as unreachable AND to
            // treat the chain as effectively divergent when every arm
            // body diverges.
            self.out.exhaustive_enum_chains.insert(head_id);
            return;
        }
        // Missing variants exist — only record the finding when there's
        // no catch-all `else` to fall through to. Recording (instead of
        // emitting a SemanticDiagnostic directly) lets the
        // `non-exhaustive` lint surface this as a rule-keyed,
        // suppressible diagnostic in the shared lint pipeline.
        if chain.has_final_else {
            return;
        }
        self.out.non_exhaustive_findings.push(NonExhaustiveFinding {
            head_id,
            enum_name: chain.enum_name,
            missing,
            byte_range: head_range,
        });
    }

    /// Walk the `else if` chain rooted at `head_id`. Each arm's
    /// condition must be `x == E::Variant` (or reverse) where `x` is a
    /// stable Param/Local binding shared across the whole chain.
    fn extract_enum_chain(&self, head_id: Idx<Stmt>) -> Option<EnumChain> {
        let Stmt::If(IfStmt {
            condition,
            else_branch,
            ..
        }) = &self.hir.stmts[head_id]
        else {
            return None;
        };
        let (binding, enum_name, variant) = self.match_enum_eq(*condition)?;
        let mut arms = vec![EnumChainArm {
            if_stmt_id: head_id,
            variant,
        }];
        let mut cursor = *else_branch;
        let mut has_final_else = false;
        while let Some(eb_id) = cursor {
            match &self.hir.stmts[eb_id] {
                Stmt::If(IfStmt {
                    condition: c,
                    else_branch: nested_eb,
                    ..
                }) => {
                    let Some((b, e, v)) = self.match_enum_eq(*c) else {
                        // A non-conforming `else if` works as a
                        // catch-all from the chain's perspective.
                        has_final_else = true;
                        break;
                    };
                    if b != binding || e != enum_name {
                        has_final_else = true;
                        break;
                    }
                    arms.push(EnumChainArm {
                        if_stmt_id: eb_id,
                        variant: v,
                    });
                    cursor = *nested_eb;
                }
                _ => {
                    has_final_else = true;
                    break;
                }
            }
        }
        Some(EnumChain {
            binding,
            enum_name,
            arms,
            has_final_else,
        })
    }

    fn match_enum_eq(&self, cond_id: Idx<Expr>) -> Option<(Idx<Ident>, Symbol, Symbol)> {
        let Expr::Binary(BinaryExpr {
            op: BinOp::Eq,
            left,
            right,
            ..
        }) = &self.hir.exprs[cond_id]
        else {
            return None;
        };
        if let Some(t) = self.try_extract_eq(*left, *right) {
            return Some(t);
        }
        self.try_extract_eq(*right, *left)
    }

    /// Symbol-keyed companion to [`match_enum_eq`]: returns
    /// `(binding, enum_sym, variant_sym)` for a `Binary` `Eq`/`Neq`
    /// expression of shape `x == E::V` (either operand order). Used by
    /// `derive_cond_narrows` to populate enum-value narrows without
    /// round-tripping through interned strings.
    fn match_enum_cmp_syms(&self, cond_id: Idx<Expr>) -> Option<(Idx<Ident>, Symbol, Symbol)> {
        let Expr::Binary(BinaryExpr {
            op, left, right, ..
        }) = &self.hir.exprs[cond_id]
        else {
            return None;
        };
        if !matches!(op, BinOp::Eq | BinOp::Neq) {
            return None;
        }
        if let Some(t) = self.try_extract_eq_syms(*left, *right) {
            return Some(t);
        }
        self.try_extract_eq_syms(*right, *left)
    }

    fn try_extract_eq_syms(
        &self,
        ident_side: Idx<Expr>,
        static_side: Idx<Expr>,
    ) -> Option<(Idx<Ident>, Symbol, Symbol)> {
        let Expr::Ident { name: name_idx, .. } = &self.hir.exprs[ident_side] else {
            return None;
        };
        let binding = match self.res.lookup(*name_idx)? {
            Definition::Param(d) | Definition::Local(d) => d,
            _ => return None,
        };
        let Expr::Static(StaticExpr { ty, property, .. }) = &self.hir.exprs[static_side] else {
            return None;
        };
        let enum_sym = self.hir.idents[self.hir.type_refs[*ty].name].symbol;
        let variant_sym = self.hir.idents[property.ident()].symbol;
        Some((binding, enum_sym, variant_sym))
    }

    fn try_extract_eq(
        &self,
        ident_side: Idx<Expr>,
        static_side: Idx<Expr>,
    ) -> Option<(Idx<Ident>, Symbol, Symbol)> {
        let Expr::Ident { name: name_idx, .. } = &self.hir.exprs[ident_side] else {
            return None;
        };
        let binding = match self.res.lookup(*name_idx)? {
            Definition::Param(d) | Definition::Local(d) => d,
            _ => return None,
        };
        let Expr::Static(StaticExpr { ty, property, .. }) = &self.hir.exprs[static_side] else {
            return None;
        };
        let ty_name = self.hir.idents[self.hir.type_refs[*ty].name].symbol;
        let prop_name = self.hir.idents[property.ident()].symbol;
        Some((binding, ty_name, prop_name))
    }

    fn ident_compared_to_null(&self, l: Idx<Expr>, r: Idx<Expr>) -> Option<Idx<Ident>> {
        let le = &self.hir.exprs[l];
        let re = &self.hir.exprs[r];
        if let (Expr::Ident { name, .. }, Expr::Null { .. }) = (le, re) {
            return Some(*name);
        }
        if let (Expr::Null { .. }, Expr::Ident { name, .. }) = (le, re) {
            return Some(*name);
        }
        None
    }

    /// `foo.bar != null` / `null != foo.bar`
    /// (and `==`, plus the `->` arrow form `foo->bar` and `arr[N]`
    /// with a literal int index) shape detection. Returns the
    /// member-access path string when one side is an `Expr::Member` /
    /// `Expr::Arrow` / `Expr::Offset` rooted at an Ident / `this` and
    /// the other side is the null literal. Returns `None` for any
    /// other shape (so e.g. `foo.bar == baz.qux` or `f().x != null`
    /// don't participate).
    fn member_compared_to_null(&self, l: Idx<Expr>, r: Idx<Expr>) -> Option<String> {
        let is_null_lit = |id: Idx<Expr>| matches!(&self.hir.exprs[id], Expr::Null { .. });
        let is_pathy = |id: Idx<Expr>| {
            matches!(
                self.hir.exprs[id],
                Expr::Member(_) | Expr::Arrow(_) | Expr::Offset(_)
            )
        };
        if is_pathy(l) && is_null_lit(r) {
            return self.member_path(l);
        }
        if is_pathy(r) && is_null_lit(l) {
            return self.member_path(r);
        }
        None
    }

    /// `true` when `expr` is a value whose runtime type cannot be null
    /// without further reasoning — literals, strings, `Type::variant`,
    /// `this`, array / object / tuple constructors, and `x!!`.
    /// Conservative: returns `false` for idents and calls, even when
    /// their declared type is non-nullable, because `derive_cond_narrows`
    /// runs in `&self` context and cannot consult the type inference
    /// state.
    fn is_syntactically_non_null(&self, expr_id: Idx<Expr>) -> bool {
        match &self.hir.exprs[expr_id] {
            Expr::Literal(_)
            | Expr::String(_)
            | Expr::Static(_)
            | Expr::QualifiedStatic { .. }
            | Expr::This { .. }
            | Expr::Array(..)
            | Expr::Object(_)
            | Expr::PositionalObject(_)
            | Expr::Tuple(..) => true,
            Expr::Paren(inner, _) => self.is_syntactically_non_null(*inner),
            Expr::Unary(UnaryExpr {
                op: UnaryOp::NonNullAssert,
                ..
            }) => true,
            _ => false,
        }
    }

    /// Walk `chain` as a Member / Arrow chain. For each null-safe step
    /// — either the step's own `opt_chaining` (`?.` / `?->` with `?`
    /// immediately before the property) or the step's `post_optional`
    /// (`a.b?` / `a->b?` whose result is treated as nullable) —
    /// record the path that must be non-null for the chain to be
    /// non-null on the requested side. Stops at the first non-chain
    /// node. Idents resolving to a Param / Local land in `out_idents`;
    /// other expressions are recorded via `member_path` into `out_paths`.
    ///
    /// `opt_chaining` narrows the step's RECEIVER (the value before
    /// `?` — `t?.f` non-null implies `t` non-null).
    ///
    /// `post_optional` narrows the STEP ITSELF (`t.g?` non-null implies
    /// `t.g` non-null). This case load-bears the chained `?.` shape:
    /// the grammar parses `t.g?.f` as `(t.g?).f`, attaching the `?`
    /// as `post_optional` on the inner `t.g` rather than `opt_chaining`
    /// on the outer `.f`. Both flags can be set on a single step
    /// (`t?.g?` → opt_chaining=true, post=true) and are handled independently.
    fn collect_optional_chain_receivers(
        &self,
        chain: Idx<Expr>,
        out_idents: &mut Vec<Idx<Ident>>,
        out_paths: &mut Vec<String>,
    ) {
        let mut cursor = chain;
        loop {
            let (receiver, opt_chaining, post_optional) = match &self.hir.exprs[cursor] {
                Expr::Member(MemberExpr {
                    receiver,
                    opt_chaining,
                    post_optional,
                    ..
                })
                | Expr::Arrow(MemberExpr {
                    receiver,
                    opt_chaining,
                    post_optional,
                    ..
                }) => (*receiver, opt_chaining.is_some(), post_optional.is_some()),
                Expr::Paren(inner, _) => {
                    cursor = *inner;
                    continue;
                }
                _ => break,
            };
            if opt_chaining {
                self.narrow_path_or_ident(receiver, out_idents, out_paths);
            }
            if post_optional {
                self.narrow_path_or_ident(cursor, out_idents, out_paths);
            }
            cursor = receiver;
        }
    }

    /// Resolve `expr` to either an ident binding or a member path and
    /// push it onto the matching out-vector. No-op for shapes that root
    /// in a fresh computed value (call, offset, paren of those, ...).
    fn narrow_path_or_ident(
        &self,
        expr: Idx<Expr>,
        out_idents: &mut Vec<Idx<Ident>>,
        out_paths: &mut Vec<String>,
    ) {
        if let Expr::Ident { name: name_idx, .. } = &self.hir.exprs[expr] {
            if let Some(Definition::Param(def) | Definition::Local(def)) =
                self.res.lookup(*name_idx)
            {
                out_idents.push(def);
            }
        } else if let Some(path) = self.member_path(expr) {
            out_paths.push(path);
        }
    }

    fn visit_expr(&mut self, expr_id: Idx<Expr>) -> TypeId {
        let ty = self.infer_expr(expr_id);
        self.record(expr_id, ty);
        ty
    }

    fn infer_expr(&mut self, expr_id: Idx<Expr>) -> TypeId {
        let expr = &self.hir.exprs[expr_id];
        match expr {
            Expr::Ident { name: idx, .. } => match self.res.lookup(*idx) {
                Some(Definition::Param(def)) | Some(Definition::Local(def)) => {
                    // P-erasure taint: carry a binding's runtime-erased
                    // shape (recorded at its `var x = <erased call>`) to
                    // this reference so `validate_type_relations` flags
                    // narrowing uses of `x` the runtime would throw on.
                    if let Some(rt) = self.out.def_runtime_types.get(&def).copied() {
                        self.out.expr_runtime_types.insert(expr_id, rt);
                    }
                    self.lookup_def_type(def)
                        .unwrap_or_else(|| self.any_nullable())
                }
                Some(Definition::Decl(decl_id)) => match &self.hir.decls[decl_id] {
                    Decl::Var(vd) => vd
                        .ty
                        .map(|ty_ref| self.lower_type_ref(ty_ref))
                        .unwrap_or_else(|| self.any_nullable()),
                    // Bare type / enum references used in value
                    // position carry the named decl's identity, not
                    // the generic `type` shape. Refine to
                    // `TypeOf(<that decl>)` so generic inference's
                    // typeof rule can witness `T := X` when the value
                    // flows into a `typeof T` slot. Stays assignable
                    // to plain `type` (see `is_assignable_to_with_index`'s
                    // `TypeOf → Type(core::type)` rule) so existing
                    // `fn foo(t: type)` consumers still accept these.
                    Decl::Type(td) => {
                        let name_sym = self.hir.idents[td.name].symbol;
                        let inner = self
                            .arena
                            .alloc_type(ItemKey::new(self.module_sym, name_sym));
                        self.arena.type_of(inner)
                    }
                    Decl::Enum(ed) => {
                        let name_sym = self.hir.idents[ed.name].symbol;
                        let item = ItemKey::new(self.module_sym, name_sym);
                        let inner = self
                            .index
                            .enum_types
                            .get(&item)
                            .copied()
                            .unwrap_or_else(|| self.arena.alloc_type(item));
                        self.arena.type_of(inner)
                    }
                    Decl::Fn(_) => {
                        // In-module fn ref: consult fn_signatures via
                        // the local ItemKey so the structural Lambda
                        // carries the real params / declared return.
                        let fn_sym = self.hir.idents[*idx].symbol;
                        let item = ItemKey::new(self.module_sym, fn_sym);
                        match self.index.fn_signatures.get(&item).cloned() {
                            Some(sig) => self.fn_ref_ty_from_sig(&sig),
                            None => self.function_ty(),
                        }
                    }
                    _ => self.any_nullable(),
                },
                Some(Definition::ProjectDecl {
                    uri: ref dec_uri, ..
                }) => {
                    // Cross-module bare ident value typing via
                    // the project signatures index.
                    // Top-level vars get their declared type from
                    // `var_types` (lowered in S7-S11). Without this,
                    // `var groups: nodeIndex<String, node<Group>>`
                    // referenced from another module would fall
                    // through to `index.has_name` (vars are in
                    // `values`) and type as `type`, breaking
                    // for-in iteration over the foreign var.
                    let sym = self.hir.idents[*idx].symbol;
                    let item = self.index.item_id_for(dec_uri, sym);
                    if let Some(var_ty) = item.and_then(|id| self.index.var_types.get(&id)).copied()
                    {
                        var_ty
                    } else if let Some(sig) = item
                        .and_then(|id| self.index.fn_signatures.get(&id))
                        .cloned()
                    {
                        // Native + non-native (.gcl) fns with a
                        // declared return live in `fn_signatures`. Mint
                        // a structural Lambda from the pre-lowered
                        // params/return so calls through the ident
                        // check against the real signature.
                        self.fn_ref_ty_from_sig(&sig)
                    } else if self.index.fn_names.contains(&sym) {
                        // Non-native fn without a declared return type
                        // (skipped from `fn_signatures` per
                        // `stage_lower_signatures`'s `let Some(ret) =
                        // ... else continue`). Opaque `function`
                        // fallback — no structural shape to mint.
                        self.function_ty()
                    } else if let Some(enum_id) =
                        item.and_then(|id| self.index.enum_types.get(&id).copied())
                    {
                        // P-typeof — cross-module bare ident referring
                        // to an enum decl. Refine to
                        // `TypeOf(<enum's interned TypeId>)` so the
                        // typeof witness rule fires at call sites like
                        // `type::enum_by_name(DurationUnit, "...")`.
                        self.arena.type_of(enum_id)
                    } else if let Some(item) =
                        item.filter(|id| self.index.type_members.contains_key(id))
                    {
                        // P-typeof — cross-module bare ident referring
                        // to a type decl. Refine to `TypeOf(Type(item))`.
                        let inner = self.arena.alloc_type(item);
                        self.arena.type_of(inner)
                    } else if self.index.has_name(sym) {
                        // Recognised name without a decl handle yet
                        // (runtime-internal type the user doesn't
                        // author). Keep the unrefined `type` shape so
                        // existing behavior for these stays put.
                        self.type_ty()
                    } else {
                        self.any_nullable()
                    }
                }
                Some(Definition::Project) => {
                    // Runtime-exposed value-position
                    // globals (e.g. `Infinity`, `NaN`) carry a fixed
                    // type the runtime owns; without this lookup the
                    // body walker would type them as `any` and float
                    // dispatch downstream would fail.
                    let sym = self.hir.idents[*idx].symbol;
                    self.index
                        .runtime_globals
                        .get(&sym)
                        .copied()
                        .unwrap_or_else(|| self.any_nullable())
                }
                Some(Definition::Generic(_)) | None => self.any_nullable(),
            },
            Expr::Literal(LiteralExpr { kind, .. }) => match kind {
                LiteralKind::Bool(_) => self.arena.builtins.bool_,
                LiteralKind::Int(_) => self.arena.builtins.int,
                LiteralKind::Float(_) => self.arena.builtins.float,
                LiteralKind::Char(_) => self.arena.builtins.char_,
                LiteralKind::Duration(_) => self.arena.builtins.duration,
                LiteralKind::Time(_) | LiteralKind::Iso8601(_) => self.arena.builtins.time,
            },
            Expr::Null { .. } => self.null(),
            Expr::This { .. } => self
                .this_stack
                .last()
                .copied()
                .unwrap_or_else(|| self.any_nullable()),
            Expr::String(StringExpr { parts, .. }) => {
                // Visit each `${expr}` interpolation so the
                // analyzer types and binds the inner identifiers
                // (otherwise locals referenced only inside template
                // strings would surface as `unused-local` and never
                // get an `expr_types` entry).
                for part in parts {
                    if let greycat_analyzer_hir::hir::StringPart::Interp { expr, .. } = part {
                        let _ = self.visit_expr(*expr);
                    }
                }
                self.arena.builtins.string
            }
            Expr::Tuple(items, _) => {
                // assert_eq!(items.len(), 2, "Tuple items length must be exactly 2");
                let x = self.visit_expr(items[0]);
                let y = self.visit_expr(items[1]);
                self.arena.tuple(self.arena.builtins.tuple_key, x, y)
            }
            Expr::Array(items, _) => {
                let mut first: Option<TypeId> = None;
                let mut uniform = true;
                for &i in items {
                    let t = self.visit_expr(i);
                    match first {
                        None => first = Some(t),
                        Some(f) => {
                            if t != f {
                                uniform = false;
                            }
                        }
                    }
                }
                let uniform_elem = if uniform { first } else { None };
                let inferred_elem = self.infer_array_element_type(items, uniform_elem);

                let elem = inferred_elem.unwrap_or_else(|| self.any_nullable());
                self.arena
                    .alloc_generic(self.arena.builtins.array_key, vec![elem])
            }
            Expr::Object(ObjectExpr {
                ty,
                fields,
                byte_range,
            }) => {
                let anonymous = self.check_anonymous_object_head(*ty, byte_range.clone());
                let obj_ty = self.lower_type_ref(*ty);
                // For a `Map { k: v }` the keys are value expressions
                // (typed against `K`), so they must be visited too; for
                // a classic object the key is a field *name*
                // (member-resolved by `check_object_required_attrs`),
                // not a value, so visiting it would mis-type a field
                // name as a value use.
                let is_map = self.head_decl_is_map(obj_ty);
                if is_map {
                    for f in fields {
                        let _ = self.visit_expr(f.name);
                        let _ = self.visit_expr(f.value);
                    }
                } else {
                    for f in fields {
                        let _ = self.visit_expr(f.value);
                    }
                    if !anonymous {
                        self.check_object_required_attrs(*ty, obj_ty, fields);
                    }
                }
                obj_ty
            }
            Expr::PositionalObject(PositionalObjectExpr {
                ty,
                fields,
                byte_range,
            }) => {
                let anonymous = self.check_anonymous_object_head(*ty, byte_range.clone());
                for f in fields {
                    let _ = self.visit_expr(*f);
                }
                let obj_ty = self.lower_type_ref(*ty);
                if fields.is_empty() && !anonymous {
                    self.check_object_required_attrs(*ty, obj_ty, &[]);
                }
                obj_ty
            }
            Expr::Member(MemberExpr {
                receiver,
                property,
                opt_chaining,
                post_optional,
                ..
            })
            | Expr::Arrow(MemberExpr {
                receiver,
                property,
                opt_chaining,
                post_optional,
                ..
            }) => {
                let property = property.ident();
                let recv_ty = self.visit_expr(*receiver);
                // P16.5 — `n->field` where `n: node<T>` (or any node-tag
                // shape: `nodeTime<T>`, `nodeIndex<K, V>`, …) resolves
                // `field` against the inner type's attrs / methods, not
                // against the tag's. The auto-deref only applies on
                // `Expr::Arrow` so `n.method()` still binds to `node`'s
                // own method list (the `.` → `->` rewrite advice from
                // completion is what nudges users toward the right
                // shape; the analyzer doesn't silently auto-deref `.`).
                let resolution_ty = if matches!(self.hir.exprs[expr_id], Expr::Arrow(_)) {
                    self.arrow_deref_receiver(recv_ty).unwrap_or(recv_ty)
                } else {
                    recv_ty
                };
                self.resolve_member(resolution_ty, property);
                // P16.1 — once `resolve_member` has bound the property
                // (intra-module case populates `member_uses`), the
                // expression's own inferred type is whatever the bound
                // attr / method gives us:
                //   `Attr(id)`   -> attr's lowered declared type
                //   `Method(_)`  -> `function` (gcl's first-class type;
                //                   the rich signature view comes from
                //                   `member_uses` at hover time, not
                //                   from the expr's `TypeId`).
                // Cross-module bindings live in `foreign_member_uses`,
                // which the project pipeline writes back later (P16.3).
                // Anonymous-type / primitive cases stay `any` here —
                // primitives are extended in P16.2.
                let base_ty = if let Some(member) = self.out.member_uses.get(&property).copied() {
                    match member {
                        MemberDef::Attr(attr_id) => {
                            let attr = self.hir.type_attrs[attr_id].clone();
                            attr.ty
                                .map(|ty| self.lower_type_ref(ty))
                                .unwrap_or_else(|| self.any_nullable())
                        }
                        MemberDef::Method(_) => self.function_ty(),
                    }
                } else if self.out.foreign_member_uses.contains_key(&property) {
                    // P22 — cross-module attr / method typing inline.
                    // Reads the project signatures index built in S7
                    // (`stage_lower_signatures`) and applies generic
                    // substitution from the receiver's instantiation.
                    self.foreign_member_type(resolution_ty, property)
                        .unwrap_or_else(|| self.any_nullable())
                } else {
                    self.any_nullable()
                };
                // P16.7 + P19.17 — nullability propagates *up the chain*
                // whenever the receiver is nullable, regardless of
                // whether the user wrote `?.` at this segment. The
                // runtime evaluates the whole chain to null when any
                // prior `?.` shorts, so `x?.y.z` types as `Z?`. The
                // `opt_chaining` flag is what the lint reads to decide
                // whether to flag the dereference as "possibly null"
                // (no flag → flag fires), but it doesn't change typing.
                // `a.b?` / `a->b?` still lifts unconditionally as a
                // user-asserted "treat as nullable" override.
                let _ = opt_chaining;
                let recv_nullable = self.arena.get(recv_ty).nullable;
                let result_ty = if recv_nullable || post_optional.is_some() {
                    self.arena.nullable(base_ty)
                } else {
                    base_ty
                };
                // **P19.16 + P19.21** — strip the result's nullability
                // when the member-access path was guarded non-null in
                // an enclosing scope (`if (foo.bar != null) { ... }`)
                // or write-narrowed by `?=`. Applies to both
                // `Expr::Member` (`.`) and `Expr::Arrow` (`->`); the
                // path keys carry the operator (`->` vs `.`) so the
                // two forms don't share narrows.
                // Also: an `x is T` guard on the same path overrides
                // the declared type with the narrowed type.
                let path = self.member_path(expr_id);
                if let Some(p) = path.as_deref()
                    && let Some(narrowed) = self.lookup_member_typed(p)
                {
                    narrowed
                } else if self.arena.get(result_ty).nullable
                    && let Some(p) = path.as_deref()
                    && self.member_path_is_non_null(p)
                {
                    self.arena.strip_nullable(result_ty)
                } else {
                    result_ty
                }
            }
            Expr::Static(s) => {
                // `Type::method` resolution. Lower the receiver
                // type so cross-module receivers land as `Named(name)`
                // (via `lower_type_ref`'s `index.has_name(&name)` arm),
                // then run `resolve_member` on the property.
                let recv_ty = self.lower_type_ref(s.ty);
                let property = s.property.ident();
                self.resolve_member_with(recv_ty, property, false);
                // Enum-variant access: `Foo::a` where `Foo` is an enum
                // and `a` is one of its variants — the value's type is
                // the enum itself, not `any`.
                if let TypeKind::Enum { variants, .. } = &self.arena.get(recv_ty).kind {
                    let prop_sym = self.hir.idents[property].symbol;
                    if variants.contains(&prop_sym) {
                        return recv_ty;
                    }
                }
                // `Type::attr` (no parens) → `field`,
                // `Type::method` (no parens) → `function`. Replaces
                // pass 3.5's static-as-value typing. **P19.13** —
                // pass `recv_ty` so `static_value_type` can resolve
                // cross-module static-attr value access through the
                // project index (`Programs::python3` typed as
                // `String` instead of `field` when `python3` is
                // declared `static`).
                if let Some(ty) = self.static_value_type(recv_ty, property) {
                    return ty;
                }
                // `module::Name` shapes parse as `Static` with
                // the module name as the "type ref" (the parser
                // doesn't distinguish modules from types). Fall back
                // to a 2-segment QualifiedStatic-style lookup against
                // the project signatures index.
                let recv_name = self.hir.type_refs[s.ty].name;
                let chain = [recv_name, property];
                self.qualified_static_value_type(&chain)
                    .unwrap_or_else(|| self.any_nullable())
            }
            Expr::QualifiedStatic { chain, .. } => {
                // P23 — chained `module::name` / `module::Type::name`
                // shapes. Bind the chain segments to their foreign
                // decls / members so hover / goto-def have something
                // to point at, then type the value-position expr
                // inline using the project signatures index. (Calls
                // are routed through `try_member_call_typing` from
                // the `Expr::Call` branch.)
                self.bind_qualified_chain_segments(chain);
                self.qualified_static_value_type(chain)
                    .unwrap_or_else(|| self.any_nullable())
            }
            Expr::Offset(OffsetExpr {
                receiver,
                index,
                pre_optional,
                post_optional,
                ..
            }) => {
                let recv_ty = self.visit_expr(*receiver);
                let _ = self.visit_expr(*index);
                // **P19.11** — element-type inference for offset
                // access. `arr[i]` on `Array<T>` / `Set<T>` /
                // `nodeList<T>` yields `T`; `m[k]` on `Map<K, V>`
                // / `nodeIndex<K, V>` yields `V`. The receiver's
                // optional marker propagates through `pre_optional`
                // (`a?[i]` lifts the result to nullable when `a`
                // is nullable); `post_optional` (`a[i]?`) lifts
                // unconditionally. Strip the optional from the
                // receiver before pattern-matching so the binding
                // logic is the same with or without `?`.
                let underlying = if self.arena.get(recv_ty).nullable {
                    let mut t = self.arena.get(recv_ty).clone();
                    t.nullable = false;
                    self.arena.alloc(t)
                } else {
                    recv_ty
                };
                // **P19.15** — when the index is an `Expr::Range`
                // the offset is a "slice view" that returns the
                // *receiver type* unchanged (still iterable in the
                // same shape). Otherwise it's a single-element
                // lookup that returns the element type.
                let index_is_range = matches!(&self.hir.exprs[*index], Expr::Range { .. });
                let base = if index_is_range {
                    underlying
                } else {
                    // Offset access (`arr[i]`) is allowed only on
                    // `Array<T>` per the GreyCat runtime — `greycat
                    // run` rejects `Map[k]`, `nodeList[k]`,
                    // `nodeIndex[k]`, etc. with "Offset access is
                    // only allowed on instances of type 'Array'".
                    // Compare the generic's decl-handle against
                    // `arena.builtins.array_key` rather than
                    // string-matching its printable name: handle
                    // equality is the canonical identity check for
                    // std-core decls (see `TypeArena::is_node_tag`)
                    // and isn't fooled by a user-declared
                    // `type Array<T>` in their own module.
                    match &self.arena.get(underlying).kind {
                        TypeKind::Generic { tpl, args }
                            if *tpl == self.arena.builtins.array_key && !args.is_empty() =>
                        {
                            args[0]
                        }
                        _ => self.any_nullable(),
                    }
                };
                let lift_pre = pre_optional.is_some() && self.arena.get(recv_ty).nullable;
                let result_ty = if lift_pre || post_optional.is_some() {
                    self.arena.nullable(base)
                } else {
                    base
                };
                // Mirror `Expr::Member`'s narrow consult — when the
                // offset path is keyable (literal index) and an `is T`
                // guard or `!= null` guard recorded a narrow for it,
                // override the element type. `arr[0] is IfcFloatAttr`
                // followed by `arr[0].value` should see the narrowed
                // type on `arr[0]`, same as `obj.foo is T` does for
                // `obj.foo`.
                let path = self.member_path(expr_id);
                if let Some(p) = path.as_deref()
                    && let Some(narrowed) = self.lookup_member_typed(p)
                {
                    narrowed
                } else if self.arena.get(result_ty).nullable
                    && let Some(p) = path.as_deref()
                    && self.member_path_is_non_null(p)
                {
                    self.arena.strip_nullable(result_ty)
                } else {
                    result_ty
                }
            }
            Expr::Call(CallExpr { callee, args, .. }) => {
                let callee_ty = self.visit_expr(*callee);
                let arg_tys: Vec<TypeId> = args.iter().map(|a| self.visit_expr(*a)).collect();
                let call_range = self.hir.exprs[expr_id].byte_range();
                // Compute the raw return type via the first call-typing
                // path that applies, then apply the receiver-nullability
                // lift once, uniformly, at this single funnel. The lift
                // (P19.17 — `recvExpr?.m()` / `recvExpr.m()` where the
                // receiver is nullable yields `Ret?`) lives in
                // `lift_call_result_for_nullable_receiver` rather than
                // inside each path so the generic-inference path
                // (`run_method_generic_inference`, which handles methods
                // whose return is the receiver's own generic param — e.g.
                // `node<T>::resolve(): T`) and the non-generic path stay
                // in lockstep. Putting the lift in only one path is what
                // let `s.loadMethod()?.resolve()` (generic return) drop
                // the `?` while `s.loadMethod()` (concrete return) kept it.
                //
                // P12.1: the first path handles generic fns / generic
                // methods via constraint-based inference.
                let raw = if let Some((materialized, runtime)) =
                    self.try_generic_call_inference(*callee, &arg_tys, call_range)
                {
                    // P-erasure — when the callee erases its generic
                    // container result at runtime, stash the erased shape
                    // (same nullability lift as the materialized result)
                    // so `validate_type_relations` can flag narrowing uses
                    // the runtime would throw on.
                    if let Some(rt) = runtime {
                        let lifted = self.lift_call_result_for_nullable_receiver(*callee, rt);
                        self.out.expr_runtime_types.insert(expr_id, lifted);
                    }
                    materialized
                } else if let Some(ret) = self.try_member_call_typing(*callee) {
                    // P23 — inline call-return typing for Member / Arrow /
                    // Static method calls. Pulls the method's lowered
                    // return type from the S7 signatures index and applies
                    // `arena.substitute` against the receiver's
                    // instantiation. Replaces pass 3.5 + the receiver-
                    // driven shape-substitution shim for these shapes.
                    ret
                } else {
                    // P15.10: pairwise arg-type validation runs in
                    // `ProjectAnalysis::validate_type_relations` so outer
                    // calls whose args contain inner static-expr calls
                    // validate against settled arg types. Doing it here
                    // would surface false positives for arg shapes whose
                    // type isn't known until pass 3.5 fixes them up.
                    //
                    // lambda-unify(6): if the callee resolves to a
                    // structural Lambda (lambda literal in a var, or a
                    // fn-ref minted by `fn_ref_ty_from_sig`), the call's
                    // result type is the lambda's `ret` slot. `None` →
                    // `any?` fallback (lambda has no observable return).
                    // Strip the callee's outer nullable bit so `function?`-
                    // typed callees still produce the underlying ret.
                    let stripped = self.arena.strip_nullable(callee_ty);
                    let lambda_ret = match &self.arena.get(stripped).kind {
                        TypeKind::Lambda { ret, .. } => Some(*ret),
                        _ => None,
                    };
                    match lambda_ret {
                        Some(ret_opt) => ret_opt.unwrap_or_else(|| self.any_nullable()),
                        None => self.any_nullable(),
                    }
                };
                self.lift_call_result_for_nullable_receiver(*callee, raw)
            }
            Expr::Binary(BinaryExpr {
                op, left, right, ..
            }) => {
                let lt = self.visit_expr(*left);
                // P13.2-followup — short-circuit operands narrow the
                // *other* operand, not just the enclosing `if`. In
                // `x != null && f(x)`, the right side only runs when
                // the left held, so `f(x)` should see `x` non-null.
                // Mirrored for `||`: right only runs when left failed,
                // so `else_non_null` applies. Same `derive_cond_narrows`
                // engine the if-condition path uses, just scoped to a
                // single operand visit.
                let rt = match op {
                    BinOp::And | BinOp::Or => {
                        let CondNarrows {
                            then_non_null,
                            else_non_null,
                            then_typed,
                            then_member_non_null,
                            else_member_non_null,
                            then_member_typed,
                            else_typed: _,
                            else_member_typed: _,
                            then_typed_union,
                            else_typed_union: _,
                            then_typed_id: _,
                            else_typed_id: _,
                            then_null,
                            else_null,
                            then_member_null,
                            else_member_null,
                            is_atomic_is: _,
                            then_enum_values: _,
                            else_enum_values: _,
                        } = self.derive_cond_narrows(*left);
                        let (
                            non_null,
                            typed,
                            typed_union,
                            member_non_null,
                            member_typed,
                            null,
                            member_null,
                        ) = match op {
                            BinOp::And => (
                                then_non_null,
                                then_typed,
                                then_typed_union,
                                then_member_non_null,
                                then_member_typed,
                                then_null,
                                then_member_null,
                            ),
                            BinOp::Or => (
                                else_non_null,
                                Vec::new(),
                                Vec::new(),
                                else_member_non_null,
                                Vec::new(),
                                else_null,
                                else_member_null,
                            ),
                            _ => unreachable!(),
                        };
                        self.push_narrow();
                        for ident in &non_null {
                            if let Some(cur) = self.lookup_def_type(*ident) {
                                let stripped = self.arena.strip_nullable(cur);
                                self.write_narrow(*ident, stripped);
                            }
                        }
                        for (ident, ty_ref) in &typed {
                            let ty = self.lower_type_ref(*ty_ref);
                            self.write_narrow(*ident, ty);
                        }
                        for (ident, ty_refs) in &typed_union {
                            let ty = self.lower_typed_union(ty_refs);
                            self.write_narrow(*ident, ty);
                        }
                        for path in member_non_null {
                            self.write_member_non_null(Cow::Owned(path));
                        }
                        for (path, ty_ref) in member_typed {
                            let ty = self.lower_type_ref(ty_ref);
                            self.write_member_typed(Cow::Owned(path), ty);
                        }
                        for ident in &null {
                            let null_ty = self.null();
                            self.write_narrow(*ident, null_ty);
                        }
                        for path in member_null {
                            let null_ty = self.null();
                            self.write_member_typed(Cow::Owned(path), null_ty);
                        }
                        let rt = self.visit_expr(*right);
                        self.pop_narrow();
                        rt
                    }
                    _ => self.visit_expr(*right),
                };
                // **P19.16** — GreyCat's `=` parses as a binary
                // expression (not a Stmt::Assign). When the LHS is
                // a Param/Local Ident, narrow its binding to the
                // RHS's type for the rest of the enclosing block.
                // The post-if join logic then lifts narrows that
                // hold along every path.
                if matches!(op, BinOp::Other("=")) {
                    self.check_private_attr_write(*left);
                    self.check_static_attr_write(*left);
                    self.record_assign_narrow(*left, rt);
                } else if matches!(op, BinOp::Other("?=")) {
                    self.check_private_attr_write(*left);
                    self.check_static_attr_write(*left);
                    self.record_coalesce_assign_narrow(*left, rt);
                }
                self.infer_binary(*op, lt, rt)
            }
            Expr::Unary(UnaryExpr { op, operand, .. }) => {
                let inner = self.visit_expr(*operand);
                match op {
                    UnaryOp::Not => self.arena.builtins.bool_,
                    UnaryOp::Neg | UnaryOp::Pos | UnaryOp::BitNot | UnaryOp::Inc | UnaryOp::Dec => {
                        inner
                    }
                    // `*n` deref. For
                    // `Generic { name: "node", args: [T] }` (and
                    // similar tag shapes) returns `T`; otherwise
                    // returns `inner` so non-node uses still get
                    // a usable type. Strips a nullable on the
                    // receiver so `*n?` returns `T?` (handled by
                    // the `nullable` flag on the inner TypeId
                    // when lifted).
                    UnaryOp::Deref => self.arrow_deref_receiver(inner).unwrap_or(inner),
                    UnaryOp::NonNullAssert => {
                        // `x!!` strips nullable from the result and (P6.4)
                        // narrows the operand binding for the rest of the
                        // enclosing block when the operand is an Ident
                        // bound to a Param/Local.
                        //
                        // When the operand is a stable
                        // member-access path (`x.y`, `this.foo.bar`,
                        // `x->y`), record the path on the
                        // `member_narrows` stack so subsequent reads of
                        // the same path strip the nullable bit at the
                        // bottom of the `Expr::Member` / `Expr::Arrow`
                        // arm (the same site P19.16 / P19.21 use for
                        // `!= null` / `?=` narrows). The narrow
                        // correctly drops on assignment to the path
                        // (existing `record_assign_narrow` clears it
                        // when the RHS is nullable).
                        let result = self.arena.strip_nullable(inner);
                        if let Expr::Ident { name: name_idx, .. } = self.hir.exprs[*operand].clone()
                            && let Some(Definition::Param(def) | Definition::Local(def)) =
                                self.res.lookup(name_idx)
                        {
                            self.write_narrow(def, result);
                        }
                        if matches!(
                            self.hir.exprs[*operand],
                            Expr::Member(_) | Expr::Arrow(_) | Expr::Offset(_)
                        ) && let Some(path) = self.member_path(*operand)
                        {
                            self.write_member_non_null(Cow::Owned(path));
                        }
                        result
                    }
                }
            }
            Expr::Paren(inner, _) => self.visit_expr(*inner),
            Expr::Lambda(LambdaExpr {
                params,
                return_type,
                body,
                ..
            }) => {
                let mut param_tys = Vec::with_capacity(params.len());
                for p in params {
                    let p = self.hir.fn_params[*p].clone();
                    let pt =
                        p.ty.map(|t| self.lower_type_ref(t))
                            .unwrap_or_else(|| self.any_nullable());
                    param_tys.push(pt);
                }
                let declared_ret = return_type.map(|t| self.lower_type_ref(t));
                self.visit_block(body, declared_ret);
                // Lambda body inference mirrors the fn-level
                // infer-return-type lint — same `return_inference`
                // helper. When the user didn't annotate, walk the body
                // returns and join them; only accept a single GCL-
                // expressible type (`T` or `T?`), otherwise stay
                // `None` (rendered `fn(...)`).
                let inferred_ret = declared_ret.or_else(|| {
                    let ty = crate::return_inference::inferred_return_from_block(
                        self.hir, self.out, self.arena, body,
                    )?;
                    crate::return_inference::is_expressible_type_ident(self.arena, ty).then_some(ty)
                });
                self.arena.lambda(param_tys, inferred_ret)
            }
            Expr::Is { value, .. } => {
                let _ = self.visit_expr(*value);
                self.arena.builtins.bool_
            }
            Expr::Range { from, to, .. } => {
                // **P19.15** — visit both endpoints so their
                // exprs get types in the table; the range itself
                // doesn't have a useful TypeId on its own (it only
                // appears as an offset index or a for-in iterator
                // range, both of which look at the surrounding
                // shape, not the range's own type).
                if let Some(f) = from {
                    let _ = self.visit_expr(*f);
                }
                if let Some(t) = to {
                    let _ = self.visit_expr(*t);
                }
                self.any_nullable()
            }
            Expr::Cast { value, ty, .. } => {
                let from_ty = self.visit_expr(*value);
                let to_ty = self.lower_type_ref(*ty);
                // P12.3: validate the cast against the GreyCat `as`
                // rules. Surfaces invalid casts as a diagnostic; the
                // resulting expression type is still `to_ty` so
                // downstream inference doesn't cascade.
                //
                // Inheritance-aware up/down-casts within a supertype
                // chain (e.g. `pvEntity as PVInstallation` where
                // `PVInstallation extends PVEntity`) are accepted by
                // `is_castable_with_index`, which walks the more-
                // specific side's chain with substituted args and
                // verifies the hop's args match the other side. The
                // GreyCat runtime drops `as` casts entirely — this is
                // the only safety net, so the wrapper is the single
                // source of truth for what's allowed.
                if !crate::project::is_castable_with_index(
                    self.index,
                    self.decl_registry,
                    self.arena,
                    from_ty,
                    to_ty,
                ) {
                    let r = self.hir.exprs[expr_id].byte_range();
                    let msg = format!(
                        "cannot cast `{}` to `{}`",
                        self.display(from_ty),
                        self.display(to_ty),
                    );
                    self.diag(Severity::Error, "invalid-cast", msg, r);
                }
                to_ty
            }
            Expr::Unsupported { .. } => self.any_nullable(),
        }
    }

    fn infer_binary(&mut self, op: BinOp, lt: TypeId, rt: TypeId) -> TypeId {
        let int = self.arena.builtins.int;
        let float = self.arena.builtins.float;
        let bool_t = self.arena.builtins.bool_;

        match op {
            BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Lte | BinOp::Gt | BinOp::Gte => bool_t,
            BinOp::And | BinOp::Or => bool_t,
            BinOp::Add => {
                // **P19.13** — String concat: `String + X` /
                // `X + String` → `String`. The runtime coerces the
                // non-string side via `to_string()`. Only `+`
                // overloads on String — the other arithmetic ops
                // stay numeric.
                // **P19.15** — strip nullability for arithmetic
                // dispatch only (Coalesce / comparisons read the
                // original `nullable` flag, so we keep `lt` / `rt`
                // intact at the function entry).
                let lt_n = self.arena.strip_nullable(lt);
                let rt_n = self.arena.strip_nullable(rt);
                let string_t = self.arena.builtins.string;
                let time_t = self.arena.builtins.time;
                let dur_t = self.arena.builtins.duration;
                if lt_n == string_t || rt_n == string_t {
                    string_t
                } else if (lt_n == time_t && rt_n == dur_t) || (lt_n == dur_t && rt_n == time_t) {
                    // **P19.14** — time arithmetic.
                    time_t
                } else if lt_n == dur_t && rt_n == dur_t {
                    dur_t
                } else if lt_n == float || rt_n == float {
                    float
                } else if lt_n == int && rt_n == int {
                    int
                } else {
                    self.any_nullable()
                }
            }
            BinOp::Sub => {
                // **P19.14** — `time - time → duration`,
                // `time - duration → time`,
                // `duration - duration → duration`.
                let lt_n = self.arena.strip_nullable(lt);
                let rt_n = self.arena.strip_nullable(rt);
                let time_t = self.arena.builtins.time;
                let dur_t = self.arena.builtins.duration;
                if lt_n == time_t && rt_n == time_t {
                    dur_t
                } else if lt_n == time_t && rt_n == dur_t {
                    time_t
                } else if lt_n == dur_t && rt_n == dur_t {
                    dur_t
                } else if lt_n == float || rt_n == float {
                    float
                } else if lt_n == int && rt_n == int {
                    int
                } else {
                    self.any_nullable()
                }
            }
            BinOp::Mul => {
                // **P19.14** — `duration * int / float → duration`.
                let lt_n = self.arena.strip_nullable(lt);
                let rt_n = self.arena.strip_nullable(rt);
                let dur_t = self.arena.builtins.duration;
                if (lt_n == dur_t && (rt_n == int || rt_n == float))
                    || ((lt_n == int || lt_n == float) && rt_n == dur_t)
                {
                    dur_t
                } else if lt_n == float || rt_n == float {
                    float
                } else if lt_n == int && rt_n == int {
                    int
                } else {
                    self.any_nullable()
                }
            }
            BinOp::Div | BinOp::Mod => {
                if lt == float || rt == float {
                    float
                } else if lt == int && rt == int {
                    int
                } else {
                    self.any_nullable()
                }
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => int,
            BinOp::Coalesce => {
                // P16.7 — `a ?? b`: returns `a` when not-null, else
                // `b`. Type: `(typeof a stripped of null) | (typeof b
                // stripped of null)`, then re-wrapped nullable when
                // `b` itself is nullable (because the fallback can
                // still be null in that case). Same-shape collapse
                // keeps `T? ?? T → T` clean for the assignability
                // checker.
                let lt_stripped = self.arena.strip_nullable(lt);
                let rt_nullable = self.arena.get(rt).nullable;
                let rt_stripped = self.arena.strip_nullable(rt);
                let merged = if lt_stripped == rt_stripped {
                    lt_stripped
                } else {
                    self.arena.alloc(Type {
                        kind: TypeKind::Union {
                            alts: Box::new([lt_stripped, rt_stripped]),
                        },
                        nullable: false,
                    })
                };
                if rt_nullable {
                    self.arena.nullable(merged)
                } else {
                    merged
                }
            }
            BinOp::Other(_) => self.any_nullable(),
        }
    }

    /// Element-type inference for an `Expr::Array` literal. Returns
    /// `Some(T)` when every element is constant-evaluable in shape and
    /// shares the same TypeId; otherwise `None` (caller falls back to
    /// `any?` so the array types as bare `Array`).
    ///
    /// Mimics the runtime's syntactic trigger rules so user expectations
    /// stay aligned with `greycat run`. See [`Self::is_constant_array_element_shape`]
    /// for the per-element rules. The resulting *element type* is the
    /// analyzer's own inference, so binary expressions over typed
    /// literals (e.g. `42time - 10s`) get the correct `time` here even
    /// though the runtime currently mis-infers them as `duration` — a
    /// deliberate deviation from the buggy runtime behavior.
    fn infer_array_element_type(
        &self,
        items: &[Idx<Expr>],
        uniform_elem: Option<TypeId>,
    ) -> Option<TypeId> {
        // `uniform_elem` is `Some(t)` only when the array is non-empty and
        // every element typed as the same `t`; empty / divergent arrays
        // arrive as `None` and bail (no widening, mirroring the runtime).
        let elem = uniform_elem?;
        for &i in items {
            if !self.is_constant_array_element_shape(i) {
                return None;
            }
        }
        // Runtime returns bare `Array` when the shared element type is
        // `null` (covers the all-`null` case).
        if matches!(self.arena.get(elem).kind, TypeKind::Null) {
            return None;
        }
        Some(elem)
    }

    /// Per-element "constant-evaluable" shape check used by the
    /// array-literal element-type inference. Mirrors the runtime's
    /// (purely syntactic) algorithm — verified empirically against
    /// `greycat run`:
    ///
    /// - Literals (int / float / bool / time / duration / iso8601) and
    ///   string literals qualify. `null` qualifies *shape-wise* but
    ///   its type causes the array to bail in the caller.
    /// - **`char` literals do NOT qualify** (runtime quirk — the
    ///   compiler skips them even though they have a type).
    /// - `paren` / `unary` / `binary` recurse into their operands.
    /// - Nested `array` literals recurse into their elements (so
    ///   `[[1, 2]]` infers as `Array<Array<int>>`).
    /// - `object_expr` qualifies when it carries a type-ident — the
    ///   runtime does NOT recurse into the object's slots, so
    ///   `[Foo { x: <ident> }]` still infers `Array<Foo>`.
    /// - Everything else (ident, call, member / arrow / static access,
    ///   offset, range, cast, lambda, `this`, tuple, …) disqualifies.
    fn is_constant_array_element_shape(&self, expr_id: Idx<Expr>) -> bool {
        match &self.hir.exprs[expr_id] {
            Expr::Literal(l) => !matches!(l.kind, LiteralKind::Char(_)),
            Expr::String(_) | Expr::Null { .. } => true,
            Expr::Array(items, _) => items
                .iter()
                .all(|i| self.is_constant_array_element_shape(*i)),
            Expr::Paren(inner, _) => self.is_constant_array_element_shape(*inner),
            Expr::Unary(u) => self.is_constant_array_element_shape(u.operand),
            Expr::Binary(b) => {
                self.is_constant_array_element_shape(b.left)
                    && self.is_constant_array_element_shape(b.right)
            }
            Expr::Object(_) | Expr::PositionalObject(_) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_core::SymbolTable;
    use greycat_analyzer_hir::lower_module;
    use greycat_analyzer_syntax::parse;

    /// Convenience wrapper that allocates a private arena and decl
    /// registry. Same caveat as [`analyze`]: both are returned to the
    /// caller alongside the result so any [`TypeId`] / [`ItemKey`] in
    /// the result can still be looked up.
    pub fn analyze_with_index(
        hir: &Hir,
        res: &Resolutions,
        arena: &mut TypeArena,
        index: &ProjectIndex,
    ) -> (DeclRegistry, AnalysisResult) {
        use std::str::FromStr;
        let module_uri = Uri::from_str("file:///module.gcl").unwrap();
        // Standalone equivalent of `ProjectIndex::ingest`'s decl
        // registration step — see [`analyze`] for the rationale.
        let mut decl_registry = DeclRegistry::default();
        if let Some(module) = hir.module.as_ref() {
            for d_id in &module.decls {
                let name = match &hir.decls[*d_id] {
                    Decl::Type(td) => hir.idents[td.name].symbol,
                    Decl::Enum(ed) => hir.idents[ed.name].symbol,
                    _ => continue,
                };
                if let Some(item) = index.item_id_for(&module_uri, name) {
                    decl_registry.record(item, *d_id);
                }
            }
        }
        let out = analyze_with_index_into(hir, res, index, &decl_registry, &module_uri, arena);
        (decl_registry, out)
    }

    fn analyze_src(src: &str) -> (TypeArena, AnalysisResult) {
        let tree = parse(src);
        let symbols = SymbolTable::new();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let mut arena = TypeArena::new(&symbols);
        let index = ProjectIndex::new(symbols, &arena);
        let res = index.resolutions(&hir, None, None);
        let (_decl_registry, analysis) = analyze_with_index(&hir, &res, &mut arena, &index);
        (arena, analysis)
    }

    /// Drop-in helper for tests that don't need to inspect the arena.
    fn analyze_src_only(src: &str) -> AnalysisResult {
        analyze_src(src).1
    }

    /// Run `src` through the real `ProjectAnalysis` pipeline (single
    /// module) and return `(driver, uri)`. Use this — not
    /// [`analyze_src_only`] — for checks that consult the project-wide
    /// index (enum exhaustiveness, member shapes), which the standalone
    /// [`analyze`] harness doesn't populate.
    fn analyze_project_only(src: &str) -> (crate::project::ProjectAnalysis, Uri) {
        use greycat_analyzer_core::SourceManager;
        use std::str::FromStr;
        let uri = Uri::from_str("file:///proj/main.gcl").unwrap();
        let mut mgr = SourceManager::new();
        mgr.add_simple(uri.clone(), src, "project", false);
        (crate::project::ProjectAnalysis::analyze(&mgr), uri)
    }

    /// `(expr_runtime_types.len(), def_runtime_types.len())` for the
    /// single module in `src`, run through the project pipeline so the
    /// built-in `Array` / `Map` / … decls are seeded (the bare per-module
    /// walk has no stdlib, so `Array<T>` wouldn't lower to a `Generic`).
    fn erasure_table_sizes(src: &str) -> (usize, usize) {
        use greycat_analyzer_core::SourceManager;
        use std::str::FromStr;
        let mut mgr = SourceManager::new();
        let uri = Uri::from_str("file:///mod.gcl").unwrap();
        mgr.add_simple(uri.clone(), src, "project", false);
        let pa = crate::project::ProjectAnalysis::analyze(&mgr);
        let m = pa.module(&uri).unwrap();
        (
            m.analysis.expr_runtime_types.len(),
            m.analysis.def_runtime_types.len(),
        )
    }

    #[test]
    fn erasing_call_records_runtime_type() {
        // A local generic fn that constructs and returns `Box<T>` erases
        // at runtime; the call result and the var it's bound to both
        // carry the erased runtime shape. (User-defined `Box` keeps the
        // test hermetic — the built-in `Array` only resolves with the
        // stdlib loaded.)
        let (exprs, defs) = erasure_table_sizes(
            "type Box<T> { item: T; }\n\
             fn wrap<T>(x: T): Box<T> { return Box<T> { item: x }; }\n\
             fn main() { var r = wrap(1); }\n",
        );
        assert!(
            exprs > 0,
            "erasing call should record a runtime-erased type"
        );
        assert!(
            defs > 0,
            "var bound to an erasing call should carry the taint"
        );
    }

    #[test]
    fn honored_call_records_no_runtime_type() {
        // Pass-through generic fn forwards what it got — no new erasure,
        // so nothing diverges from the materialized type.
        let (exprs, _defs) = erasure_table_sizes(
            "type Box<T> { item: T; }\n\
             fn id<T>(x: Box<T>): Box<T> { return x; }\n\
             fn main() { var b = Box<int> { item: 1 }; var r = id(b); }\n",
        );
        assert_eq!(
            exprs, 0,
            "pass-through call must not record a runtime divergence"
        );
    }

    /// Project-aware variant — exercises the full pipeline including
    /// `validate_type_relations`. Tests that assert type-relation
    /// diagnostics MUST go through this path; the per-module
    /// `analyze_src` no longer emits them (intentional, see
    /// `DiagCategory`).
    fn analyze_project_src(src: &str) -> Vec<crate::analyzer::SemanticDiagnostic> {
        use greycat_analyzer_core::SourceManager;
        use std::str::FromStr;
        let mut mgr = SourceManager::new();
        let uri = Uri::from_str("file:///mod.gcl").unwrap();
        mgr.add_simple(uri.clone(), src, "project", false);
        let pa = crate::project::ProjectAnalysis::analyze(&mgr);
        pa.module(&uri).unwrap().analysis.diagnostics.clone()
    }

    #[test]
    fn clean_function_no_diagnostics() {
        let r = analyze_src_only("fn add(a: int, b: int): int { return a + b; }\n");
        assert!(r.diagnostics.is_empty(), "unexpected: {:?}", r.diagnostics);
    }

    #[test]
    fn return_type_mismatch_surfaces() {
        // Type-relation diagnostic — runs through the project
        // pipeline's `validate_type_relations` post-pass.
        let diags = analyze_project_src("fn bad(): int { return \"hi\"; }\n");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("not assignable to declared return type")),
            "expected return-type error, got: {diags:?}"
        );
    }

    #[test]
    fn if_condition_must_be_bool() {
        // GreyCat's `if` requires parentheses (`if (cond) { ... }`).
        // Type-relation diagnostic — runs through the project pipeline.
        let diags =
            analyze_project_src("fn f(x: int): int { if (x) { return 1; } else { return 0; } }\n");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("if condition must be `bool`")),
            "expected condition error, got: {diags:?}"
        );
    }

    #[test]
    fn unresolved_name_promoted_to_diagnostic() {
        let r = analyze_src_only("fn f(): int { return missing; }\n");
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.message.contains("unresolved")),
            "expected unresolved-name diag, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn cast_rejects_invalid_string_to_int() {
        // P12.3: `String as int` is rejected by the GreyCat cast rules.
        // The expression's type still becomes `int` (so downstream
        // inference doesn't cascade), but a diagnostic surfaces.
        let src = r#"
fn f(s: String): int { return s as int; }
"#;
        let r = analyze_src_only(src);
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.message.contains("cannot cast")),
            "expected cast diagnostic, got: {:?}",
            r.diagnostics,
        );
    }

    #[test]
    fn cast_int_to_node_tag_is_allowed() {
        // P12.3: `int as nodeTime<T>` is one of the asymmetric promotion
        // rules — int casts to any of the node-tag heads.
        let src = r#"
fn f(i: int): nodeTime { return i as nodeTime; }
"#;
        let r = analyze_src_only(src);
        assert!(
            r.diagnostics
                .iter()
                .all(|d| !d.message.contains("cannot cast")),
            "did not expect cast diagnostic, got: {:?}",
            r.diagnostics,
        );
    }

    #[test]
    fn cast_nullable_node_to_int_is_allowed() {
        // Every node handle is a u64 at runtime, so `node<int> as int` and
        // its nullability variants are valid (`node<int>? as int?` -> int,
        // verified via `greycat run`).
        let src = r#"
fn a(n: node<int>): int { return n as int; }
fn b(n: node<int>?): int? { return n as int?; }
fn c(n: nodeTime?): int? { return n as int?; }
"#;
        let r = analyze_src_only(src);
        assert!(
            r.diagnostics
                .iter()
                .all(|d| !d.message.contains("cannot cast")),
            "did not expect cast diagnostic, got: {:?}",
            r.diagnostics,
        );
    }

    #[test]
    fn cast_nullable_primitive_conversion_is_allowed() {
        // `int? as float?` is allowed: the `int <-> float` conversion is
        // runtime-checked through nullability (`null as float?` -> null,
        // `42 as float?` -> 42.0, verified via `greycat run`).
        let src = r#"
fn nn(x: int?): float? { return x as float?; }
"#;
        let r = analyze_src_only(src);
        assert!(
            r.diagnostics
                .iter()
                .all(|d| !d.message.contains("cannot cast")),
            "did not expect cast diagnostic, got: {:?}",
            r.diagnostics,
        );
    }

    #[test]
    fn generic_call_inference_substitutes_return_type() {
        // P12.1: `id<T>(x: T): T` called with `id(1)` should produce
        // an `int`-typed call expression, not `any`.
        let src = r#"
fn id<T>(x: T): T { return x; }
fn caller(): int { return id(1); }
"#;
        let r = analyze_src_only(src);
        assert!(
            r.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            r.diagnostics,
        );
    }

    #[test]
    fn generic_call_inference_reports_witness_conflict() {
        // P12.1: `pair<T>(a: T, b: T): T` called with `pair(1, "s")`
        // should emit a `cannot infer T` conflict diagnostic.
        let src = r#"
fn pair<T>(a: T, b: T): T { return a; }
fn caller() { pair(1, "s"); }
"#;
        let r = analyze_src_only(src);
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.message.contains("cannot infer")),
            "expected witness-conflict diag, got: {:?}",
            r.diagnostics,
        );
    }

    #[test]
    fn binary_arith_widens_to_float() {
        let src = "fn f(a: int, b: float): float { return a + b; }\n";
        let r = analyze_src_only(src);
        assert!(r.diagnostics.is_empty(), "unexpected: {:?}", r.diagnostics);
    }

    /// End-to-end anchor: a bare-`f` float literal flowing into a
    /// `float`-typed parameter must not raise an assignability
    /// diagnostic. (Old `numeric_literal_kind` unit tests were
    /// retired when the float-vs-int dispatch moved into HIR lowering
    /// alongside the typed [`LiteralKind`] variants — this end-to-end
    /// check now anchors the same invariant.)
    #[test]
    fn bare_f_suffix_assigns_to_float_parameter() {
        let src = "fn main() { foo(2f); }\nnative fn foo(_: float) {}\n";
        let r = analyze_src_only(src);
        assert!(r.diagnostics.is_empty(), "unexpected: {:?}", r.diagnostics);
    }

    #[test]
    fn member_access_binds_to_type_attr() {
        let src = r#"
type Point {
    x: int;
    y: int;
}

fn first(p: Point): int { return p.x; }
"#;
        let tree = parse(src);
        let symbols = SymbolTable::new();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let mut arena = TypeArena::new(&symbols);
        let index = ProjectIndex::new(symbols, &arena);
        let res = index.resolutions(&hir, None, None);
        let (_decl_registry, analysis) = analyze_with_index(&hir, &res, &mut arena, &index);

        // Find the property ident `x` inside `p.x` — the second `x`
        // ident in the source (the first is the attr decl name).
        let x_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| &index.symbols[i.symbol] == "x")
            .map(|(idx, _)| idx)
            .collect();
        assert_eq!(x_uses.len(), 2, "expected attr decl + member use");

        // The use site is the second `x` (later byte_range).
        let mut sorted = x_uses.clone();
        sorted.sort_by_key(|idx| hir.idents[*idx].byte_range.start);
        let property = sorted[1];

        let member = analysis
            .member_lookup(property)
            .expect("member binding for p.x");
        assert!(matches!(member, MemberDef::Attr(_)));
    }

    #[test]
    fn arrow_access_binds_to_type_attr() {
        let src = r#"
type Box {
    inner: int;
}

fn read(b: Box): int { return b->inner; }
"#;
        let tree = parse(src);
        let symbols = SymbolTable::new();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let mut arena = TypeArena::new(&symbols);
        let index = ProjectIndex::new(symbols, &arena);
        let res = index.resolutions(&hir, None, None);
        let (_decl_registry, analysis) = analyze_with_index(&hir, &res, &mut arena, &index);

        let inner_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| &index.symbols[i.symbol] == "inner")
            .map(|(idx, _)| idx)
            .collect();
        assert_eq!(inner_uses.len(), 2);
        let mut sorted = inner_uses.clone();
        sorted.sort_by_key(|idx| hir.idents[*idx].byte_range.start);
        let property = sorted[1];

        assert!(matches!(
            analysis.member_lookup(property),
            Some(MemberDef::Attr(_))
        ));
    }

    #[test]
    fn null_neq_narrows_then_branch() {
        // `if (x != null) { use(x) }` — inside the then-branch x is
        // non-null, so passing it to a slot expecting non-null int
        // shouldn't error.
        let src = r#"
fn use_int(v: int) {}
fn f(x: int?) {
    if (x != null) {
        use_int(x);
    }
}
"#;
        let r = analyze_src_only(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected no nullability error inside narrowed then-branch, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn null_eq_narrows_else_branch() {
        // `if (x == null) { ... } else { use(x) }` narrows x in else.
        let src = r#"
fn use_int(v: int) {}
fn f(x: int?) {
    if (x == null) {
    } else {
        use_int(x);
    }
}
"#;
        let r = analyze_src_only(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected no nullability error inside narrowed else-branch, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn conjunctive_narrowing_then_branch() {
        // P13.2: `if (x != null && y != null) { use(x); use(y); }` —
        // both x and y narrowed to non-null in the then-branch.
        let src = r#"
fn use_int(v: int) {}
fn f(x: int?, y: int?) {
    if (x != null && y != null) {
        use_int(x);
        use_int(y);
    }
}
"#;
        let r = analyze_src_only(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected no nullability error in conjunctive then-branch, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn conjunctive_operand_narrows_inside_and() {
        // P13.2-followup: `if (x != null && f(x))` — the second operand
        // of `&&` runs only when the first held, so `f(x)` should see
        // `x` narrowed to non-null. Without the followup the analyzer
        // emitted `value of type \`int?\` is not assignable to parameter
        // \`v: int\`` on the call inside the conjunction.
        let src = r#"
fn use_int(v: int): bool { return true; }
fn f(x: int?) {
    if (x != null && use_int(x)) {}
}
"#;
        let r = analyze_src_only(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected no nullability error inside the && right operand, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn disjunctive_operand_narrows_inside_or() {
        // P13.2-followup: `if (x == null || f(x))` — the second operand
        // of `||` runs only when the first failed (i.e. `x` is non-null
        // there). Mirror of the && case.
        let src = r#"
fn use_int(v: int): bool { return true; }
fn f(x: int?) {
    if (x == null || use_int(x)) {}
}
"#;
        let r = analyze_src_only(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected no nullability error inside the || right operand, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn disjunctive_narrowing_else_branch() {
        // P13.2: `if (x == null || y == null) { } else { use(x); use(y); }` —
        // both narrowed to non-null in the else-branch.
        let src = r#"
fn use_int(v: int) {}
fn f(x: int?, y: int?) {
    if (x == null || y == null) {
    } else {
        use_int(x);
        use_int(y);
    }
}
"#;
        let r = analyze_src_only(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected no nullability error in disjunctive else-branch, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn early_return_narrows_post_if_scope() {
        // P13.1: `if (x == null) { return; } use_int(x);` — after
        // the early-return then-branch, `x` is non-null in the rest
        // of the enclosing block.
        let src = r#"
fn use_int(v: int) {}
fn f(x: int?) {
    if (x == null) {
        return;
    }
    use_int(x);
}
"#;
        let r = analyze_src_only(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected no nullability error after early-return narrowing, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn early_throw_narrows_post_if_scope() {
        // P13.1 mirror: `throw` also terminates the then-branch.
        let src = r#"
fn use_int(v: int) {}
fn f(x: int?) {
    if (x == null) {
        throw "oops";
    }
    use_int(x);
}
"#;
        let r = analyze_src_only(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected no nullability error after early-throw narrowing, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn non_null_assert_narrows_rest_of_block() {
        // `x!!;` propagates non-null to subsequent uses of x in the
        // same block. Without P6.4 narrowing, the second `use_int(x)`
        // would error.
        let src = r#"
fn use_int(v: int) {}
fn f(x: int?) {
    use_int(x!!);
    use_int(x);
}
"#;
        let r = analyze_src_only(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected no nullability error after `x!!`, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn is_guard_narrows_then_branch() {
        let src = r#"
type Foo {}
fn use_foo(f: Foo) {}
fn dispatch(x: any) {
    if (x is Foo) {
        use_foo(x);
    }
}
"#;
        let r = analyze_src_only(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected `is`-narrowed `x` to satisfy `Foo` arg, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn as_cast_adopts_target_type() {
        let src = r#"
type Foo {}
fn use_foo(f: Foo) {}
fn dispatch(x: any) {
    use_foo(x as Foo);
}
"#;
        let r = analyze_src_only(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected `as Foo` to type as Foo, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn non_exhaustive_enum_chain_records_finding() {
        let src = r#"
enum Color { Red, Green, Blue }
fn pick(c: Color): int {
    if (c == Color::Red) {
        return 1;
    } else if (c == Color::Green) {
        return 2;
    }
    return 0;
}
"#;
        let (pa, uri) = analyze_project_only(src);
        let r = &pa.module(&uri).unwrap().analysis;
        // Recording happens during analysis; the lint pipeline turns
        // this into a `non-exhaustive` LintDiagnostic later.
        assert_eq!(
            r.non_exhaustive_findings.len(),
            1,
            "expected one non-exhaustive finding, got: {:?}",
            r.non_exhaustive_findings
        );
        let finding = &r.non_exhaustive_findings[0];
        assert_eq!(pa.symbol(&finding.enum_name), "Color");
        assert_eq!(
            finding
                .missing
                .iter()
                .map(|s| pa.symbol(s))
                .collect::<Vec<_>>(),
            vec!["Blue"]
        );
        // The legacy `SemanticDiagnostic` channel must not also fire.
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("non-exhaustive")),
            "non-exhaustive must no longer ride the structural channel, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn lone_if_enum_eq_is_silent() {
        // **P20.3** — a lone `if (x == E::V) { ... }` (no `else if`,
        // no final `else`) is not a match-like dispatch and should
        // not flag exhaustiveness. The canonical pattern is sequential
        // `if (x == E::A) { return ...; } if (x == E::B) { return
        // ...; } ... return fallback;` where each `if` stands alone.
        let src = r#"
enum Color { Red, Green, Blue }
fn pick(c: Color): int {
    if (c == Color::Red) {
        return 1;
    }
    if (c == Color::Green) {
        return 2;
    }
    if (c == Color::Blue) {
        return 3;
    }
    return 0;
}
"#;
        let r = analyze_src_only(src);
        assert!(
            r.non_exhaustive_findings.is_empty(),
            "lone `if (x == E::V)` should not flag exhaustiveness, got: {:?}",
            r.non_exhaustive_findings
        );
    }

    #[test]
    fn exhaustive_enum_chain_silent() {
        let src = r#"
enum Color { Red, Green, Blue }
fn pick(c: Color): int {
    if (c == Color::Red) {
        return 1;
    } else if (c == Color::Green) {
        return 2;
    } else if (c == Color::Blue) {
        return 3;
    }
    return 0;
}
"#;
        let (pa, uri) = analyze_project_only(src);
        let r = &pa.module(&uri).unwrap().analysis;
        assert!(
            r.non_exhaustive_findings.is_empty(),
            "expected no exhaustiveness finding, got: {:?}",
            r.non_exhaustive_findings
        );
    }

    #[test]
    fn final_else_makes_chain_exhaustive() {
        let src = r#"
enum Color { Red, Green, Blue }
fn pick(c: Color): int {
    if (c == Color::Red) {
        return 1;
    } else {
        return 0;
    }
}
"#;
        let (pa, uri) = analyze_project_only(src);
        let r = &pa.module(&uri).unwrap().analysis;
        assert!(
            r.non_exhaustive_findings.is_empty(),
            "expected final-else to suppress finding, got: {:?}",
            r.non_exhaustive_findings
        );
    }

    #[test]
    fn cross_module_enum_chain_records_finding() {
        // Regression: a non-exhaustive chain over an enum declared in a
        // *different* module must still flag. The exhaustiveness check
        // used to resolve the enum through the module-local registry, so
        // a cross-module enum (std `@library` / `@include`d) was silently
        // never checked.
        use greycat_analyzer_core::SourceManager;
        use std::str::FromStr;
        let defs_uri = Uri::from_str("file:///proj/defs.gcl").unwrap();
        let main_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
        let mut mgr = SourceManager::new();
        mgr.add_simple(
            defs_uri,
            "enum Color { Red, Green, Blue }\n",
            "project",
            false,
        );
        mgr.add_simple(
            main_uri.clone(),
            "fn pick(c: Color): int {\n\
                 if (c == Color::Red) { return 1; }\n\
                 else if (c == Color::Green) { return 2; }\n\
                 return 0;\n\
             }\n",
            "project",
            false,
        );
        let pa = crate::project::ProjectAnalysis::analyze(&mgr);
        let r = &pa.module(&main_uri).unwrap().analysis;
        assert_eq!(
            r.non_exhaustive_findings.len(),
            1,
            "cross-module enum chain must flag, got: {:?}",
            r.non_exhaustive_findings
        );
        assert_eq!(
            r.non_exhaustive_findings[0]
                .missing
                .iter()
                .map(|s| pa.symbol(s))
                .collect::<Vec<_>>(),
            vec!["Blue"]
        );
    }

    // P16.1
    /// `Expr::Member` resolving to an `Attr` reports the
    /// attr's declared type as the expression type, not `any`. Closes
    /// the project.gcl bug where `var s = x.s.size();` typed `x.s` as
    /// `any` even though `s: String` was bound.
    #[test]
    fn member_attr_typing_matches_attr_decl_type() {
        let src = r#"
type Foo { s: String; }
fn f(x: Foo): String { return x.s; }
"#;
        let r = analyze_src_only(src);
        assert!(
            r.diagnostics.is_empty(),
            "x.s should type as String matching the return type, got diagnostics: {:?}",
            r.diagnostics
        );
    }

    // P16.1
    /// `Expr::Member` resolving to a `Method` reports
    /// `function`-typed (gcl's first-class function type).
    #[test]
    fn member_method_ref_types_as_function() {
        // `function` is a runtime type — declared as `native type
        // function` in `lib/std/core.gcl`. The per-file resolver
        // (no stdlib ingest) needs an inline declaration so the
        // name resolves.
        let src = r#"
native type function {}
type Foo { fn run(): int { return 0; } }
fn caller(x: Foo): function { return x.run; }
"#;
        let r = analyze_src_only(src);
        assert!(
            r.diagnostics.is_empty(),
            "x.run (no call) should type as `function`, got diagnostics: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn member_access_unknown_property_has_no_binding() {
        let src = r#"
type Point { x: int; }
fn f(p: Point): int { return p.bogus; }
"#;
        let tree = parse(src);
        let symbols = SymbolTable::new();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let mut arena = TypeArena::new(&symbols);
        let index = ProjectIndex::new(symbols, &arena);
        let res = index.resolutions(&hir, None, None);
        let (_decl_registry, analysis) = analyze_with_index(&hir, &res, &mut arena, &index);

        let bogus = hir
            .idents
            .iter()
            .find(|(_, i)| &index.symbols[i.symbol] == "bogus")
            .map(|(idx, _)| idx)
            .expect("bogus ident exists");
        assert!(analysis.member_lookup(bogus).is_none());
    }

    // -------------------------------------------------------------------
    // P16.7 — null-safe access notations + `??` widening
    // -------------------------------------------------------------------

    /// Resolve the inferred type for the `init` of `var <name> = …`.
    /// Routes through the project pipeline so user-decl names render
    /// through the registry-aware `display_type`.
    fn local_init_ty(src: &str, name: &str) -> Option<String> {
        use greycat_analyzer_core::SourceManager;
        use std::str::FromStr;
        let mut mgr = SourceManager::new();
        let src_uri = Uri::from_str("file:///mod.gcl").unwrap();
        mgr.add_simple(src_uri.clone(), src, "project", false);
        let pa = crate::project::ProjectAnalysis::analyze(&mgr);
        let module = pa.module(&src_uri)?;
        for (_id, stmt) in module.hir.stmts.iter() {
            if let Stmt::Var(v) = stmt
                && pa.symbol(&module.hir.idents[v.name].symbol) == name
                && let Some(init) = v.init
            {
                let ty = module.analysis.expr_types.get(&init).copied()?;
                return Some(pa.display_type(ty).to_string());
            }
        }
        None
    }

    /// Project-pipeline variant of [`local_init_ty`] that loads a
    /// synthetic stdlib fixture before analyzing `src`. Use it when
    /// the test exercises behavior gated on stdlib annotations
    /// (`@deref("resolve")` on `node<T>`, `@iterable` on `Array<T>`,
    /// …) — those flags only land in the project index after the
    /// declaring module is ingested.
    fn local_init_ty_with_stdlib(stdlib_src: &str, src: &str, name: &str) -> Option<String> {
        use greycat_analyzer_core::SourceManager;
        use std::str::FromStr;
        let mut mgr = SourceManager::new();
        let stdlib_uri = Uri::from_str("file:///std/core.gcl").unwrap();
        mgr.add_simple(stdlib_uri, stdlib_src, "std", false);
        let src_uri = Uri::from_str("file:///mod.gcl").unwrap();
        mgr.add_simple(src_uri.clone(), src, "project", false);
        let pa = crate::project::ProjectAnalysis::analyze(&mgr);
        let module = pa.module(&src_uri)?;
        for (_id, stmt) in module.hir.stmts.iter() {
            if let Stmt::Var(v) = stmt
                && pa.symbol(&module.hir.idents[v.name].symbol) == name
                && let Some(init) = v.init
            {
                let ty = module.analysis.expr_types.get(&init).copied()?;
                return Some(pa.display_type(ty).to_string());
            }
        }
        None
    }

    /// Minimal synthetic `lib/std/core.gcl` covering the runtime
    /// names that show up in test fixtures: primitives + `node<T>`
    /// with its `@deref("resolve")` annotation + `resolve(): T`.
    /// Use with [`local_init_ty_with_stdlib`] when the test
    /// exercises arrow-deref / `@deref`-driven typing.
    fn synthetic_std_core() -> &'static str {
        "native type any {}\n\
         native type null {}\n\
         native type bool {}\n\
         native type char {}\n\
         native type int {}\n\
         native type float {}\n\
         native type String {}\n\
         native type time {}\n\
         native type duration {}\n\
         native type geo {}\n\
         native type type {}\n\
         native type field {}\n\
         native type function {}\n\
         @deref(\"resolve\")\n\
         native type node<T> {\n    fn resolve(): T;\n}\n\
         @deref(\"resolve\")\n\
         native type nodeTime<T> {\n    fn resolve(): T;\n}\n\
         @deref(\"resolve\")\n\
         native type nodeList<T> {\n    fn resolve(): T;\n}\n\
         @deref(\"resolve\")\n\
         native type nodeGeo<T> {\n    fn resolve(): T;\n}\n\
         native type nodeIndex<K, V> {}\n\
         native type Array<T> {}\n\
         native type Map<K, V> {}\n\
         type Tuple<T, U> { a: T; b: U; }\n"
    }

    #[test]
    fn p16_7_question_dot_on_nullable_lifts_result() {
        // `f?.name` where `f: Foo?` — result is `String?`. The receiver
        // is nullable so the null-safe access propagates.
        let src = r#"
type Foo { name: String; }
fn caller(f: Foo?) {
    var s = f?.name;
}
"#;
        assert_eq!(local_init_ty(src, "s").as_deref(), Some("String?"));
    }

    #[test]
    fn p16_7_question_dot_on_non_nullable_is_noop() {
        // `f?.name` where `f: Foo` (non-nullable) — the marker is
        // syntactic sugar; result stays `String`.
        let src = r#"
type Foo { name: String; }
fn caller(f: Foo) {
    var s = f?.name;
}
"#;
        assert_eq!(local_init_ty(src, "s").as_deref(), Some("String"));
    }

    #[test]
    fn p16_7_post_question_lifts_unconditionally() {
        // `f.name?` — explicit "treat as nullable" suffix. Even though
        // `name: String` is non-null, the suffix lifts the result.
        let src = r#"
type Foo { name: String; }
fn caller(f: Foo) {
    var s = f.name?;
}
"#;
        assert_eq!(local_init_ty(src, "s").as_deref(), Some("String?"));
    }

    #[test]
    fn p16_7_question_arrow_on_nullable_node_lifts() {
        // `n?->name` for `n: node<Foo>?` — null-safe access through
        // the deref. Result lifts to `String?` because the receiver
        // is nullable. Needs stdlib loaded because the analyzer
        // looks up the `@deref("resolve")` annotation on `node<T>`'s
        // decl (in `lib/std/core.gcl`) to know that `*n` / `n->m`
        // desugars to `n.resolve().m`.
        let src = r#"
type Foo { name: String; }
fn caller(n: node<Foo>?) {
    var s = n?->name;
}
"#;
        assert_eq!(
            local_init_ty_with_stdlib(synthetic_std_core(), src, "s").as_deref(),
            Some("String?"),
        );
    }

    // P19.17 — receiver-nullability lift for *method calls* (the
    // call-typing analog of the `p16_7_question_*` field-access lifts
    // above). `recvExpr?.m()` where `recvExpr: T?` yields `Ret?`: the
    // chain shorts to null when the receiver is null. The three tests
    // below cover the three ways the nullable receiver of the trailing
    // `?.resolve()` is produced — instance `.` call, static `::` call,
    // and arrow `->` call — all returning `node<UserDetails>?`, then
    // `?.resolve()` (where `resolve(): T` returns the generic param)
    // must lift `UserDetails` to `UserDetails?`. The generic-return
    // method is what makes these distinct from the field-access tests:
    // the call routes through `run_method_generic_inference`, which
    // must honor the same lift as the non-generic call path.

    #[test]
    fn chained_call_lifts_nullable_through_instance_method() {
        // `s.loadMethod()?.resolve()` — `loadMethod(): node<UserDetails>?`
        // so the `?.resolve()` receiver is nullable. Result is
        // `UserDetails?`.
        let src = r#"
type UserDetails {}
type UserService {
    native fn loadMethod(): node<UserDetails>?;
}
fn foo(s: UserService) {
    var d = s.loadMethod()?.resolve();
}
"#;
        assert_eq!(
            local_init_ty_with_stdlib(synthetic_std_core(), src, "d").as_deref(),
            Some("UserDetails?"),
        );
    }

    #[test]
    fn chained_call_lifts_nullable_through_static_method() {
        // `UserService::loadSomething(42)?.resolve()` — the nullable
        // receiver of `?.resolve()` is a static method call.
        let src = r#"
type UserDetails {}
type UserService {
    native static fn loadSomething(id: int): node<UserDetails>?;
}
fn foo() {
    var d = UserService::loadSomething(42)?.resolve();
}
"#;
        assert_eq!(
            local_init_ty_with_stdlib(synthetic_std_core(), src, "d").as_deref(),
            Some("UserDetails?"),
        );
    }

    #[test]
    fn chained_call_lifts_nullable_through_arrow_method() {
        // `ns->loadMethod()?.resolve()` — the nullable receiver of
        // `?.resolve()` is an arrow (`->`) method call; `ns: node<…>`
        // derefs to `UserService` before `loadMethod` binds.
        let src = r#"
type UserDetails {}
type UserService {
    native fn loadMethod(): node<UserDetails>?;
}
fn foo(ns: node<UserService>) {
    var d = ns->loadMethod()?.resolve();
}
"#;
        assert_eq!(
            local_init_ty_with_stdlib(synthetic_std_core(), src, "d").as_deref(),
            Some("UserDetails?"),
        );
    }

    #[test]
    fn p16_7_coalesce_same_shape_collapses() {
        // `T? ?? T → T`. `int? ?? int` collapses to `int` (no union).
        let src = r#"
fn caller(x: int?) {
    var y = x ?? 7;
}
"#;
        assert_eq!(local_init_ty(src, "y").as_deref(), Some("int"));
    }

    #[test]
    fn p16_7_coalesce_distinct_shapes_widen_to_union() {
        // `T? ?? U → T | U`. Different shapes on each side widen to
        // a 2-alt union (formerly the analyzer dropped the left and
        // returned `U` only — false-precision in the assignability
        // checker).
        let src = r#"
type Foo {}
type Bar {}
fn caller(f: Foo?, b: Bar) {
    var x = f ?? b;
}
"#;
        let display = local_init_ty(src, "x").expect("init type");
        // Order is left-then-right; `display` joins union alts with
        // ` | `.
        assert_eq!(display, "Foo | Bar");
    }

    /// Object expression with all required attrs missing emits an
    /// error naming each. Mirrors the runtime which rejects
    /// `Foo {}` when `Foo` declares non-nullable, non-default
    /// fields.
    #[test]
    fn object_expr_missing_required_attrs_errors() {
        let src = "type Foo { a: int; b: String?; }\nfn main() { var _ = Foo {}; }\n";
        let diags = analyze_project_src(src);
        let hit = diags
            .iter()
            .find(|d| d.message.contains("missing required field"))
            .unwrap_or_else(|| panic!("expected missing-required-field diag: {diags:?}"));
        assert!(
            hit.message.contains("`a`"),
            "diagnostic should name `a`: {}",
            hit.message
        );
        assert!(
            !hit.message.contains("`b`"),
            "nullable `b` should not appear: {}",
            hit.message
        );
    }

    /// `Foo {}` against a type whose every attr is nullable produces
    /// no missing-field diagnostic. GreyCat forbids initializers on
    /// non-static attrs (caught by `non-static-attr-initializer` at
    /// parse-shape time), so "has a default" is not a way for an
    /// instance attr to opt out of required-ness — only `T?` is.
    #[test]
    fn object_expr_all_optional_no_error() {
        let src = "type Foo { a: int?; b: String?; }\nfn main() { var _ = Foo {}; }\n";
        let diags = analyze_project_src(src);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("missing required field")),
            "no missing-field diag expected: {diags:?}"
        );
    }

    /// Supplying the missing attr by name silences the diagnostic.
    #[test]
    fn object_expr_supplied_required_attr_no_error() {
        let src = "type Foo { a: int; }\nfn main() { var _ = Foo { a: 1 }; }\n";
        let diags = analyze_project_src(src);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("missing required field")),
            "no missing-field diag expected: {diags:?}"
        );
    }

    /// Static attrs aren't part of the per-instance schema and must
    /// not be counted as required — even when they carry an initializer
    /// (the only attr kind that legally can). Test pairs a static-with-
    /// init against a required instance attr `b`, then asserts the
    /// diagnostic names `b` but not the static `k`.
    #[test]
    fn object_expr_static_attr_not_required() {
        let src =
            "type Foo { static k: int = 0; a: int?; b: int; }\nfn main() { var _ = Foo {}; }\n";
        let diags = analyze_project_src(src);
        let hit = diags
            .iter()
            .find(|d| d.message.contains("missing required field"))
            .unwrap_or_else(|| panic!("expected diag for `b`: {diags:?}"));
        assert!(
            hit.message.contains("`b`"),
            "should name `b`: {}",
            hit.message
        );
        assert!(
            !hit.message.contains("`k`"),
            "static `k` should not appear: {}",
            hit.message
        );
    }

    /// A supplied field whose name isn't declared (instance) on the
    /// type or any supertype is an `unknown-field` error.
    #[test]
    fn object_expr_unknown_field_errors() {
        let src = "type Foo { a: int?; }\nfn main() { var _ = Foo { a: 1, oops: 2 }; }\n";
        let diags = analyze_project_src(src);
        let hit = diags
            .iter()
            .find(|d| d.message.contains("unknown field"))
            .unwrap_or_else(|| panic!("expected unknown-field diag: {diags:?}"));
        assert!(
            hit.message.contains("`oops`"),
            "should name `oops`: {}",
            hit.message
        );
        assert!(
            hit.message.contains("`Foo`"),
            "should name `Foo`: {}",
            hit.message
        );
    }

    /// Inherited attrs count as known — supplying a parent attr on a
    /// child instance is fine, only truly-unknown names fire.
    #[test]
    fn object_expr_inherited_field_is_known() {
        let src = "type Animal { name: String?; }\n\
                   type Dog extends Animal { breed: String?; }\n\
                   fn main() { var _ = Dog { name: \"rex\", breed: \"lab\" }; }\n";
        let diags = analyze_project_src(src);
        assert!(
            !diags.iter().any(|d| d.message.contains("unknown field")),
            "inherited `name` should be known: {diags:?}"
        );
    }

    /// Static attrs aren't assignable via object syntax — naming one
    /// in `Foo { k: 1 }` is an unknown-field error in this context.
    #[test]
    fn object_expr_static_field_is_unknown_in_instance_construction() {
        let src = "type Foo { static k: int = 0; a: int?; }\n\
                   fn main() { var _ = Foo { k: 1 }; }\n";
        let diags = analyze_project_src(src);
        let hit = diags
            .iter()
            .find(|d| d.message.contains("unknown field"))
            .unwrap_or_else(|| panic!("expected unknown-field diag: {diags:?}"));
        assert!(
            hit.message.contains("`k`"),
            "should name `k`: {}",
            hit.message
        );
    }

    /// `Sub extends Super` inherits `Super`'s required attrs. `Sub {}`
    /// must complain about both the inherited and the own-declared
    /// required fields.
    #[test]
    fn object_expr_missing_inherited_required_attrs_errors() {
        let src = "type Animal { name: String; }\n\
                   type Dog extends Animal { breed: String; }\n\
                   fn main() { var _ = Dog {}; }\n";
        let diags = analyze_project_src(src);
        let hit = diags
            .iter()
            .find(|d| d.message.contains("missing required field"))
            .unwrap_or_else(|| panic!("expected missing-required-field diag: {diags:?}"));
        assert!(
            hit.message.contains("`breed`"),
            "should name own-declared `breed`: {}",
            hit.message
        );
        assert!(
            hit.message.contains("`name`"),
            "should name inherited `name`: {}",
            hit.message
        );
    }

    /// Supplying the inherited required attr (but not the own) only
    /// names the own one in the diagnostic.
    #[test]
    fn object_expr_inherited_required_attr_supplied() {
        let src = "type Animal { name: String; }\n\
                   type Dog extends Animal { breed: String; }\n\
                   fn main() { var _ = Dog { name: \"rex\" }; }\n";
        let diags = analyze_project_src(src);
        let hit = diags
            .iter()
            .find(|d| d.message.contains("missing required field"))
            .unwrap_or_else(|| panic!("expected diag: {diags:?}"));
        assert!(
            hit.message.contains("`breed`"),
            "should name `breed`: {}",
            hit.message
        );
        assert!(
            !hit.message.contains("`name`"),
            "should not name supplied `name`: {}",
            hit.message
        );
    }

    /// Three-level chain: Sub extends Mid extends Top — every level's
    /// required attrs must surface.
    #[test]
    fn object_expr_three_level_chain_required_attrs() {
        let src = "type Top { a: int; }\n\
                   type Mid extends Top { b: int; }\n\
                   type Sub extends Mid { c: int; }\n\
                   fn main() { var _ = Sub {}; }\n";
        let diags = analyze_project_src(src);
        let hit = diags
            .iter()
            .find(|d| d.message.contains("missing required field"))
            .unwrap_or_else(|| panic!("expected diag: {diags:?}"));
        for name in ["`a`", "`b`", "`c`"] {
            assert!(
                hit.message.contains(name),
                "should name {name}: {}",
                hit.message
            );
        }
    }

    /// Inherited nullable attrs stay optional — the chain walk must
    /// not turn a parent's `T?` into a required field on the child.
    #[test]
    fn object_expr_inherited_nullable_attr_not_required() {
        let src = "type Animal { species: String?; }\n\
                   type Dog extends Animal { breed: String; }\n\
                   fn main() { var _ = Dog { breed: \"lab\" }; }\n";
        let diags = analyze_project_src(src);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("missing required field")),
            "no missing-field diag expected when nullable inherited attr is omitted: {diags:?}"
        );
    }

    #[test]
    fn p16_7_coalesce_with_nullable_right_stays_nullable() {
        // `T? ?? U?` — fallback can still be null, so the whole
        // expression stays nullable.
        let src = r#"
type Foo {}
type Bar {}
fn caller(f: Foo?, b: Bar?) {
    var x = f ?? b;
}
"#;
        let display = local_init_ty(src, "x").expect("init type");
        // Nullable unions render with an explicit `null` alt — the
        // `?` suffix would visually bind to only the last alt
        // (would have looked like `"Foo | Bar?"`).
        assert_eq!(display, "Foo | Bar | null");
    }

    // -------------------------------------------------------------------
    // P41 — union-arm `is`-narrowing complement
    // -------------------------------------------------------------------

    /// Repro shape from the plan: `?? `-produced union with an
    /// `is`-guarded early-return strips the matched arm from the
    /// post-if continuation.
    #[test]
    fn p41_is_narrow_strips_arm_via_then_terminates() {
        let src = r#"
type A {}
type B {}
fn use_a(a: A) {}
fn caller(p: A?, q: B?) {
    var x = p ?? q;
    if (x == null) { return; }
    if (x is B) { return; }
    use_a(x);
}
"#;
        let diags = analyze_project_src(src);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("not assignable") || d.message.contains("cannot cast")),
            "expected zero is-narrow-related diagnostics: {diags:?}"
        );
    }

    /// Explicit else-branch (no early return) gets the same
    /// complement narrow applied at else-entry.
    #[test]
    fn p41_is_narrow_strips_arm_in_else_branch() {
        let src = r#"
type A {}
type B {}
fn use_a(a: A) {}
fn caller(p: A?, q: B?) {
    var x = p ?? q;
    if (x == null) { return; }
    if (x is B) {
        // no-op
    } else {
        use_a(x);
    }
}
"#;
        let diags = analyze_project_src(src);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("not assignable") || d.message.contains("cannot cast")),
            "expected zero is-narrow-related diagnostics: {diags:?}"
        );
    }

    /// `!is` swaps the complement to the then-side. With an
    /// `else { return; }` the complement lifts into the post-if
    /// continuation via the `else_terminates` path.
    #[test]
    fn p41_is_narrow_negated_swaps_to_then_branch() {
        let src = r#"
type A {}
type B {}
fn use_a(a: A) {}
fn caller(p: A?, q: B?) {
    var x = p ?? q;
    if (x == null) { return; }
    if (!(x is B)) {
        use_a(x);
    }
}
"#;
        let diags = analyze_project_src(src);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("not assignable") || d.message.contains("cannot cast")),
            "expected zero is-narrow-related diagnostics on `!is` shape: {diags:?}"
        );
    }

    /// Single-alt collapse: when the subtraction leaves one survivor,
    /// the narrowed type is the bare alt, not a `Union { alts:
    /// [single] }`. Inspect via `local_init_ty` on a post-guard
    /// re-bind.
    #[test]
    fn p41_is_narrow_union_collapse_to_lone_alt() {
        let src = r#"
type A {}
type B {}
fn caller(p: A?, q: B?) {
    var u = p ?? q;
    if (u == null) { return; }
    if (u is B) { return; }
    var x = u;
}
"#;
        // After the two guards `u` should narrow to `A` — a bare
        // type, not a single-element union. The display path renders
        // a `Union { alts: [A] }` as `A` regardless, so we also
        // assert there's no residual nullable wrapper or stray alt.
        assert_eq!(local_init_ty(src, "x").as_deref(), Some("A"));
    }

    /// Non-union source: the helper returns `None` and behavior is
    /// unchanged from before P41. Existing then-side `is`-narrowing
    /// still works.
    #[test]
    fn p41_is_narrow_non_union_source_no_effect() {
        let src = r#"
type Foo { v: int; }
fn caller(f: Foo?) {
    if (f == null) { return; }
    if (f is Foo) {
        var x = f.v;
    }
}
"#;
        let diags = analyze_project_src(src);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("not assignable") || d.message.contains("cannot cast")),
            "non-union source path should be unchanged: {diags:?}"
        );
    }

    /// Asserted type that's a supertype of multiple arms strips all
    /// of them. Documents runtime-faithful behavior: the runtime's
    /// `is T` returns true for any subtype of `T`. When all arms are
    /// subtypes of the asserted type, the subtraction empties out and
    /// the helper returns `None` (exhaustion case — diagnostic
    /// deferred to P42's `exhaustive-is-check`).
    #[test]
    fn p41_is_narrow_asserted_is_supertype_of_arms() {
        let src = r#"
abstract type Animal {}
type Cat extends Animal {}
type Dog extends Animal {}
fn caller(c: Cat?, d: Dog?) {
    var u = c ?? d;
    if (u == null) { return; }
    if (u is Animal) { return; }
    // Unreachable in practice — both arms are subtypes of Animal.
    // We don't lift any complement (helper returns None on zero
    // survivors); no diagnostic regression expected here.
    var _ = u;
}
"#;
        let diags = analyze_project_src(src);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("not assignable") || d.message.contains("cannot cast")),
            "supertype-asserted shape should not regress: {diags:?}"
        );
    }

    // -------------------------------------------------------------------
    // P42 — sealed-hierarchy abstract-decl `is`-narrowing complement
    // -------------------------------------------------------------------

    /// Canonical reproducer (single-leaf-remaining): an abstract root
    /// with exactly two concrete derivatives; `is Rect` else narrows
    /// `s` to `Circle` (the lone survivor).
    #[test]
    fn p42_is_narrow_abstract_strips_to_lone_concrete() {
        let src = r#"
abstract type Shape {}
type Rect extends Shape {}
type Circle extends Shape {}
fn expect_circle(c: Circle) {}
fn caller(s: Shape) {
    if (s is Rect) { return; }
    expect_circle(s);
}
"#;
        let diags = analyze_project_src(src);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("not assignable") || d.message.contains("cannot cast")),
            "expected zero is-narrow-related diagnostics: {diags:?}"
        );
    }

    /// Multi-leaf-remaining: asserting one leaf out of three leaves
    /// `Circle | Triangle` in the post-guard continuation. A call
    /// that expects only `Circle` *should* still flag — confirms the
    /// surviving Union shows up in the diagnostic and that we didn't
    /// accidentally collapse two leaves into the parent abstract.
    #[test]
    fn p42_is_narrow_abstract_multi_leaf_returns_union() {
        let src = r#"
abstract type Shape {}
type Rect extends Shape {}
type Circle extends Shape {}
type Triangle extends Shape {}
fn expect_circle(c: Circle) {}
fn caller(s: Shape) {
    if (s is Rect) { return; }
    expect_circle(s);
}
"#;
        let diags = analyze_project_src(src);
        // Should flag — `s` is `Circle | Triangle` here, not `Circle`.
        // We assert the diagnostic *names the union*, pinning that the
        // narrow surfaced as the multi-arm shape (no spurious
        // collapse).
        let assign_errs: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("not assignable"))
            .collect();
        assert!(
            !assign_errs.is_empty(),
            "expected an assignability error on `Circle | Triangle` → `Circle`: {diags:?}"
        );
        let mentions_union = assign_errs
            .iter()
            .any(|d| d.message.contains("Circle") && d.message.contains("Triangle"));
        assert!(
            mentions_union,
            "expected the diagnostic to surface the surviving union (`Circle | Triangle`): {assign_errs:#?}"
        );
    }

    /// Mandatory ancestor-collapse: when the subtraction set exactly
    /// equals `closure(A)` for some abstract `A`, the narrowed type
    /// renders as `A`, not the explicit union. Strip `Mammal` from
    /// `Animal` → `{Eagle, Penguin, Sparrow}` ≡ `closure(Bird)` →
    /// narrow renders as `Bird`.
    #[test]
    fn p42_is_narrow_abstract_collapses_to_ancestor() {
        let src = r#"
abstract type Animal {}
abstract type Mammal extends Animal {}
abstract type Bird extends Animal {}
type Dog extends Mammal {}
type Cat extends Mammal {}
type Eagle extends Bird {}
type Penguin extends Bird {}
type Sparrow extends Bird {}
fn caller(a: Animal) {
    if (a is Mammal) { return; }
    var x = a;
}
"#;
        // Post-guard `var x = a` reads `a`'s narrowed type via the
        // narrow stack. The complement `closure(Animal) \ closure(Mammal)`
        // = `{Eagle, Penguin, Sparrow}` matches `closure(Bird)` exactly,
        // so the narrow collapses to the bare ancestor `Bird` rather
        // than rendering as a 3-arm union.
        assert_eq!(local_init_ty(src, "x").as_deref(), Some("Bird"));
    }

    /// Concrete root: `Shape` is not declared `abstract`, so the
    /// runtime could legitimately have a `Shape` value at this binding.
    /// `narrow_complement_abstract` returns `None`, the apply sites
    /// are inert, and behavior is unchanged from before P42.
    #[test]
    fn p42_is_narrow_concrete_root_no_effect() {
        let src = r#"
type Shape {}
type Rect extends Shape {}
type Circle extends Shape {}
fn expect_circle(c: Circle) {}
fn caller(s: Shape) {
    if (s is Rect) { return; }
    expect_circle(s);
}
"#;
        let diags = analyze_project_src(src);
        // No collapse — `s` stays as `Shape` in the continuation, so
        // the call to `expect_circle` should still flag.
        let assign_errs: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("not assignable"))
            .collect();
        assert!(
            !assign_errs.is_empty(),
            "concrete root must not lift a narrow (Shape itself is a runtime possibility): {diags:?}"
        );
    }

    /// Asserting the root itself empties the subtraction set
    /// (exhausted). The helper returns `None`; we expect no
    /// diagnostic regression and no spurious narrow into the
    /// (unreachable) continuation. P42.5 will add the
    /// `exhaustive-is-check` warning at this shape.
    #[test]
    fn p42_is_narrow_abstract_exhausted_returns_none() {
        let src = r#"
abstract type Shape {}
type Rect extends Shape {}
type Circle extends Shape {}
fn caller(s: Shape) {
    if (s is Shape) { return; }
    var _ = s;
}
"#;
        let diags = analyze_project_src(src);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("not assignable") || d.message.contains("cannot cast")),
            "exhausted subtraction must not regress: {diags:?}"
        );
    }

    /// Disjunctive `is || is`: in the else of
    /// `if (s is Rect || s is Circle)`, both arms have been ruled
    /// out — narrow `s` to the remaining concrete leaf. Exercises the
    /// `then_typed_union` → `else_typed_id` chained-subtraction path.
    #[test]
    fn p42_is_narrow_disjunctive_is_or_is_else_branch() {
        let src = r#"
abstract type Shape {}
type Rect extends Shape {}
type Circle extends Shape {}
type Triangle extends Shape {}
fn expect_triangle(t: Triangle) {}
fn caller(s: Shape) {
    if (s is Rect || s is Circle) { return; }
    expect_triangle(s);
}
"#;
        let diags = analyze_project_src(src);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("not assignable") || d.message.contains("cannot cast")),
            "expected zero is-narrow-related diagnostics on disjunctive shape: {diags:?}"
        );
    }

    /// `!`-swapped disjunction: `if (!(s is Rect || s is Circle))`
    /// pushes the chained complement into `then_typed_id` via the
    /// `else_typed_union` path. Then-branch narrows `s` to the lone
    /// remaining leaf.
    #[test]
    fn p42_is_narrow_disjunctive_negated_then_branch() {
        let src = r#"
abstract type Shape {}
type Rect extends Shape {}
type Circle extends Shape {}
type Triangle extends Shape {}
fn expect_triangle(t: Triangle) {}
fn caller(s: Shape) {
    if (!(s is Rect || s is Circle)) {
        expect_triangle(s);
    }
}
"#;
        let diags = analyze_project_src(src);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("not assignable") || d.message.contains("cannot cast")),
            "expected zero is-narrow-related diagnostics on `!(is||is)` shape: {diags:?}"
        );
    }

    /// Disjunction with ancestor-collapse: stripping two concrete
    /// leaves from an abstract root with three concrete derivatives
    /// could land on `closure(A)` for some other abstract `A`. Here
    /// `closure(Animal) \ closure(Cat) \ closure(Dog)` = `{Eagle}`
    /// — single leaf, no ancestor match — so the narrow collapses to
    /// `Eagle`. Demonstrates the chained-subtraction composes with
    /// the abstract-decl arm of `narrow_complement`.
    #[test]
    fn p42_is_narrow_disjunctive_chain_collapses() {
        let src = r#"
abstract type Animal {}
type Cat extends Animal {}
type Dog extends Animal {}
type Eagle extends Animal {}
fn caller(a: Animal) {
    if (a is Cat || a is Dog) { return; }
    var x = a;
}
"#;
        assert_eq!(local_init_ty(src, "x").as_deref(), Some("Eagle"));
    }

    /// Disjunction guarded by `&&`: the `&&` arm intentionally leaves
    /// `is_atomic_is = false` (an `&&` else might be either operand
    /// failing — per-binding complements would be unsound). The
    /// disjunctive loop stays inert and the call to `expect_circle`
    /// still flags. Regression guard against accidentally extending
    /// `is_atomic_is` propagation through `&&`.
    #[test]
    fn p42_is_narrow_disjunctive_under_and_no_lift() {
        let src = r#"
abstract type Shape {}
type Rect extends Shape {}
type Circle extends Shape {}
type Triangle extends Shape {}
fn ok(): bool { return true; }
fn expect_triangle(t: Triangle) {}
fn caller(s: Shape) {
    if ((s is Rect || s is Circle) && ok()) { return; }
    expect_triangle(s);
}
"#;
        let diags = analyze_project_src(src);
        let assign_errs: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("not assignable"))
            .collect();
        assert!(
            !assign_errs.is_empty(),
            "`&&` must not lift a disjunctive else-complement (would be unsound); got: {diags:?}"
        );
    }

    // -------------------------------------------------------------------
    // P42.5 — exhaustive-is-check warning
    // -------------------------------------------------------------------

    /// Sealed-hierarchy exhaustion: a `||`-disjunction of `is`-checks
    /// covers every concrete derivative of the abstract root. The
    /// else-branch is unreachable; emit `exhaustive `is`-check`.
    #[test]
    fn p42_exhaustive_is_check_disjunction_covers_hierarchy() {
        let src = r#"
abstract type Shape {}
type Rect extends Shape {}
type Circle extends Shape {}
fn caller(s: Shape) {
    if (s is Rect || s is Circle) {
        var _ = s;
    } else {
        var _ = s;
    }
}
"#;
        let diags = analyze_project_src(src);
        let hit: Vec<_> = diags
            .iter()
            .filter(|d| d.code == "exhaustive-is-check")
            .collect();
        assert_eq!(
            hit.len(),
            1,
            "expected exactly one exhaustive-is-check diagnostic; got: {diags:#?}"
        );
        assert!(
            hit[0].message.contains("else-branch is unreachable"),
            "diagnostic should name the unreachable branch; got: {}",
            hit[0].message
        );
    }

    /// Union source: every alt covered by the disjunction.
    /// Closure-of-asserted equals known's full set → exhausted.
    /// Requires an `else` branch in source — without one the dead
    /// branch doesn't exist and the warning is suppressed.
    #[test]
    fn p42_exhaustive_is_check_union_source() {
        let src = r#"
type A {}
type B {}
fn caller(p: A?, q: B?) {
    var u = p ?? q;
    if (u == null) { return; }
    if (u is A || u is B) {
        var _ = u;
    } else {
        var _ = u;
    }
}
"#;
        let diags = analyze_project_src(src);
        let hit: Vec<_> = diags
            .iter()
            .filter(|d| d.code == "exhaustive-is-check")
            .collect();
        assert_eq!(
            hit.len(),
            1,
            "expected exhaustive-is-check on union covering both alts: {diags:#?}"
        );
    }

    /// No `else` branch on the head if: even when the asserted set
    /// exhausts the known type, there's no dead branch to flag.
    /// `is`-check is just a narrow guard on the then-body — emitting
    /// would point at an imaginary `else` and offer no actionable
    /// signal.
    #[test]
    fn p42_exhaustive_is_check_no_else_silent() {
        let src = r#"
abstract type AbstractType {}
type ConcreteType extends AbstractType {}
fn foo(x: AbstractType) {
    if (x is ConcreteType) {
        var _ = x;
    }
}
"#;
        let diags = analyze_project_src(src);
        let hit: Vec<_> = diags
            .iter()
            .filter(|d| d.code == "exhaustive-is-check")
            .collect();
        assert!(
            hit.is_empty(),
            "no-else single-subtype gate must not flag: {hit:#?}"
        );
    }

    /// Same shape with an explicit (even empty) `else` branch: the
    /// dead branch is now present in source, so the warning fires.
    #[test]
    fn p42_exhaustive_is_check_no_else_with_else_fires() {
        let src = r#"
abstract type AbstractType {}
type ConcreteType extends AbstractType {}
fn foo(x: AbstractType) {
    if (x is ConcreteType) {
        var _ = x;
    } else {
        var _ = x;
    }
}
"#;
        let diags = analyze_project_src(src);
        let hit: Vec<_> = diags
            .iter()
            .filter(|d| d.code == "exhaustive-is-check")
            .collect();
        assert_eq!(
            hit.len(),
            1,
            "with else branch present, exhaustion should flag: {diags:#?}"
        );
        assert!(
            hit[0].message.contains("else-branch is unreachable"),
            "diagnostic should name the unreachable branch; got: {}",
            hit[0].message
        );
    }

    /// `!`-swap: `if (!(s is Rect || s is Circle))` is the
    /// negation of an exhausted check — the *then* branch is
    /// unreachable. Confirms the symmetric emission path.
    #[test]
    fn p42_exhaustive_is_check_negated_then_unreachable() {
        let src = r#"
abstract type Shape {}
type Rect extends Shape {}
type Circle extends Shape {}
fn caller(s: Shape) {
    if (!(s is Rect || s is Circle)) {
        var _ = s;
    }
}
"#;
        let diags = analyze_project_src(src);
        let hit: Vec<_> = diags
            .iter()
            .filter(|d| d.code == "exhaustive-is-check")
            .collect();
        assert_eq!(hit.len(), 1, "expected one diagnostic; got: {diags:#?}");
        assert!(
            hit[0].message.contains("then-branch is unreachable"),
            "negated form must flag the then-branch; got: {}",
            hit[0].message
        );
    }

    /// Partial coverage: hierarchy has three concrete derivatives,
    /// disjunction covers two. NOT exhausted — no diagnostic.
    #[test]
    fn p42_exhaustive_is_check_partial_coverage_silent() {
        let src = r#"
abstract type Shape {}
type Rect extends Shape {}
type Circle extends Shape {}
type Triangle extends Shape {}
fn caller(s: Shape) {
    if (s is Rect || s is Circle) {
        var _ = s;
    }
}
"#;
        let diags = analyze_project_src(src);
        let hit: Vec<_> = diags
            .iter()
            .filter(|d| d.code == "exhaustive-is-check")
            .collect();
        assert!(
            hit.is_empty(),
            "partial coverage must not flag exhaustion: {hit:#?}"
        );
    }

    /// Concrete root: `Shape` is not abstract, so `is Shape`-style
    /// "always-true" is the domain of the existing per-`is`
    /// decidability pass. P42.5's exhaustion pass intentionally
    /// returns `false` for concrete known types to avoid double-fire.
    #[test]
    fn p42_exhaustive_is_check_concrete_known_no_double_fire() {
        use greycat_analyzer_core::SourceManager;
        use std::str::FromStr;
        let src = r#"
type Rect {}
fn caller(r: Rect) {
    if (r is Rect) {
        var _ = r;
    }
}
"#;
        let mut mgr = SourceManager::new();
        let uri = Uri::from_str("file:///mod.gcl").unwrap();
        mgr.add_simple(uri.clone(), src, "project", false);
        let pa = crate::project::ProjectAnalysis::analyze(&mgr);
        let module = pa.module(&uri).unwrap();
        let diags = &module.analysis.diagnostics;
        let exhaustion: Vec<_> = diags
            .iter()
            .filter(|d| d.code == "exhaustive-is-check")
            .collect();
        assert!(
            exhaustion.is_empty(),
            "concrete known must not fire exhaustion (the `condition is always true` path covers it): {diags:#?}"
        );
        // Existing always-true pass must still fire — now surfaced as a
        // suppressible `decidable-condition` lint rather than a raw
        // semantic warning.
        let lints = &module.lints;
        let always_true: Vec<_> = lints
            .iter()
            .filter(|l| {
                l.rule == "decidable-condition" && l.message.contains("condition is always true")
            })
            .collect();
        assert!(
            !always_true.is_empty(),
            "existing always-true diag should fire for `r: Rect; r is Rect`: {lints:#?}"
        );
    }

    /// Nested narrows compose via the narrow stack: the inner else
    /// subtracts from the *currently narrowed* type (Mammal), not
    /// from the original declaration (Animal). After `is Mammal`
    /// then-branch, `a: Mammal`. Inside, `is Feline` else subtracts
    /// `closure(Feline) = {Cat, Lynx}` from `closure(Mammal) = {Cat,
    /// Lynx, Dog, Horse}` → `{Dog, Horse}`. That set has no abstract
    /// ancestor (`Canid` only covers `{Dog}`), so the narrow renders
    /// as the explicit union.
    #[test]
    fn p42_is_narrow_abstract_nested_subtracts_from_outer_narrow() {
        let src = r#"
abstract type Animal {}
abstract type Mammal extends Animal {}
abstract type Feline extends Mammal {}
type Cat extends Feline {}
type Lynx extends Feline {}
type Dog extends Mammal {}
type Horse extends Mammal {}
type Eagle extends Animal {}
fn caller(a: Animal) {
    if (a is Mammal) {
        if (a is Feline) {
            return;
        }
        var x = a;
    }
}
"#;
        let ty = local_init_ty(src, "x").expect("init type");
        // The two leaves of `Mammal \ Feline` are `Dog` and `Horse`,
        // in canonical (Symbol-sorted) order. Source order in the
        // type-decl listing matches; the union renders in that order.
        assert!(
            ty.contains("Dog") && ty.contains("Horse") && !ty.contains("Eagle"),
            "expected narrow to subtract from Mammal (not Animal); got: {ty}"
        );
        // And not collapsed: there's no abstract `A` with
        // `closure(A) = {Dog, Horse}`.
        assert!(
            ty.contains('|'),
            "expected an explicit Union narrow (no ancestor-collapse here); got: {ty}"
        );
    }
}
