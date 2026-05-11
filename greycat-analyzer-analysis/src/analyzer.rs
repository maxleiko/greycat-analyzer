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

use std::ops::Range;

use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;

use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{
    AssignStmt, AtStmt, BinOp, BinaryExpr, CallExpr, Decl, DoWhileStmt, Expr, FnDecl, ForInStmt,
    ForStmt, Ident, IfStmt, LambdaExpr, LiteralExpr, LiteralKind, LocalVar, MemberExpr, ObjectExpr,
    OffsetExpr, Pragma, StaticExpr, Stmt, StringExpr, TryStmt, TypeAttr, TypeDecl, TypeRef,
    UnaryExpr, UnaryOp, VarDeclTop, WhileStmt,
};
use greycat_analyzer_types::{
    GenericOwner, InferenceTable, Primitive, Type, TypeArena, TypeId, TypeKind, TypeRegistry,
    is_castable, is_node_tag,
};

use crate::resolver::{Definition, Resolutions};
use crate::stdlib::ProjectIndex;

// P13.1
/// Does this statement always exit the enclosing control
/// flow (`return`, `throw`, `break`, `continue`)? `Block` recurses
/// into its last statement. `If` requires *both* branches to
/// terminate (no else → not terminal). Used by the analyzer to lift
/// the else-branch's narrowing into the post-if scope when the
/// then-branch always exits early — handles the `if (x == null)
/// { return; } use(x);` idiom.
fn stmt_terminates(hir: &Hir, stmt_id: Idx<Stmt>) -> bool {
    match &hir.stmts[stmt_id] {
        Stmt::Return(_) | Stmt::Throw(_) | Stmt::Break | Stmt::Continue => true,
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
fn block_terminates(hir: &Hir, block: &greycat_analyzer_hir::types::BlockStmt) -> bool {
    block.stmts.last().is_some_and(|s| stmt_terminates(hir, *s))
}

// P12.4
/// Classify a numeric literal's source text as `int` or
/// `float`. Returns `Primitive::Float` for literals that contain a
/// decimal point, scientific notation (`1e3`, `1.5E-2`), or trailing
/// `f` / `_f` suffix; everything else falls back to `Primitive::Int`.
/// Other typed suffixes (`_time`, `_duration`, …) leave `LiteralKind::Number`
/// untyped today;  promotes those to dedicated `LiteralKind`
/// variants so this helper only sees float / int candidates.
fn numeric_literal_kind(text: &str) -> Primitive {
    // Bare `f` is the float suffix; the leading `_` (`_f`) is a
    // formatter convention, not a grammar requirement, so both forms
    // must classify identically. `time` / duration suffixes have
    // already been split off into `LiteralKind::Time` /
    // `LiteralKind::Duration` by HIR lowering's `classify_number`,
    // and none of them end in `f`, so this check has no false
    // positives.
    if text.ends_with('f') {
        return Primitive::Float;
    }
    if text.contains('.') {
        return Primitive::Float;
    }
    // Scientific notation: an `e` / `E` immediately preceded by an
    // ASCII digit and followed by `+` / `-` or another digit. Guards
    // against false positives on typed suffixes like `_time` (contains
    // 'e' but not at a digit-anchored position).
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if (b == b'e' || b == b'E') && i > 0 {
            let prev = bytes[i - 1];
            if prev.is_ascii_digit()
                && let Some(&next) = bytes.get(i + 1)
                && (next == b'+' || next == b'-' || next.is_ascii_digit())
            {
                return Primitive::Float;
            }
        }
    }
    Primitive::Int
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
    pub message: String,
    pub byte_range: Range<usize>,
    pub category: DiagCategory,
}

impl SemanticDiagnostic {
    /// Default-constructor for callers in the analyzer / resolver
    /// that emit non-type-relation diagnostics. Type-relation
    /// callers (only the project pipeline's validation pass) must
    /// build the struct literally so the category is explicit.
    pub fn structural(severity: Severity, message: String, byte_range: Range<usize>) -> Self {
        Self {
            severity,
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
    // P25.6
    pub enum_name: SmolStr,
    /// Variants the chain failed to cover, in declaration order.
    // P25.6
    pub missing: Vec<SmolStr>,
    /// Byte range of the head `if`, used as the diagnostic's range.
    pub byte_range: Range<usize>,
}

/// Output of the analyzer for a single module.
///
// P19
/// The [`TypeArena`] that backs every `TypeId` in this struct
/// is owned by [`crate::project::ProjectAnalysis`], not here. Pass it
/// alongside any `AnalysisResult` you want to inspect — call
/// [`crate::project::ProjectAnalysis::arena`] to get a borrow.
#[derive(Debug, Default)]
pub struct AnalysisResult {
    pub registry: TypeRegistry,
    /// Per-expression inferred type (subset — entries only for expressions
    /// the analyzer actually visited).
    pub expr_types: FxHashMap<Idx<Expr>, TypeId>,
    /// Per-binding inferred type. Keyed by the *defining* `Idx<Ident>`
    /// (e.g. the param name in `fn f(x: int)`, the local name in
    /// `var y: T = …`).
    pub def_types: FxHashMap<Idx<Ident>, TypeId>,
    /// Module-local map from declared type name to its HIR `TypeDecl`.
    /// Built when the analyzer walks top-level decls — lets
    /// member resolution navigate from a receiver's `TypeId` back to
    /// the declaring node so attr / method idents can be bound.
    // P25.3
    pub type_decls: FxHashMap<SmolStr, Idx<Decl>>,
    /// Member-access bindings produced by each property ident in
    /// `a.b` / `a->b` that resolves to a [`TypeAttr`] or to a
    /// `TypeDecl::methods` entry, keyed by the property `Idx<Ident>`.
    /// Capabilities consult this in addition to [`Resolutions`] so
    /// goto-definition / hover work on member access.
    pub member_uses: FxHashMap<Idx<Ident>, MemberDef>,
    // P11.5, P21
    /// Cross-module member bindings — same keying as `member_uses`
    /// but the resolved attr / method lives in another module's HIR.
    ///
    /// Populated directly by `Cx::resolve_member` against
    /// [`crate::stdlib::ProjectIndex::type_members`] when the
    /// receiver's type isn't declared in this module. Pass 3
    /// (`resolve_cross_module_members`) and the per-module
    /// `deferred_member_uses` deferral list are gone — S2-S6 build
    /// the structure index up front, so the body walker resolves
    /// inline.
    pub foreign_member_uses: FxHashMap<Idx<Ident>, ForeignMember>,
    // P15.x
    /// Chain-segment bindings populated by `ProjectAnalysis`
    /// pass 3.5 for `Expr::QualifiedStatic` shapes. Each segment ident
    /// (chain[1] = the type, chain[2] = the member when length is 3)
    /// binds to the foreign top-level decl that declares it. Lets
    /// hover / goto-def show the right content for each segment of
    /// `runtime::Identity::create`.
    pub foreign_decl_uses: FxHashMap<Idx<Ident>, ForeignDecl>,
    pub diagnostics: Vec<SemanticDiagnostic>,
    // P24.2
    /// Head if-stmt ids of enum-eq chains that
    /// exhaustively cover every variant of the dispatched-on enum.
    /// Consumed by the `unreachable` lint to flag the trailing
    /// `else` arm of such a chain as dead code, and to treat the
    /// chain as effectively divergent for fall-through-deadness
    /// analysis when every arm body diverges.
    pub exhaustive_enum_chains: FxHashSet<Idx<Stmt>>,
    /// Non-exhaustive enum-eq chains detected in pass 2. The
    /// `non-exhaustive` lint reads this and emits a real
    /// [`crate::lint::LintDiagnostic`] (rule-keyed, suppressible via
    /// `// gcl-lint-off…`). Unlike historical structural diagnostics
    /// (which lacked a rule code), this flow integrates with the
    /// shared directive / quickfix machinery.
    pub non_exhaustive_findings: Vec<NonExhaustiveFinding>,
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

// P11.5
/// A member-access binding that resolves into another module.
/// `uri` names the home module of the foreign type's declaration; the
/// `member` indices reference that module's HIR arenas, not the
/// analyzed module's.
#[derive(Debug, Clone)]
pub struct ForeignMember {
    pub uri: greycat_analyzer_core::lsp_types::Uri,
    pub member: MemberDef,
}

// P15.x
/// A top-level decl reference resolved into another module.
/// Used for chain-segment bindings (`runtime::Identity::create`
/// chain[1] points at runtime.gcl's `type Identity` decl).
#[derive(Debug, Clone)]
pub struct ForeignDecl {
    pub uri: greycat_analyzer_core::lsp_types::Uri,
    pub decl: Idx<Decl>,
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

    // P11.5
    /// Look up a cross-module member-access binding for `ident`.
    /// Falls back to `None` for members that are intra-module
    /// ([`Self::member_lookup`]) or unresolved.
    pub fn foreign_member_lookup(&self, ident: Idx<Ident>) -> Option<&ForeignMember> {
        self.foreign_member_uses.get(&ident)
    }

    // P15.x
    /// Look up a chain-segment binding (e.g. `Identity` in
    /// `runtime::Identity::create` -> the foreign type decl).
    pub fn foreign_decl_lookup(&self, ident: Idx<Ident>) -> Option<&ForeignDecl> {
        self.foreign_decl_uses.get(&ident)
    }
}

/// Run the analyzer with no cross-module project context. Falls back
/// to an empty [`ProjectIndex`]; cross-module type names lower to
/// `any` and cross-module member access can't bind. Used by per-file
/// capabilities and unit tests.
///
// P19
/// Allocates a fresh [`TypeArena`] internally and discards it
/// — callers that need to inspect `TypeId`s after the call must use
/// [`analyze_with_index_into`] instead so the arena outlives the call.
pub fn analyze(hir: &Hir, res: &Resolutions) -> (TypeArena, AnalysisResult) {
    let index = ProjectIndex::new();
    let mut arena = TypeArena::new();
    let out = analyze_with_index_into(hir, res, &index, &mut arena);
    (arena, out)
}

/// Convenience wrapper that allocates a private arena. Same caveat as
/// [`analyze`]: the arena is returned to the caller alongside the
/// result so any [`TypeId`] in the result can still be looked up.
pub fn analyze_with_index(
    hir: &Hir,
    res: &Resolutions,
    index: &ProjectIndex,
) -> (TypeArena, AnalysisResult) {
    let mut arena = TypeArena::new();
    let out = analyze_with_index_into(hir, res, index, &mut arena);
    (arena, out)
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
    arena: &mut TypeArena,
) -> AnalysisResult {
    let mut out = AnalysisResult::default();
    seed_builtins(arena);
    register_module_types(hir, arena, &mut out);

    let Some(module) = hir.module.as_ref() else {
        return out;
    };
    let mut cx = Cx {
        hir,
        res,
        out: &mut out,
        arena,
        index,
        narrows: Vec::new(),
        member_narrows: Vec::new(),
        member_typed_narrows: Vec::new(),
        chain_member_ifs: FxHashSet::default(),
        generics_in_scope: Vec::new(),
        this_stack: Vec::new(),
    };
    for d in &module.decls {
        cx.visit_decl(*d);
    }

    // Surface resolver's unresolved-name list as analyzer diagnostics so
    // P2.7 (LSP publish) only needs one list per file.
    let unresolved = res.unresolved.clone();
    for ident_idx in unresolved {
        let ident = &hir.idents[ident_idx];
        out.diagnostics.push(SemanticDiagnostic::structural(
            Severity::Error,
            format!("unresolved name `{}`", ident.text),
            ident.byte_range.clone(),
        ));
    }

    out
}

/// Seed primitive type ids in the arena so cx.{int, bool, ...} are cheap.
/// Idempotent — `alloc` interns equal types so re-seeding is a no-op
/// (the project pipeline calls this once per `analyze_with_index_into`,
/// which all share the same arena).
pub(crate) fn seed_builtins(arena: &mut TypeArena) {
    let _ = arena.primitive(Primitive::Bool);
    let _ = arena.primitive(Primitive::Int);
    let _ = arena.primitive(Primitive::Float);
    let _ = arena.primitive(Primitive::Char);
    let _ = arena.primitive(Primitive::String);
    let _ = arena.primitive(Primitive::Time);
    let _ = arena.primitive(Primitive::Duration);
    let _ = arena.primitive(Primitive::Geo);
    let _ = arena.null();
    let _ = arena.any();
    let _ = arena.never();
}

/// Build a TypeRegistry from the module's user declarations. Each
/// `type Foo {}` becomes a Named("Foo") TypeId; later phases can
/// elaborate the type's attribute list separately.
///
/// Also populates [`AnalysisResult::type_decls`] (name → HIR
/// `TypeDecl` index) so  member resolution can navigate from a
/// receiver's `TypeId` back to the declaring node.
fn register_module_types(hir: &Hir, arena: &mut TypeArena, out: &mut AnalysisResult) {
    let Some(module) = hir.module.as_ref() else {
        return;
    };
    for d in &module.decls {
        let decl = &hir.decls[*d];
        match decl {
            Decl::Type(td) => {
                let name: SmolStr = hir.idents[td.name].text.as_str().into();
                let id = arena.named(name.as_str());
                out.registry.register(name.clone(), id);
                out.type_decls.insert(name, *d);
            }
            Decl::Enum(ed) => {
                let name: SmolStr = hir.idents[ed.name].text.as_str().into();
                let variants: Vec<SmolStr> = ed
                    .fields
                    .iter()
                    .map(|f| hir.idents[hir.enum_fields[*f].name].text.as_str().into())
                    .collect();
                let id = arena.alloc(Type {
                    kind: TypeKind::Enum {
                        name: name.clone(),
                        variants,
                    },
                    nullable: false,
                });
                out.registry.register(name.clone(), id);
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
    // P19.16
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
}

/// One arm in an enum-equality chain.
struct EnumChainArm {
    if_stmt_id: Idx<Stmt>,
    variant: String,
}

/// An `if (x == E::A) else if (x == E::B) ...` chain.
struct EnumChain {
    enum_name: String,
    arms: Vec<EnumChainArm>,
    /// `true` when the chain ends with a final `else { ... }` or with
    /// a non-conforming `else if` — both act as catch-alls.
    has_final_else: bool,
}

// P23
/// Small dispatch enum used by [`Cx::try_member_call_typing`]
/// so we can read the callee shape from `&self.hir.exprs[idx]` and
/// then drop that borrow before the recursive `&mut self` call. Plain
/// `Copy` fields plus an owned `Vec<Idx<Ident>>` for the qualified-
/// chain case (the only shape that actually allocates).
enum CalleeShape {
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
    QualifiedStatic(Vec<Idx<Ident>>),
}

struct Cx<'a> {
    hir: &'a Hir,
    res: &'a Resolutions,
    out: &'a mut AnalysisResult,
    // P19
    /// Project-wide type arena. Owned by `ProjectAnalysis`, so
    /// every module's analyzer mints into the same `TypeArena` and
    /// `TypeId`s are comparable across module boundaries.
    arena: &'a mut TypeArena,
    // P11.5
    /// Cross-module project index. Per-file callers pass an
    /// empty [`ProjectIndex::new`]; the project pipeline passes the
    /// index it just rebuilt. Used by `lower_type_ref` to recognize
    /// type names that aren't declared in this module.
    index: &'a ProjectIndex,
    /// Null-flow narrowing stack. Each frame is a binding ident
    /// → temporary `TypeId` override. Frames are pushed on block /
    /// then-branch / else-branch entry and popped on exit, so a
    /// narrowing introduced inside a block stays alive for the rest
    /// of that block but doesn't leak to siblings.
    narrows: Vec<FxHashMap<Idx<Ident>, TypeId>>,
    // P19.16
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
    /// `Stmt::If` ids already accounted for as nested members of an
    /// enclosing exhaustiveness chain. Suppresses duplicate
    /// "non-exhaustive" diagnostics on inner `else if` arms.
    chain_member_ifs: FxHashSet<Idx<Stmt>>,
    // P12.1
    /// Generic-context stack: type-parameter names visible at the
    /// current scope, mapped to their declaring [`GenericOwner`].
    /// Entered on `fn f<T>(...)` / `type Foo<T> {}` and used by
    /// `lower_type_ref` to mint `GenericParam(name, owner)` instead
    /// of `Named(name)` / `Any` for in-scope generics. The stack is a
    /// `Vec<HashMap>` so nested fns inside a generic type see both
    /// outer and inner names.
    // P25.5
    generics_in_scope: Vec<FxHashMap<SmolStr, GenericOwner>>,
    // P19.11
    /// `this` typing stack. Pushed on entry to a
    /// type's method body (in `visit_type_decl`), popped on exit.
    /// `LiteralKind::This` returns the top of the stack so a
    /// reference to `this` inside `type Foo<T> { fn m() { this } }`
    /// types as `Generic { name: "Foo", args: [GenericParam(T)] }`
    /// — matches what an external `node<Foo<int>>` deref would see.
    /// Empty outside method bodies (top-level fns / lambdas).
    this_stack: Vec<TypeId>,
}

impl<'a> Cx<'a> {
    fn primitive(&mut self, p: Primitive) -> TypeId {
        self.arena.primitive(p)
    }
    fn any(&mut self) -> TypeId {
        self.arena.any()
    }
    fn null(&mut self) -> TypeId {
        self.arena.null()
    }
    fn record(&mut self, expr: Idx<Expr>, ty: TypeId) {
        self.out.expr_types.insert(expr, ty);
    }
    fn diag(&mut self, severity: Severity, message: impl Into<String>, range: Range<usize>) {
        // The analyzer's first pass only emits structural diagnostics
        // (unresolved names, member-resolution failures, exhaustiveness,
        // …). Type-relation diagnostics live in the project pipeline's
        // `validate_type_relations` post-pass — see `DiagCategory`.
        self.out.diagnostics.push(SemanticDiagnostic::structural(
            severity,
            message.into(),
            range,
        ));
    }
    fn ident_text(&self, idx: Idx<Ident>) -> &str {
        &self.hir.idents[idx].text
    }

    fn push_narrow(&mut self) {
        self.narrows.push(FxHashMap::default());
        self.member_narrows.push(FxHashSet::default());
        self.member_typed_narrows.push(FxHashMap::default());
    }
    fn pop_narrow(&mut self) {
        self.narrows.pop();
        self.member_narrows.pop();
        self.member_typed_narrows.pop();
    }
    fn write_narrow(&mut self, name: Idx<Ident>, ty: TypeId) {
        if let Some(top) = self.narrows.last_mut() {
            top.insert(name, ty);
        }
    }
    // P19.16
    /// Record `path` as guaranteed non-null in the current
    /// scope. Subsequent `Expr::Member` evaluations at the same path
    /// strip the result's nullable bit.
    fn write_member_non_null(&mut self, path: String) {
        if let Some(top) = self.member_narrows.last_mut() {
            top.insert(path);
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
    fn write_member_typed(&mut self, path: String, ty: TypeId) {
        if let Some(top) = self.member_typed_narrows.last_mut() {
            top.insert(path, ty);
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
    // P19.16
    /// Build a string path key for an expression that's a
    /// chain of `Expr::Member` rooted at an `Expr::Ident` (the binding
    /// name) or `Expr::Literal(This)` (yielding `"this"` as the root).
    /// Returns `None` for any other shape (offsets, calls, parens, etc.)
    /// so we don't accidentally narrow paths whose receiver is a fresh
    /// computed value rather than a stable reference.
    fn member_path(&self, expr_id: Idx<Expr>) -> Option<String> {
        match &self.hir.exprs[expr_id] {
            Expr::Ident { name: name_idx, .. } => Some(self.ident_text(*name_idx).to_string()),
            Expr::Literal(LiteralExpr {
                kind: LiteralKind::This,
                ..
            }) => Some("this".to_string()),
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
            _ => None,
        }
    }
    fn strip_nullable(&mut self, ty: TypeId) -> TypeId {
        let t = self.arena.get(ty);
        if !t.nullable {
            return ty;
        }
        let mut t = t.clone();
        t.nullable = false;
        self.arena.alloc(t)
    }

    // P19.16
    /// When an assignment's LHS is an `Ident` resolving
    /// to a Param/Local, narrow that binding to the RHS's type for
    /// the rest of the enclosing block. The `Stmt::If` post-pass
    /// then lifts narrows that hold along every path through the if.
    /// When the LHS is a member-access path (e.g.
    /// `this.matchingNormalisation = ...`), record / clear the
    /// member-narrow for that path based on the RHS's nullability.
    /// Other LHS shapes (offsets, calls, etc.) don't have a stable
    /// identity and silently no-op.
    fn record_assign_narrow(&mut self, target: Idx<Expr>, value_ty: TypeId) {
        if let Expr::Ident { name: name_idx, .. } = &self.hir.exprs[target] {
            if let Some(Definition::Param(def) | Definition::Local(def)) =
                self.res.lookup(*name_idx)
            {
                self.write_narrow(def, value_ty);
            }
            return;
        }
        if matches!(self.hir.exprs[target], Expr::Member(_) | Expr::Arrow(_))
            && let Some(path) = self.member_path(target)
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
                self.write_member_non_null(path);
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
                let stripped = self.strip_nullable(cur);
                self.write_narrow(def, stripped);
            }
            return;
        }
        if matches!(self.hir.exprs[target], Expr::Member(_) | Expr::Arrow(_))
            && let Some(path) = self.member_path(target)
        {
            self.write_member_non_null(path);
        }
    }

    // P16.5
    /// When an `Expr::Arrow` receiver is a single-arg node-tag
    /// generic (`node<T>`, `nodeTime<T>`, `nodeList<T>`, `nodeGeo<T>`),
    /// `n->field` resolves against the inner type's members rather
    /// than the tag's own. Mirrors the runtime's `*n.field` semantics:
    /// `->` is sugar for "deref then access", so members are searched
    /// on the deref'd type, not on the tag. Tag-owned methods stay
    /// reachable via the dot syntax (`n.resolve()`, `n.size()`) — the
    /// `Expr::Member` path in the caller is unchanged.
    /// Multi-arg shapes (`nodeIndex<K, V>`) don't match — there's no
    /// canonical `inner` to redirect to. Returns `None` for non-tag
    /// receivers so the caller resolves against the receiver itself.
    fn arrow_deref_receiver(&self, recv_ty: TypeId) -> Option<TypeId> {
        let ty = self.arena.get(recv_ty);
        match &ty.kind {
            TypeKind::Generic { name, args } if is_node_tag(name) && args.len() == 1 => {
                Some(args[0])
            }
            _ => None,
        }
    }

    // P6.3
    /// Member resolution: bind the property ident in `a.b` /
    /// `a->b` to the matching `TypeAttr` or method `Decl` whenever the
    /// receiver's type names a `TypeDecl` declared in this module.
    /// Anonymous types and primitives stay no-binding; cross-module
    /// receivers consult [`crate::stdlib::ProjectIndex::type_members`]
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
        let type_name: Option<SmolStr> = match &ty.kind {
            TypeKind::Named { name } => Some(name.clone()),
            TypeKind::Generic { name, .. } => Some(name.clone()),
            // P16.2 — primitives (`String`, `int`, ...) carry methods
            // declared as `native type String { ... }` in stdlib.
            // Map the primitive back to its name and fall through to
            // the same `type_decls` / `decl_locations` lookup path so
            // `"hello".size()` and friends bind correctly.
            TypeKind::Primitive(p) => Some(p.name().into()),
            TypeKind::Anonymous { fields } => {
                // Anonymous types don't have a backing TypeDecl, so we
                // resolve their fields directly from the type shape.
                let prop = &*self.hir.idents[property].text;
                if fields.iter().any(|(n, _)| *n == prop) {
                    // No TypeAttr / Decl to point to — capabilities
                    // gracefully no-op without a member_uses entry.
                }
                None
            }
            _ => None,
        };
        let Some(name) = type_name else {
            return;
        };
        let prop_text = self.ident_text(property);

        // Resolution order: attrs first (local then inherited), methods
        // second (local then inherited). Attrs always win over methods
        // of the same name — the runtime resolves `this.from` to an
        // inherited `from: time` attr even when a `static fn from(...)`
        // is declared on the receiver type.
        let local_type_decl = self
            .out
            .type_decls
            .get(name.as_str())
            .copied()
            .and_then(|id| match &self.hir.decls[id] {
                Decl::Type(td) => Some(td.clone()),
                _ => None,
            });

        if let Some(type_decl) = local_type_decl.as_ref() {
            for attr_id in &type_decl.attrs {
                let attr = &self.hir.type_attrs[*attr_id];
                if self.hir.idents[attr.name].text == prop_text {
                    self.out
                        .member_uses
                        .insert(property, MemberDef::Attr(*attr_id));
                    return;
                }
            }
        }
        if let Some((uri, attr_id)) = self.index.type_attr_id_chain(&name, prop_text) {
            self.out.foreign_member_uses.insert(
                property,
                ForeignMember {
                    uri,
                    member: MemberDef::Attr(attr_id),
                },
            );
            return;
        }
        if let Some(type_decl) = local_type_decl.as_ref() {
            for method_id in &type_decl.methods {
                let Decl::Fn(m) = &self.hir.decls[*method_id] else {
                    continue;
                };
                if instance_access && m.modifiers.static_ {
                    continue;
                }
                if self.hir.idents[m.name].text == prop_text {
                    self.out
                        .member_uses
                        .insert(property, MemberDef::Method(*method_id));
                    return;
                }
            }
        }
        let method_lookup = if instance_access {
            self.index.type_instance_method_id_chain(&name, prop_text)
        } else {
            self.index.type_method_id_chain(&name, prop_text)
        };
        if let Some((uri, method_id)) = method_lookup {
            self.out.foreign_member_uses.insert(
                property,
                ForeignMember {
                    uri,
                    member: MemberDef::Method(method_id),
                },
            );
        }
    }

    fn resolve_member(&mut self, recv_ty: TypeId, property: Idx<Ident>) {
        self.resolve_member_with(recv_ty, property, true);
    }

    // P23
    /// Inline call-return typing for Member / Arrow /
    /// Static callees. Looks up the method's pre-lowered return
    /// `TypeId` in `index.type_members[type_name].method_returns` and
    /// applies `arena.substitute` against the receiver's
    /// instantiation. Returns `None` for callees this path doesn't
    /// handle (Ident / QualifiedStatic / Lambda / etc.) so the
    /// caller falls back to `any` until those branches land.
    fn try_member_call_typing(&mut self, callee: Idx<Expr>) -> Option<TypeId> {
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
            Expr::Ident { name: name_idx, .. } => CalleeShape::Ident(*name_idx),
            Expr::QualifiedStatic { chain, .. } => CalleeShape::QualifiedStatic(chain.clone()),
            _ => return None,
        };
        match dispatch {
            CalleeShape::Member {
                receiver,
                property,
                is_arrow,
            } => {
                let recv_ty = self.out.expr_types.get(&receiver).copied()?;
                let ret = self.method_return_for(recv_ty, property, is_arrow)?;
                // P19.17 — propagate receiver nullability. `x?.foo()`
                // and `x.foo()` where `x: T?` both yield `Ret?` at
                // runtime: the call shorts (or NPEs) when the receiver
                // is null. The Member arm of `infer_expr` does the
                // same lift for value access; here we mirror it for
                // calls so chains like `x?.foo().bar` carry the
                // nullability through.
                if self.arena.get(recv_ty).nullable {
                    Some(self.arena.nullable(ret))
                } else {
                    Some(ret)
                }
            }
            CalleeShape::Static { ty, property } => {
                let recv_ty = self.lower_type_ref(ty);
                self.method_return_for(recv_ty, property, false)
            }
            CalleeShape::Ident(name_idx) => self.bare_fn_return(name_idx),
            CalleeShape::QualifiedStatic(chain) => self.qualified_static_call_return(&chain),
        }
    }

    // P23
    /// Type a bare-Ident call (`foo()` / `module_fn()`) by
    /// looking up the fn's signature. Local fns lower the return
    /// `TypeRef` inline; cross-module fns consult the project
    /// signatures index. Generic fns aren't handled here — they
    /// route through [`Self::try_generic_call_inference`] which the
    /// caller tries first.
    fn bare_fn_return(&mut self, name_idx: Idx<Ident>) -> Option<TypeId> {
        let def = self.res.lookup(name_idx)?;
        let fn_name = self.ident_text(name_idx);
        // Project signatures index covers local + cross-module + native
        // fns (P23 includes natives in `stage_lower_signatures`).
        if let Some(sig) = self.index.fn_signature_for(fn_name) {
            return Some(sig.return_ty);
        }
        match def {
            Definition::Decl(decl_id) => {
                let Decl::Fn(fnd) = &self.hir.decls[decl_id] else {
                    return None;
                };
                if !fnd.generics.is_empty() {
                    return None;
                }
                let ret = fnd.return_type?;
                Some(self.lower_type_ref(ret))
            }
            _ => None,
        }
    }

    // P23
    /// Type a `QualifiedStatic` callee. Two shapes:
    /// - `module::fn(...)` — chain has 2 segments. Look up
    ///   `chain[1]` in `index.fn_signatures`.
    /// - `module::Type::method(...)` — chain has 3 segments. Look
    ///   up `chain[1]` as a type, then `chain[2]` as one of its
    ///   methods.
    fn qualified_static_call_return(&mut self, chain: &[Idx<Ident>]) -> Option<TypeId> {
        match chain.len() {
            2 => {
                let fn_name = self.ident_text(chain[1]);
                let sig = self.index.fn_signature_for(fn_name)?;
                Some(sig.return_ty)
            }
            3 => {
                let type_name = self.ident_text(chain[1]);
                let method_name = self.ident_text(chain[2]);
                // P19.14 — chain-walking lookup.
                self.index.type_method_return_chain(type_name, method_name)
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
        let (type_name, instantiation): (SmolStr, Vec<TypeId>) = match recv.kind {
            TypeKind::Named { name } => (name, Vec::new()),
            // P25.7
            TypeKind::Generic { name, args } => (name, args.into_vec()),
            TypeKind::Primitive(p) => (p.name().into(), Vec::new()),
            _ => return None,
        };
        // **P19.14** — chain-walking lookup so methods declared on
        // a parent type resolve through a `Sub` receiver.
        let property_text = self.ident_text(property);
        let ret_ty = self
            .index
            .type_method_return_chain(&type_name, property_text)?;
        let members = self.index.type_members_for(&type_name)?;
        let mut subst: FxHashMap<String, TypeId> = FxHashMap::default();
        for (i, gp_sym) in members.generics.iter().enumerate() {
            if let Some(arg) = instantiation.get(i)
                && let Some(gp_name) = self.index.symbols.resolve(*gp_sym)
            {
                subst.insert(gp_name.to_string(), *arg);
            }
        }
        Some(self.arena.substitute(ret_ty, &subst))
    }

    // P23
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
        // Snapshot decl-location: the &Uri from `locate_decl` borrows
        // `self.index`, so we'd otherwise collide with the `&mut
        // self.out` insert below. Cloning the Uri is the necessary
        // owned copy here, not the laziness kind.
        let (host_uri, host_decl_id) =
            match self.index.locate_decl(self.ident_text(chain[1])).first() {
                Some((uri, decl_id)) => (uri.clone(), *decl_id),
                None => return,
            };
        self.out.foreign_decl_uses.insert(
            chain[1],
            ForeignDecl {
                uri: host_uri,
                decl: host_decl_id,
            },
        );
        if chain.len() == 3 {
            // Resolve the (uri, member) pair without holding an
            // `&self.index` borrow across the `&mut self.out` insert.
            let resolved = self
                .index
                .type_members_for(self.ident_text(chain[1]))
                .and_then(|members| {
                    let prop = self.hir.idents[chain[2]].text.as_str();
                    if let Some(attr_id) = members.attr_id(&self.index.symbols, prop) {
                        Some((members.home_uri.clone(), MemberDef::Attr(attr_id)))
                    } else {
                        members
                            .method_id(&self.index.symbols, prop)
                            .map(|decl_id| (members.home_uri.clone(), MemberDef::Method(decl_id)))
                    }
                });
            if let Some((uri, member)) = resolved {
                self.out
                    .foreign_member_uses
                    .insert(chain[2], ForeignMember { uri, member });
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
        let prop_text = self.hir.idents[property].text.as_str();
        if let Some(MemberDef::Attr(attr_id)) = self.out.member_uses.get(&property).copied() {
            let attr = &self.hir.type_attrs[attr_id];
            if attr.modifiers.static_ {
                if let Some(tr) = attr.ty {
                    return Some(self.lower_type_ref(tr));
                }
                return Some(self.any());
            }
            return Some(self.arena.named("field"));
        }
        if let Some(foreign) = self.out.foreign_member_uses.get(&property)
            && matches!(foreign.member, MemberDef::Attr(_))
        {
            // Cross-module attr — consult `static_attrs` for the
            // receiver's type. The receiver is the `Type::` part
            // of the static expr; we have its lowered TypeId.
            let owner_name: Option<SmolStr> = match &self.arena.get(recv_ty).kind {
                TypeKind::Named { name } => Some(name.clone()),
                TypeKind::Generic { name, .. } => Some(name.clone()),
                TypeKind::Enum { name, .. } => Some(name.clone()),
                // **P19.14** — primitives carry static methods /
                // attrs in stdlib (`time::max`, `int::max`, etc.);
                // map them to the stdlib type name so the
                // static-attr lookup hits.
                TypeKind::Primitive(p) => Some(p.name().into()),
                _ => None,
            };
            if let Some(name) = owner_name
                && let Some(members) = self.index.type_members_for(&name)
            {
                let prop_sym = self.index.symbol(prop_text);
                let is_static = prop_sym.is_some_and(|s| members.static_attrs.contains(&s));
                if is_static {
                    if let Some(ty) = members.attr_ty(&self.index.symbols, prop_text) {
                        return Some(ty);
                    }
                    return Some(self.any());
                }
            }
            return Some(self.arena.named("field"));
        }
        // Method reference (in-module or cross-module).
        let kind = self.out.member_uses.get(&property).copied().or_else(|| {
            self.out
                .foreign_member_uses
                .get(&property)
                .map(|f| f.member)
        })?;
        Some(match kind {
            MemberDef::Attr(_) => self.arena.named("field"),
            MemberDef::Method(_) => self.arena.named("function"),
        })
    }

    // P23
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
                let name = self.ident_text(chain[1]);
                if self.index.contains_fn_signature(name) {
                    Some(self.arena.named("function"))
                } else if self.index.contains_value(name) {
                    // Non-native fn (`private fn foo()` or fn without
                    // declared return type) — present in `values` but
                    // skipped from `fn_signatures`. The runtime still
                    // treats `module::fn_name` as a function ref, so
                    // type it as `function` (not `type`).
                    Some(self.arena.named("function"))
                } else if self.index.contains_type_member(name) || self.index.has_name(name) {
                    Some(self.arena.named("type"))
                } else {
                    None
                }
            }
            3 => {
                let type_name = self.ident_text(chain[1]).to_string();
                let member_name = self.ident_text(chain[2]).to_string();
                // Enum variant: `module::Foo::a` types as `Foo` (the
                // enum), matching the analyzer's `Static` enum-variant
                // arm so call-arg validation against `_: Foo` passes.
                if let Some(ty_id) = self.index.enum_type_for(&type_name)
                    && let TypeKind::Enum { variants, .. } = &self.arena.get(ty_id).kind
                    && variants.iter().any(|v| v == &member_name)
                {
                    return Some(ty_id);
                }
                let members = self.index.type_members_for(&type_name)?;
                if members
                    .method_id(&self.index.symbols, &member_name)
                    .is_some()
                {
                    Some(self.arena.named("function"))
                } else if members.attr_id(&self.index.symbols, &member_name).is_some() {
                    // P19.13 — static-attr value access from a
                    // `module::Type::name` chain. Returns the
                    // attr's declared type for static attrs;
                    // `field` handle otherwise.
                    let prop_sym = self.index.symbol(&member_name);
                    let is_static = prop_sym.is_some_and(|s| members.static_attrs.contains(&s));
                    if is_static {
                        Some(
                            members
                                .attr_ty(&self.index.symbols, &member_name)
                                .unwrap_or_else(|| self.any()),
                        )
                    } else {
                        Some(self.arena.named("field"))
                    }
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    // P22
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
            return Some(self.arena.named("function"));
        }
        // Attr — extract receiver shape (need owned name + args because
        // the arena entry borrow has to drop before we re-borrow as
        // mutable for `substitute`).
        let (type_name, instantiation): (SmolStr, Vec<TypeId>) = match &self.arena.get(recv_ty).kind
        {
            TypeKind::Named { name } => (name.clone(), Vec::new()),
            // P25.7
            TypeKind::Generic { name, args } => (name.clone(), args.to_vec()),
            TypeKind::Primitive(p) => (p.name().into(), Vec::new()),
            _ => return None,
        };
        // Build the substitution map *before* mutably borrowing the
        // arena (substitute) — `members` borrows `self.index`.
        // **P19.14** — chain-walking lookup so attrs declared on a
        // parent type (`type Sub extends Super { ... }`) resolve
        // when accessed through a `Sub` receiver.
        let property_text = self.hir.idents[property].text.as_str();
        let (attr_ty, subst) = {
            let attr_ty = self.index.type_attr_ty_chain(&type_name, property_text)?;
            // Generic substitution is driven by the *receiver type*'s
            // own generic params, not the parent's — `node<Foo<int>>`
            // accessing a `Foo`-declared attr substitutes `T → int`.
            let members = self.index.type_members_for(&type_name)?;
            let mut subst: FxHashMap<String, TypeId> = FxHashMap::default();
            for (i, gp_sym) in members.generics.iter().enumerate() {
                if let Some(arg) = instantiation.get(i)
                    && let Some(gp_name) = self.index.symbols.resolve(*gp_sym)
                {
                    subst.insert(gp_name.to_string(), *arg);
                }
            }
            (attr_ty, subst)
        };
        Some(self.arena.substitute(attr_ty, &subst))
    }

    // Lower a syntactic TypeRef to a TypeId.
    fn lower_type_ref(&mut self, idx: Idx<TypeRef>) -> TypeId {
        let tr = self.hir.type_refs[idx].clone();
        let name = self.ident_text(tr.name).to_string();
        let mut base = match name.as_str() {
            "bool" => self.primitive(Primitive::Bool),
            "int" => self.primitive(Primitive::Int),
            "float" => self.primitive(Primitive::Float),
            "char" => self.primitive(Primitive::Char),
            "String" => self.primitive(Primitive::String),
            "time" => self.primitive(Primitive::Time),
            "duration" => self.primitive(Primitive::Duration),
            "geo" => self.primitive(Primitive::Geo),
            "any" => self.any(),
            "null" => self.null(),
            _ => {
                if !tr.params.is_empty() {
                    let args: Vec<TypeId> =
                        tr.params.iter().map(|p| self.lower_type_ref(*p)).collect();
                    self.arena.generic(name.clone(), args)
                } else if let Some(owner) = self.lookup_generic(&name) {
                    // P12.1: name matches a fn / type generic param in
                    // scope — produce a `GenericParam` rather than a
                    // bare `Named`, so call-site inference can record
                    // witnesses for it.
                    self.arena.generic_param(name.clone(), owner)
                } else if let Some(id) = self.out.registry.lookup(&name) {
                    id
                } else if let Some(enum_id) = self.index.enum_type_for(&name) {
                    // P19.10 — canonical enum TypeId from the
                    // project signature index. Same reason as
                    // `lower_type_ref_project`: a cross-module enum
                    // ref like `TimeZone` must lower to the
                    // `TypeKind::Enum { ... }` that
                    // `lower_module_signatures` minted in the shared
                    // arena, so `Expr::Static`'s enum-variant arm
                    // (and `arena.is_assignable_to` exact-match)
                    // recognise it as the same type as the foreign
                    // declaration site.
                    enum_id
                } else if self.index.has_name(&name) {
                    // P11.5: name is known to the project but not to
                    // this module's registry — i.e. a type declared
                    // elsewhere. Lower to `Named(name)` so receivers
                    // typed against it carry a name `resolve_member`
                    // can defer for the cross-module post-pass.
                    self.arena.named(name.clone())
                } else {
                    // Unknown type — fall back to Any so downstream rules don't
                    // mass-cascade. Resolver already emitted "unresolved name".
                    self.any()
                }
            }
        };
        if tr.optional {
            base = self.arena.nullable(base);
        }
        base
    }

    // P12.1
    /// Call-site generic inference. Returns `Some(return_ty)`
    /// when `callee` resolves to a non-native fn decl with `generics`
    /// declared; the witnesses come from each `(declared_param,
    /// arg_ty)` pair via [`Self::collect_witnesses`]. Returns `None`
    /// for non-fn callees (lambdas, member calls, cross-module decls
    /// not yet wired into the analyzer's HIR cache, etc.) so the
    /// caller falls back to `any`.
    fn try_generic_call_inference(
        &mut self,
        callee: Idx<Expr>,
        arg_tys: &[TypeId],
        call_range: Range<usize>,
    ) -> Option<TypeId> {
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
                let (fn_name_idx, fn_generics, fn_params, fn_return_type) =
                    match &self.hir.decls[decl_id] {
                        Decl::Fn(fnd) if !fnd.generics.is_empty() => (
                            fnd.name,
                            fnd.generics.clone(),
                            fnd.params.clone(),
                            fnd.return_type,
                        ),
                        _ => return None,
                    };
                // Lower the declared signature with the fn's generics in scope.
                let owner =
                    GenericOwner::Function(self.hir.idents[fn_name_idx].text.as_str().into());
                self.push_generic_scope(&fn_generics, owner);
                let declared_params: Vec<TypeId> = fn_params
                    .iter()
                    .map(|p_id| {
                        self.hir.fn_params[*p_id]
                            .ty
                            .map(|t| self.lower_type_ref(t))
                            .unwrap_or_else(|| self.any())
                    })
                    .collect();
                let declared_return = fn_return_type
                    .map(|t| self.lower_type_ref(t))
                    .unwrap_or_else(|| self.any());
                self.pop_generic_scope();

                let mut tbl = InferenceTable::new();
                let pair_count = declared_params.len().min(arg_tys.len());
                for i in 0..pair_count {
                    self.collect_witnesses(declared_params[i], arg_tys[i], &mut tbl, &call_range);
                }
                Some(tbl.substitute(self.arena, declared_return))
            }
            Definition::ProjectDecl { .. } => {
                // **P19.15** — cross-module generic call inference.
                // The S7-S11 stage pre-lowered every fn's params and
                // return type into the shared arena (`FnSignature`);
                // we can run the same witness-driven inference
                // without crossing the module boundary at body-walk
                // time. Without this, generic stdlib fns like
                // `abs<T>(x: T): T` typed every call as `T`
                // (GenericParam) and downstream arithmetic on the
                // result fell through to `any`.
                let fn_name = self.ident_text(name_idx);
                let sig = self.index.fn_signature_for(fn_name)?;
                if sig.generics.is_empty() {
                    return None;
                }
                let declared_params = sig.params.clone();
                let declared_return = sig.return_ty;
                let mut tbl = InferenceTable::new();
                let pair_count = declared_params.len().min(arg_tys.len());
                for i in 0..pair_count {
                    self.collect_witnesses(declared_params[i], arg_tys[i], &mut tbl, &call_range);
                }
                Some(tbl.substitute(self.arena, declared_return))
            }
            _ => None,
        }
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
        if let TypeKind::GenericParam { name, .. } = &pk.kind {
            // If the param is `T?`, the witness is whatever the arg
            // strips down to without nullable.
            let witness = if pk.nullable {
                self.strip_nullable(arg_ty)
            } else {
                arg_ty
            };
            if let Some(prior) = tbl.lookup(name) {
                if prior != witness {
                    let msg = format!(
                        "cannot infer `{}`: `{}` conflicts with `{}`",
                        name,
                        greycat_analyzer_types::display(self.arena, prior),
                        greycat_analyzer_types::display(self.arena, witness),
                    );
                    self.diag(Severity::Error, msg, call_range.clone());
                }
                return;
            }
            tbl.bind(name.clone(), witness);
            return;
        }
        let ak = self.arena.get(arg_ty).clone();
        if let (
            TypeKind::Generic { name: pn, args: pa },
            TypeKind::Generic { name: an, args: aa },
        ) = (&pk.kind, &ak.kind)
        {
            if pn == an && pa.len() == aa.len() {
                let pa = pa.clone();
                let aa = aa.clone();
                for (p, a) in pa.iter().zip(aa.iter()) {
                    self.collect_witnesses(*p, *a, tbl, call_range);
                }
            }
            return;
        }
        if let (TypeKind::Tuple { elements: pe }, TypeKind::Tuple { elements: ae }) =
            (&pk.kind, &ak.kind)
            && pe.len() == ae.len()
        {
            let pe = pe.clone();
            let ae = ae.clone();
            for (p, a) in pe.iter().zip(ae.iter()) {
                self.collect_witnesses(*p, *a, tbl, call_range);
            }
        }
    }

    fn visit_decl(&mut self, decl_id: Idx<Decl>) {
        let decl = self.hir.decls[decl_id].clone();
        match decl {
            Decl::Fn(d) => self.visit_fn_decl(&d),
            Decl::Type(d) => self.visit_type_decl(&d),
            Decl::Enum(_) => {}
            Decl::Var(d) => self.visit_top_var(&d),
            Decl::Pragma(p) => self.visit_pragma(&p),
        }
    }

    fn visit_fn_decl(&mut self, d: &FnDecl) {
        // P12.1: register the fn's generic params into scope so
        // `lower_type_ref` mints `GenericParam` for each `T` mention
        // instead of falling back to `any`.
        let owner = GenericOwner::Function(self.hir.idents[d.name].text.as_str().into());
        self.push_generic_scope(&d.generics, owner);
        // Bind parameter types into def_types so identifier inference
        // produces real types instead of `any`.
        for p_id in &d.params {
            let p = self.hir.fn_params[*p_id].clone();
            let ty =
                p.ty.map(|t| self.lower_type_ref(t))
                    .unwrap_or_else(|| self.any());
            self.out.def_types.insert(p.name, ty);
        }
        let return_ty = d
            .return_type
            .map(|t| self.lower_type_ref(t))
            .unwrap_or_else(|| self.any());
        if let Some(body) = d.body {
            self.visit_stmt(body, Some(return_ty));
        }
        self.pop_generic_scope();
    }

    fn visit_type_decl(&mut self, d: &TypeDecl) {
        // P12.1: type-level generics are visible in attrs + method
        // signatures.
        let type_name: SmolStr = self.hir.idents[d.name].text.as_str().into();
        // Inheritance-depth check: the runtime caps `extends` chains
        // at MAX_INHERITANCE_DEPTH types (including the leaf). A
        // declaration past that limit fails to build with
        // "too depth inheritance: <name>". Surface it as a structural
        // error at the type's declaration site so the user sees the
        // problem before they hit `greycat build`.
        let chain_len = self.index.supertype_chain_length(&type_name);
        if chain_len > crate::stdlib::ProjectIndex::MAX_INHERITANCE_DEPTH {
            let limit = crate::stdlib::ProjectIndex::MAX_INHERITANCE_DEPTH;
            let span = d
                .supertype
                .map(|tr| self.hir.type_refs[tr].byte_range.clone())
                .unwrap_or_else(|| self.hir.idents[d.name].byte_range.clone());
            self.diag(
                Severity::Error,
                format!(
                    "inheritance chain too deep: `{type_name}` is {chain_len} levels deep; \
                     greycat allows at most {limit}"
                ),
                span,
            );
        }
        let owner = GenericOwner::Type(type_name.clone());
        self.push_generic_scope(&d.generics, owner.clone());
        // **P19.11** — build the `this` TypeId. For non-generic
        // types it's `Named { name }`; for generic types it's
        // `Generic { name, args: [GenericParam(g0), GenericParam(g1), ...] }`.
        // Push it on `this_stack` so `LiteralKind::This` inside
        // method bodies returns the right thing. Done *after* the
        // generic scope is pushed so generics resolve.
        let this_ty = if d.generics.is_empty() {
            self.arena.named(type_name)
        } else {
            let args: Vec<TypeId> = d
                .generics
                .iter()
                .map(|g| {
                    let g_name = self.hir.idents[*g].text.clone();
                    self.arena.generic_param(g_name, owner.clone())
                })
                .collect();
            self.arena.generic(type_name, args)
        };
        self.this_stack.push(this_ty);
        for attr_id in &d.attrs {
            let a = self.hir.type_attrs[*attr_id].clone();
            self.visit_type_attr(&a);
        }
        for method_id in &d.methods {
            if let Decl::Fn(fnd) = self.hir.decls[*method_id].clone() {
                self.visit_fn_decl(&fnd);
            }
        }
        self.this_stack.pop();
        self.pop_generic_scope();
    }

    fn push_generic_scope(&mut self, generics: &[Idx<Ident>], owner: GenericOwner) {
        let mut frame = FxHashMap::default();
        for g in generics {
            let name = self.hir.idents[*g].text.clone();
            frame.insert(name, owner.clone());
        }
        self.generics_in_scope.push(frame);
    }

    fn pop_generic_scope(&mut self) {
        self.generics_in_scope.pop();
    }

    fn lookup_generic(&self, name: &str) -> Option<GenericOwner> {
        for frame in self.generics_in_scope.iter().rev() {
            if let Some(owner) = frame.get(name) {
                return Some(owner.clone());
            }
        }
        None
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

    fn visit_top_var(&mut self, d: &VarDeclTop) {
        let _ = d.ty.map(|t| self.lower_type_ref(t));
        if let Some(init) = d.init {
            let _ = self.visit_expr(init);
        }
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
    fn visit_block(
        &mut self,
        block: &greycat_analyzer_hir::types::BlockStmt,
        return_ty: Option<TypeId>,
    ) {
        self.push_narrow();
        for s in &block.stmts {
            self.visit_stmt(*s, return_ty);
        }
        self.pop_narrow();
    }

    fn visit_stmt(&mut self, stmt_id: Idx<Stmt>, return_ty: Option<TypeId>) {
        let stmt = self.hir.stmts[stmt_id].clone();
        match stmt {
            Stmt::Block(b) => {
                self.push_narrow();
                for s in b.stmts {
                    self.visit_stmt(s, return_ty);
                }
                self.pop_narrow();
            }
            Stmt::Expr(e) => {
                let _ = self.visit_expr(e);
            }
            Stmt::Var(LocalVar { name, ty, init, .. }) => {
                let declared = ty.map(|t| self.lower_type_ref(t));
                let init_ty = init.map(|i| self.visit_expr(i));
                // Type-relation diagnostic deferred to
                // `ProjectAnalysis::validate_type_relations`.
                let var_ty = declared.or(init_ty).unwrap_or_else(|| self.any());
                self.out.def_types.insert(name, var_ty);
            }
            Stmt::Assign(AssignStmt { target, value, .. }) => {
                // Lowering currently produces `=` as `Expr::Binary`
                // wrapped in `Stmt::Expr`, so this arm is effectively
                // dead — kept for exhaustiveness and any future
                // grammar shape that may revive it. Narrow logic
                // lives in `Expr::Binary` (op = "=").
                let _ = self.visit_expr(target);
                let value_ty = self.visit_expr(value);
                self.record_assign_narrow(target, value_ty);
            }
            Stmt::If(IfStmt {
                condition,
                then_branch,
                else_branch,
                byte_range,
            }) => {
                self.expect_bool(condition, "if condition");
                // P6.6 exhaustiveness: only run from a "head" if (i.e.
                // not already accounted for as a nested else-if).
                if !self.chain_member_ifs.contains(&stmt_id) {
                    self.check_enum_exhaustiveness(stmt_id, byte_range.clone());
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
                } = self.derive_cond_narrows(condition);

                self.push_narrow();
                for ident in &then_non_null {
                    if let Some(cur) = self.lookup_def_type(*ident) {
                        let stripped = self.strip_nullable(cur);
                        self.write_narrow(*ident, stripped);
                    }
                }
                for (ident, ty_ref) in &then_typed {
                    let ty = self.lower_type_ref(*ty_ref);
                    self.write_narrow(*ident, ty);
                }
                for path in &then_member_non_null {
                    self.write_member_non_null(path.clone());
                }
                for (path, ty_ref) in &then_member_typed {
                    let ty = self.lower_type_ref(*ty_ref);
                    self.write_member_typed(path.clone(), ty);
                }
                // **P19.16** — inline the then-branch's stmts (instead
                // of `visit_block`) so the narrow frame we just pushed
                // captures any assignments inside the branch. The
                // post-if join then sees those narrows. `visit_block`
                // would push+pop its own frame, discarding them.
                for s in &then_branch.stmts {
                    self.visit_stmt(*s, return_ty);
                }
                let then_branch_narrows: FxHashMap<Idx<Ident>, TypeId> =
                    self.narrows.last().cloned().unwrap_or_default();
                let then_branch_member_narrows: FxHashSet<String> =
                    self.member_narrows.last().cloned().unwrap_or_default();
                self.pop_narrow();
                let then_terminates = block_terminates(self.hir, &then_branch);

                let (else_terminates, else_branch_narrows, else_branch_member_narrows) =
                    if let Some(eb) = else_branch {
                        self.push_narrow();
                        for ident in &else_non_null {
                            if let Some(cur) = self.lookup_def_type(*ident) {
                                let stripped = self.strip_nullable(cur);
                                self.write_narrow(*ident, stripped);
                            }
                        }
                        for path in &else_member_non_null {
                            self.write_member_non_null(path.clone());
                        }
                        for (ident, ty_ref) in &else_typed {
                            let ty = self.lower_type_ref(*ty_ref);
                            self.write_narrow(*ident, ty);
                        }
                        for (path, ty_ref) in &else_member_typed {
                            let ty = self.lower_type_ref(*ty_ref);
                            self.write_member_typed(path.clone(), ty);
                        }
                        // P19.16 — same inline pattern for the else
                        // branch. `eb` may be a Block or a nested If
                        // (`else if`); for the Block case we inline,
                        // for the If case we still call visit_stmt
                        // (an If handles its own narrows internally).
                        if let Stmt::Block(eb_block) = &self.hir.stmts[eb] {
                            let eb_block = eb_block.clone();
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

                // P13.1 CFG-aware narrowing — early return / throw etc.
                // If the then-branch always exits the surrounding flow
                // (return / throw / break / continue), the post-if
                // scope inherits the *else* condition's narrowing
                // (e.g. `if (x == null) { return; } use(x);` — `x` is
                // non-null after the if). Mirrored for the else side.
                if then_terminates {
                    for ident in &else_non_null {
                        if let Some(cur) = self.lookup_def_type(*ident) {
                            let stripped = self.strip_nullable(cur);
                            self.write_narrow(*ident, stripped);
                        }
                    }
                    for path in &else_member_non_null {
                        self.write_member_non_null(path.clone());
                    }
                    for (ident, ty_ref) in &else_typed {
                        let ty = self.lower_type_ref(*ty_ref);
                        self.write_narrow(*ident, ty);
                    }
                    for (path, ty_ref) in &else_member_typed {
                        let ty = self.lower_type_ref(*ty_ref);
                        self.write_member_typed(path.clone(), ty);
                    }
                }
                if else_terminates {
                    for ident in &then_non_null {
                        if let Some(cur) = self.lookup_def_type(*ident) {
                            let stripped = self.strip_nullable(cur);
                            self.write_narrow(*ident, stripped);
                        }
                    }
                    for (ident, ty_ref) in &then_typed {
                        let ty = self.lower_type_ref(*ty_ref);
                        self.write_narrow(*ident, ty);
                    }
                    for path in &then_member_non_null {
                        self.write_member_non_null(path.clone());
                    }
                    for (path, ty_ref) in &then_member_typed {
                        let ty = self.lower_type_ref(*ty_ref);
                        self.write_member_typed(path.clone(), ty);
                    }
                }

                // **P19.16** — post-if assignment-narrow lift. For
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
                                    Some(self.strip_nullable(pre))
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
                                    Some(self.strip_nullable(pre))
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(pre);
                        if !self.arena.get(then_eff).nullable && !self.arena.get(else_eff).nullable
                        {
                            // Use the non-null stripping of pre to
                            // keep the narrow uniform — both paths
                            // agreed the binding is non-null, so the
                            // value's exact type at each branch end
                            // doesn't load-bear here.
                            let stripped = self.strip_nullable(pre);
                            self.write_narrow(ident, stripped);
                        }
                    }
                    // **P19.16** — same lift for member-access paths.
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
                    let to_lift: Vec<String> = member_candidates
                        .iter()
                        .filter(|&&p| {
                            let then_ok = then_branch_member_narrows.contains(p)
                                || then_member_non_null.contains(p);
                            let else_ok = if else_branch.is_some() {
                                else_branch_member_narrows.contains(p)
                                    || else_member_non_null.contains(p)
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
                        .map(|s| (*s).clone())
                        .collect();
                    for path in to_lift {
                        self.write_member_non_null(path);
                    }
                }
            }
            Stmt::While(WhileStmt {
                condition, body, ..
            }) => {
                self.expect_bool(condition, "while condition");
                self.visit_block(&body, return_ty);
            }
            Stmt::DoWhile(DoWhileStmt {
                condition, body, ..
            }) => {
                self.visit_block(&body, return_ty);
                self.expect_bool(condition, "do-while condition");
            }
            Stmt::For(ForStmt {
                init_name,
                init_ty,
                init_value,
                condition,
                increment,
                body,
                ..
            }) => {
                // **P19.14** — bind the C-style for loop's
                // `init_name` to its declared / inferred type so
                // uses of the loop var inside `condition` /
                // `increment` / `body` get a real type instead of
                // falling back to `any`. Order matters: visit the
                // init value FIRST (so its type is known), bind
                // `init_name` to declared-or-inferred, *then*
                // visit the rest.
                let init_value_ty = init_value.map(|v| self.visit_expr(v));
                if let Some(name) = init_name {
                    let bound_ty = init_ty
                        .map(|t| self.lower_type_ref(t))
                        .or(init_value_ty)
                        .unwrap_or_else(|| self.any());
                    self.out.def_types.insert(name, bound_ty);
                }
                if let Some(c) = condition {
                    self.expect_bool(c, "for condition");
                }
                if let Some(i) = increment {
                    let _ = self.visit_expr(i);
                }
                self.visit_block(&body, return_ty);
            }
            Stmt::ForIn(ForInStmt {
                params,
                range,
                body,
                ..
            }) => {
                let range_ty = self.visit_expr(range);
                // P18.x — bind each iterator param's def_type from the
                // iterable's element type. Grammar guarantees
                // `params.len() >= 2` (tuple form). Common shapes:
                //   - `Array<T>`  -> (index: int, value: T)
                //   - `Map<K, V>` -> (key: K,    value: V)
                //   - `Set<T>`    -> (index: int, value: T)
                //   - other       -> all params keep their declared
                //                    type (if any) or `any`.
                let any_id = self.any();
                let int_id = self.primitive(Primitive::Int);
                let time_id = self.primitive(Primitive::Time);
                let geo_id = self.primitive(Primitive::Geo);
                // Receiver is nullable iterables propagate through here too —
                // `for (i, v in arr?)` is valid GreyCat. Strip the optional
                // before pattern-matching the kind so the binding logic is
                // the same shape with or without the `?` marker.
                let underlying_ty = if self.arena.get(range_ty).nullable {
                    let mut t = self.arena.get(range_ty).clone();
                    t.nullable = false;
                    self.arena.alloc(t)
                } else {
                    range_ty
                };
                let inferred: Vec<TypeId> = match self.arena.get(underlying_ty).kind.clone() {
                    TypeKind::Generic { name, args }
                        if name == "Array" || name == "Set" || name == "nodeList" =>
                    {
                        let elem = args.first().copied().unwrap_or(any_id);
                        if params.len() == 2 {
                            vec![int_id, elem]
                        } else {
                            vec![any_id; params.len()]
                        }
                    }
                    TypeKind::Generic { name, args } if name == "Map" || name == "nodeIndex" => {
                        if args.len() >= 2 && params.len() == 2 {
                            vec![args[0], args[1]]
                        } else {
                            vec![any_id; params.len()]
                        }
                    }
                    TypeKind::Generic { name, args } if name == "nodeTime" => {
                        let elem = args.first().copied().unwrap_or(any_id);
                        if params.len() == 2 {
                            vec![time_id, elem]
                        } else {
                            vec![any_id; params.len()]
                        }
                    }
                    TypeKind::Generic { name, args } if name == "nodeGeo" => {
                        let elem = args.first().copied().unwrap_or(any_id);
                        if params.len() == 2 {
                            vec![geo_id, elem]
                        } else {
                            vec![any_id; params.len()]
                        }
                    }
                    // **P19.15** — bare-named (raw) collection
                    // forms `nodeTime` / `nodeIndex` / `nodeList` /
                    // `Array` / `Map` / `Set` / `nodeGeo` (no
                    // generic args declared) bind keys but the
                    // value type stays `any` because the element
                    // type is unknown.
                    TypeKind::Named { name }
                        if matches!(
                            name.as_str(),
                            "Array" | "Set" | "nodeList" | "nodeTime" | "nodeGeo"
                        ) =>
                    {
                        let key_ty = match name.as_str() {
                            "nodeTime" => time_id,
                            "nodeGeo" => geo_id,
                            _ => int_id,
                        };
                        if params.len() == 2 {
                            vec![key_ty, any_id]
                        } else {
                            vec![any_id; params.len()]
                        }
                    }
                    TypeKind::Named { name } if matches!(name.as_str(), "Map" | "nodeIndex") => {
                        vec![any_id; params.len()]
                    }
                    _ => vec![any_id; params.len()],
                };
                for (p, inf_ty) in params.iter().zip(inferred.iter()) {
                    let bound_ty = match p.ty {
                        Some(t) => self.lower_type_ref(t),
                        None => *inf_ty,
                    };
                    self.out.def_types.insert(p.name, bound_ty);
                }
                self.visit_block(&body, return_ty);
            }
            Stmt::Return(value) => {
                if let Some(v) = value {
                    let _ = self.visit_expr(v);
                    // Type-relation diagnostic deferred to
                    // `ProjectAnalysis::validate_type_relations`.
                    let _ = return_ty;
                }
            }
            Stmt::Break | Stmt::Continue => {}
            Stmt::Throw(e) => {
                let _ = self.visit_expr(e);
            }
            Stmt::Try(TryStmt {
                try_block,
                catch_block,
                ..
            }) => {
                self.visit_block(&try_block, return_ty);
                self.visit_block(&catch_block, return_ty);
            }
            Stmt::At(AtStmt { expr, block, .. }) => {
                let _ = self.visit_expr(expr);
                self.visit_block(&block, return_ty);
            }
        }
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
                    out.then_member_non_null.extend(l.then_member_non_null);
                    out.then_member_non_null.extend(r.then_member_non_null);
                    out.then_member_typed.extend(l.then_member_typed);
                    out.then_member_typed.extend(r.then_member_typed);
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
                    // Then: at least one held — can't narrow either.
                }
                BinOp::Eq | BinOp::Neq => {
                    // Ident-vs-null path (P6.4).
                    if let Some(name_idx) = self.ident_compared_to_null(*left, *right)
                        && let Some(Definition::Param(def) | Definition::Local(def)) =
                            self.res.lookup(name_idx)
                    {
                        match *op {
                            BinOp::Neq => out.then_non_null.push(def),
                            BinOp::Eq => out.else_non_null.push(def),
                            _ => {}
                        }
                        return out;
                    }
                    // **P19.16** — member-access path null comparison.
                    // `foo.bar != null` / `null != foo.bar` (and `==`)
                    // narrow the path on the matching side. Skips
                    // shapes that don't root in an Ident / `this` —
                    // those have no stable identity.
                    if let Some(path) = self.member_compared_to_null(*left, *right) {
                        match *op {
                            BinOp::Neq => out.then_member_non_null.push(path),
                            BinOp::Eq => out.else_member_non_null.push(path),
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            // P6.5: `x is T` narrows x to T in the then-branch.
            // Also: `foo.bar is T` / `foo->bar is T` narrows the member
            // path the same way (record by path string).
            Expr::Is { value, ty, .. } => {
                if let Expr::Ident { name: name_idx, .. } = &self.hir.exprs[*value]
                    && let Some(Definition::Param(def) | Definition::Local(def)) =
                        self.res.lookup(*name_idx)
                {
                    out.then_typed.push((def, *ty));
                } else if matches!(self.hir.exprs[*value], Expr::Member(_) | Expr::Arrow(_))
                    && let Some(path) = self.member_path(*value)
                {
                    out.then_member_typed.push((path, *ty));
                }
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
            }
            _ => {}
        }
        out
    }

    // P6.6
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
        let Some(enum_id) = self.out.registry.lookup(&chain.enum_name) else {
            return;
        };
        let enum_ty = self.arena.get(enum_id);
        let TypeKind::Enum { variants, .. } = &enum_ty.kind else {
            return;
        };
        let variants = variants.clone();
        let covered: FxHashSet<&str> = chain.arms.iter().map(|a| a.variant.as_str()).collect();
        let missing: Vec<&str> = variants
            .iter()
            .map(SmolStr::as_str)
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
            // P25.6
            enum_name: chain.enum_name.as_str().into(),
            missing: missing.iter().map(|v| SmolStr::from(*v)).collect(),
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
            enum_name,
            arms,
            has_final_else,
        })
    }

    fn match_enum_eq(&self, cond_id: Idx<Expr>) -> Option<(Idx<Ident>, String, String)> {
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

    fn try_extract_eq(
        &self,
        ident_side: Idx<Expr>,
        static_side: Idx<Expr>,
    ) -> Option<(Idx<Ident>, String, String)> {
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
        let enum_name = self.hir.idents[self.hir.type_refs[*ty].name]
            .text
            .to_string();
        let variant = self.hir.idents[property.ident()].text.to_string();
        Some((binding, enum_name, variant))
    }

    fn ident_compared_to_null(&self, l: Idx<Expr>, r: Idx<Expr>) -> Option<Idx<Ident>> {
        let le = &self.hir.exprs[l];
        let re = &self.hir.exprs[r];
        if let (
            Expr::Ident { name, .. },
            Expr::Literal(LiteralExpr {
                kind: LiteralKind::Null,
                ..
            }),
        ) = (le, re)
        {
            return Some(*name);
        }
        if let (
            Expr::Literal(LiteralExpr {
                kind: LiteralKind::Null,
                ..
            }),
            Expr::Ident { name, .. },
        ) = (le, re)
        {
            return Some(*name);
        }
        None
    }

    // P19.16 + P19.21
    /// `foo.bar != null` / `null != foo.bar`
    /// (and `==`, plus the `->` arrow form `foo->bar`) shape detection.
    /// Returns the member-access path string when one side is an
    /// `Expr::Member` / `Expr::Arrow` rooted at an Ident / `this` and
    /// the other side is the null literal. Returns `None` for any
    /// other shape (so e.g. `foo.bar == baz.qux` or `f().x != null`
    /// don't participate).
    fn member_compared_to_null(&self, l: Idx<Expr>, r: Idx<Expr>) -> Option<String> {
        let is_null_lit = |id: Idx<Expr>| {
            matches!(
                &self.hir.exprs[id],
                Expr::Literal(LiteralExpr {
                    kind: LiteralKind::Null,
                    ..
                })
            )
        };
        let is_member_or_arrow =
            |id: Idx<Expr>| matches!(self.hir.exprs[id], Expr::Member(_) | Expr::Arrow(_));
        if is_member_or_arrow(l) && is_null_lit(r) {
            return self.member_path(l);
        }
        if is_member_or_arrow(r) && is_null_lit(l) {
            return self.member_path(r);
        }
        None
    }

    fn expect_bool(&mut self, expr: Idx<Expr>, _label: &'static str) {
        // Type-only: populate `expr_types` so the validation pass can
        // re-check against settled types. The "must be `bool`"
        // diagnostic emission lives in
        // `ProjectAnalysis::validate_type_relations`.
        let _ = self.visit_expr(expr);
    }

    fn visit_expr(&mut self, expr_id: Idx<Expr>) -> TypeId {
        let ty = self.infer_expr(expr_id);
        self.record(expr_id, ty);
        ty
    }

    fn infer_expr(&mut self, expr_id: Idx<Expr>) -> TypeId {
        let expr = self.hir.exprs[expr_id].clone();
        match expr {
            Expr::Ident { name: idx, .. } => match self.res.lookup(idx) {
                Some(Definition::Param(def)) | Some(Definition::Local(def)) => {
                    self.lookup_def_type(def).unwrap_or_else(|| self.any())
                }
                Some(Definition::Decl(decl_id)) => match &self.hir.decls[decl_id] {
                    Decl::Var(vd) => vd
                        .ty
                        .map(|ty_ref| self.lower_type_ref(ty_ref))
                        .unwrap_or_else(|| self.any()),
                    // P23 — bare type / enum / fn references used as
                    // values were typed by pass 3.5 before. Now type
                    // them inline against the runtime "type" /
                    // "function" named shapes.
                    Decl::Type(_) | Decl::Enum(_) => self.arena.named("type"),
                    Decl::Fn(_) => self.arena.named("function"),
                    _ => self.any(),
                },
                Some(Definition::ProjectDecl { .. }) => {
                    // P23 — cross-module bare ident value typing via
                    // the project signatures index. **P19.10** —
                    // top-level vars get their declared type from
                    // `var_types` (lowered in S7-S11). Without this,
                    // `var groups: nodeIndex<String, node<Group>>`
                    // referenced from another module would fall
                    // through to `index.has_name` (vars are in
                    // `values`) and type as `type`, breaking
                    // for-in iteration over the foreign var.
                    let name = self.ident_text(idx);
                    if let Some(var_ty) = self.index.var_type_for(name) {
                        var_ty
                    } else if self.index.contains_fn_signature(name) {
                        self.arena.named("function")
                    } else if self.index.contains_type_member(name) || self.index.has_name(name) {
                        self.arena.named("type")
                    } else {
                        self.any()
                    }
                }
                Some(Definition::Project) => {
                    // **P19.16** — runtime-exposed value-position
                    // globals (e.g. `Infinity`, `NaN`) carry a fixed
                    // type the runtime owns; without this lookup the
                    // body walker would type them as `any` and float
                    // dispatch downstream would fail.
                    let name = self.ident_text(idx);
                    self.index
                        .runtime_global_for(name)
                        .unwrap_or_else(|| self.any())
                }
                Some(Definition::Generic(_)) | None => self.any(),
            },
            Expr::Literal(LiteralExpr { kind, text, .. }) => match kind {
                LiteralKind::Bool => self.primitive(Primitive::Bool),
                LiteralKind::Number => {
                    // P12.4: differentiate int vs float numeric literals
                    // by inspecting the source text. `1`, `42`, `0xff`,
                    // `0b10` lower to `int`; literals with a decimal
                    // point, scientific exponent, or trailing `_f`
                    // suffix lower to `float`. Other typed suffixes
                    // (`_time`, `_duration`, …) keep `Number`-shaped
                    // text but the lowering layer should mint a typed
                    // `LiteralKind` for them (P13.3 deepens this).
                    self.primitive(numeric_literal_kind(text.as_str()))
                }
                LiteralKind::Char => self.primitive(Primitive::Char),
                LiteralKind::Null => self.null(),
                LiteralKind::This => self
                    .this_stack
                    .last()
                    .copied()
                    .unwrap_or_else(|| self.any()),
                LiteralKind::Duration => self.primitive(Primitive::Duration),
                LiteralKind::Time | LiteralKind::Iso8601 => self.primitive(Primitive::Time),
            },
            Expr::String(StringExpr { parts, .. }) => {
                // P17.5 — visit each `${expr}` interpolation so the
                // analyzer types and binds the inner identifiers
                // (otherwise locals referenced only inside template
                // strings would surface as `unused-local` and never
                // get an `expr_types` entry).
                for part in &parts {
                    if let greycat_analyzer_hir::types::StringPart::Interp { expr, .. } = part {
                        let _ = self.visit_expr(*expr);
                    }
                }
                self.primitive(Primitive::String)
            }
            Expr::Tuple(items, _) => {
                let elems: Vec<TypeId> = items.iter().map(|i| self.visit_expr(*i)).collect();
                self.arena.tuple(elems)
            }
            Expr::Array(items, _) => {
                for i in items.iter() {
                    let _ = self.visit_expr(*i);
                }
                let any = self.any();
                self.arena.generic("Array", vec![any])
            }
            Expr::Object(ObjectExpr { ty, fields, .. }) => {
                for f in &fields {
                    let _ = self.visit_expr(f.value);
                }
                if let Some(t) = ty {
                    self.lower_type_ref(t)
                } else {
                    self.any()
                }
            }
            Expr::Member(MemberExpr {
                receiver,
                property,
                pre_optional,
                post_optional,
                ..
            })
            | Expr::Arrow(MemberExpr {
                receiver,
                property,
                pre_optional,
                post_optional,
                ..
            }) => {
                let property = property.ident();
                let recv_ty = self.visit_expr(receiver);
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
                                .unwrap_or_else(|| self.any())
                        }
                        MemberDef::Method(_) => self.arena.named("function"),
                    }
                } else if self.out.foreign_member_uses.contains_key(&property) {
                    // P22 — cross-module attr / method typing inline.
                    // Reads the project signatures index built in S7
                    // (`stage_lower_signatures`) and applies generic
                    // substitution from the receiver's instantiation.
                    self.foreign_member_type(resolution_ty, property)
                        .unwrap_or_else(|| self.any())
                } else {
                    self.any()
                };
                // P16.7 + P19.17 — nullability propagates *up the chain*
                // whenever the receiver is nullable, regardless of
                // whether the user wrote `?.` at this segment. The
                // runtime evaluates the whole chain to null when any
                // prior `?.` shorts, so `x?.y.z` types as `Z?`. The
                // `pre_optional` flag is what the lint reads to decide
                // whether to flag the dereference as "possibly null"
                // (no flag → flag fires), but it doesn't change typing.
                // `a.b?` / `a->b?` still lifts unconditionally as a
                // user-asserted "treat as nullable" override.
                let _ = pre_optional;
                let recv_nullable = self.arena.get(recv_ty).nullable;
                let result_ty = if recv_nullable || post_optional {
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
                    self.strip_nullable(result_ty)
                } else {
                    result_ty
                }
            }
            Expr::Static(s) => {
                // P15.6 — `Type::method` resolution. Lower the receiver
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
                    let prop = self.hir.idents[property].text.as_str();
                    if variants.iter().any(|v| v == prop) {
                        return recv_ty;
                    }
                }
                // P23 — `Type::attr` (no parens) → `field`,
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
                // P23 — `module::Name` shapes parse as `Static` with
                // the module name as the "type ref" (the parser
                // doesn't distinguish modules from types). Fall back
                // to a 2-segment QualifiedStatic-style lookup against
                // the project signatures index.
                let recv_name = self.hir.type_refs[s.ty].name;
                let chain = [recv_name, property];
                self.qualified_static_value_type(&chain)
                    .unwrap_or_else(|| self.any())
            }
            Expr::QualifiedStatic { chain, .. } => {
                // P23 — chained `module::name` / `module::Type::name`
                // shapes. Bind the chain segments to their foreign
                // decls / members so hover / goto-def have something
                // to point at, then type the value-position expr
                // inline using the project signatures index. (Calls
                // are routed through `try_member_call_typing` from
                // the `Expr::Call` branch.)
                self.bind_qualified_chain_segments(&chain);
                self.qualified_static_value_type(&chain)
                    .unwrap_or_else(|| self.any())
            }
            Expr::Offset(OffsetExpr {
                receiver,
                index,
                pre_optional,
                post_optional,
                ..
            }) => {
                let recv_ty = self.visit_expr(receiver);
                let _ = self.visit_expr(index);
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
                let index_is_range = matches!(&self.hir.exprs[index], Expr::Range { .. });
                let base = if index_is_range {
                    underlying
                } else {
                    match &self.arena.get(underlying).kind {
                        TypeKind::Generic { name, args }
                            if (name == "Array" || name == "Set" || name == "nodeList")
                                && !args.is_empty() =>
                        {
                            args[0]
                        }
                        TypeKind::Generic { name, args }
                            if (name == "Map" || name == "nodeIndex") && args.len() >= 2 =>
                        {
                            args[1]
                        }
                        TypeKind::Generic { name, args }
                            if name == "nodeTime" && !args.is_empty() =>
                        {
                            args[0]
                        }
                        _ => self.any(),
                    }
                };
                let lift_pre = pre_optional && self.arena.get(recv_ty).nullable;
                if lift_pre || post_optional {
                    self.arena.nullable(base)
                } else {
                    base
                }
            }
            Expr::Call(CallExpr { callee, args, .. }) => {
                let callee_ty = self.visit_expr(callee);
                let arg_tys: Vec<TypeId> = args.iter().map(|a| self.visit_expr(*a)).collect();
                // P12.1: if the callee resolves to an in-module fn decl
                // with generics, run constraint-based inference.
                let call_range = self.hir.exprs[expr_id].byte_range();
                if let Some(ret) = self.try_generic_call_inference(callee, &arg_tys, call_range) {
                    return ret;
                }
                // P23 — inline call-return typing for Member / Arrow /
                // Static method calls. Pulls the method's lowered
                // return type from the S7 signatures index and applies
                // `arena.substitute` against the receiver's
                // instantiation. Replaces pass 3.5 + the receiver-
                // driven shape-substitution shim for these shapes.
                if let Some(ret) = self.try_member_call_typing(callee) {
                    return ret;
                }
                // P15.10: pairwise arg-type validation runs in
                // `ProjectAnalysis::validate_type_relations` so outer
                // calls whose args contain inner static-expr calls
                // validate against settled arg types. Doing it here
                // would surface false positives for arg shapes whose
                // type isn't known until pass 3.5 fixes them up.
                let _ = callee_ty;
                self.any()
            }
            Expr::Binary(BinaryExpr {
                op, left, right, ..
            }) => {
                let lt = self.visit_expr(left);
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
                        } = self.derive_cond_narrows(left);
                        let (non_null, typed, member_non_null, member_typed) = match op {
                            BinOp::And => (
                                then_non_null,
                                then_typed,
                                then_member_non_null,
                                then_member_typed,
                            ),
                            BinOp::Or => {
                                (else_non_null, Vec::new(), else_member_non_null, Vec::new())
                            }
                            _ => unreachable!(),
                        };
                        self.push_narrow();
                        for ident in &non_null {
                            if let Some(cur) = self.lookup_def_type(*ident) {
                                let stripped = self.strip_nullable(cur);
                                self.write_narrow(*ident, stripped);
                            }
                        }
                        for (ident, ty_ref) in &typed {
                            let ty = self.lower_type_ref(*ty_ref);
                            self.write_narrow(*ident, ty);
                        }
                        for path in member_non_null {
                            self.write_member_non_null(path);
                        }
                        for (path, ty_ref) in member_typed {
                            let ty = self.lower_type_ref(ty_ref);
                            self.write_member_typed(path, ty);
                        }
                        let rt = self.visit_expr(right);
                        self.pop_narrow();
                        rt
                    }
                    _ => self.visit_expr(right),
                };
                // **P19.16** — GreyCat's `=` parses as a binary
                // expression (not a Stmt::Assign). When the LHS is
                // a Param/Local Ident, narrow its binding to the
                // RHS's type for the rest of the enclosing block.
                // The post-if join logic then lifts narrows that
                // hold along every path.
                if matches!(op, BinOp::Other("=")) {
                    self.record_assign_narrow(left, rt);
                } else if matches!(op, BinOp::Other("?=")) {
                    self.record_coalesce_assign_narrow(left, rt);
                }
                self.infer_binary(op, lt, rt)
            }
            Expr::Unary(UnaryExpr { op, operand, .. }) => {
                let inner = self.visit_expr(operand);
                match op {
                    UnaryOp::Not => self.primitive(Primitive::Bool),
                    UnaryOp::Neg | UnaryOp::Pos | UnaryOp::BitNot | UnaryOp::Inc | UnaryOp::Dec => {
                        inner
                    }
                    // **P19.14** — `*n` deref. For
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
                        // **P20.2** — when the operand is a stable
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
                        let result = self.strip_nullable(inner);
                        if let Expr::Ident { name: name_idx, .. } = self.hir.exprs[operand].clone()
                            && let Some(Definition::Param(def) | Definition::Local(def)) =
                                self.res.lookup(name_idx)
                        {
                            self.write_narrow(def, result);
                        }
                        if matches!(self.hir.exprs[operand], Expr::Member(_) | Expr::Arrow(_))
                            && let Some(path) = self.member_path(operand)
                        {
                            self.write_member_non_null(path);
                        }
                        result
                    }
                }
            }
            Expr::Paren(inner, _) => self.visit_expr(inner),
            Expr::Lambda(LambdaExpr { params, body, .. }) => {
                let mut param_tys = Vec::with_capacity(params.len());
                for p in &params {
                    let p = self.hir.fn_params[*p].clone();
                    let pt =
                        p.ty.map(|t| self.lower_type_ref(t))
                            .unwrap_or_else(|| self.any());
                    param_tys.push(pt);
                }
                let body_ty = self.visit_expr(body);
                self.arena.lambda(param_tys, body_ty)
            }
            Expr::Is { value, .. } => {
                let _ = self.visit_expr(value);
                self.primitive(Primitive::Bool)
            }
            Expr::Range { from, to, .. } => {
                // **P19.15** — visit both endpoints so their
                // exprs get types in the table; the range itself
                // doesn't have a useful TypeId on its own (it only
                // appears as an offset index or a for-in iterator
                // range, both of which look at the surrounding
                // shape, not the range's own type).
                if let Some(f) = from {
                    let _ = self.visit_expr(f);
                }
                if let Some(t) = to {
                    let _ = self.visit_expr(t);
                }
                self.any()
            }
            Expr::Cast { value, ty, .. } => {
                let from_ty = self.visit_expr(value);
                let to_ty = self.lower_type_ref(ty);
                // P12.3: validate the cast against the GreyCat `as`
                // rules (mirrors TS `isCastable`). Surfaces invalid
                // casts as a diagnostic; the resulting expression
                // type is still `to_ty` so downstream inference
                // doesn't cascade.
                // **P19.14** — inheritance-aware cast: allow up- /
                // down-cast within a supertype chain (e.g.
                // `pvEntity as PVInstallation` where `PVInstallation
                // extends PVEntity`). Both directions are runtime-
                // permitted (downcast may fail at runtime, upcast
                // is widening). Strip nullability so `T?` casts the
                // same way as `T`.
                let inheritance_ok = {
                    let from_kind = &self.arena.get(from_ty).kind;
                    let to_kind = &self.arena.get(to_ty).kind;
                    let extract_name = |k: &TypeKind| match k {
                        TypeKind::Named { name } => Some(name.clone()),
                        TypeKind::Generic { name, .. } => Some(name.clone()),
                        _ => None,
                    };
                    match (extract_name(from_kind), extract_name(to_kind)) {
                        (Some(fn_), Some(tn)) => {
                            self.index.is_subtype_of(&fn_, &tn)
                                || self.index.is_subtype_of(&tn, &fn_)
                        }
                        _ => false,
                    }
                };
                if !inheritance_ok && !is_castable(self.arena, from_ty, to_ty) {
                    let r = self.hir.exprs[expr_id].byte_range();
                    let msg = format!(
                        "cannot cast `{}` to `{}`",
                        greycat_analyzer_types::display(self.arena, from_ty),
                        greycat_analyzer_types::display(self.arena, to_ty),
                    );
                    self.diag(Severity::Error, msg, r);
                }
                to_ty
            }
            Expr::Unsupported { .. } => self.any(),
        }
    }

    fn infer_binary(&mut self, op: BinOp, lt: TypeId, rt: TypeId) -> TypeId {
        let int = self.primitive(Primitive::Int);
        let float = self.primitive(Primitive::Float);
        let bool_t = self.primitive(Primitive::Bool);

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
                let lt_n = self.strip_nullable(lt);
                let rt_n = self.strip_nullable(rt);
                let string_t = self.primitive(Primitive::String);
                let time_t = self.primitive(Primitive::Time);
                let dur_t = self.primitive(Primitive::Duration);
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
                    self.any()
                }
            }
            BinOp::Sub => {
                // **P19.14** — `time - time → duration`,
                // `time - duration → time`,
                // `duration - duration → duration`.
                let lt_n = self.strip_nullable(lt);
                let rt_n = self.strip_nullable(rt);
                let time_t = self.primitive(Primitive::Time);
                let dur_t = self.primitive(Primitive::Duration);
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
                    self.any()
                }
            }
            BinOp::Mul => {
                // **P19.14** — `duration * int / float → duration`.
                let lt_n = self.strip_nullable(lt);
                let rt_n = self.strip_nullable(rt);
                let dur_t = self.primitive(Primitive::Duration);
                if (lt_n == dur_t && (rt_n == int || rt_n == float))
                    || ((lt_n == int || lt_n == float) && rt_n == dur_t)
                {
                    dur_t
                } else if lt_n == float || rt_n == float {
                    float
                } else if lt_n == int && rt_n == int {
                    int
                } else {
                    self.any()
                }
            }
            BinOp::Div | BinOp::Mod => {
                if lt == float || rt == float {
                    float
                } else if lt == int && rt == int {
                    int
                } else {
                    self.any()
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
                let lt_stripped = self.strip_nullable(lt);
                let rt_nullable = self.arena.get(rt).nullable;
                let rt_stripped = self.strip_nullable(rt);
                let merged = if lt_stripped == rt_stripped {
                    lt_stripped
                } else {
                    self.arena.alloc(Type {
                        kind: TypeKind::Union {
                            alts: vec![lt_stripped, rt_stripped],
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
            BinOp::Other(_) => self.any(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::resolve;
    use greycat_analyzer_hir::lower_module;
    use greycat_analyzer_syntax::parse;

    fn analyze_src(src: &str) -> (TypeArena, AnalysisResult) {
        let tree = parse(src);
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let res = resolve(&hir);
        analyze(&hir, &res)
    }

    /// Drop-in helper for tests that don't need to inspect the arena.
    fn analyze_src_only(src: &str) -> AnalysisResult {
        analyze_src(src).1
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
        let uri = greycat_analyzer_core::lsp_types::Uri::from_str("file:///mod.gcl").unwrap();
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

    /// Regression: `2f` (bare `f` suffix, no leading underscore) must
    /// classify as `float`. The leading `_` is a formatter convention,
    /// not a grammar requirement — both `42f` and `42_f` are valid
    /// GreyCat syntax and the analyzer must agree with the runtime
    /// that they're float literals. Earlier code only matched `_f`,
    /// so `foo(2f)` against `fn foo(_: float)` lit up a spurious
    /// `int → float` assignability error.
    #[test]
    fn numeric_literal_kind_recognizes_bare_and_underscored_f_suffix() {
        assert_eq!(super::numeric_literal_kind("2f"), Primitive::Float);
        assert_eq!(super::numeric_literal_kind("2_f"), Primitive::Float);
        assert_eq!(super::numeric_literal_kind("1.5f"), Primitive::Float);
        assert_eq!(super::numeric_literal_kind("1.79e+308_f"), Primitive::Float);
        assert_eq!(super::numeric_literal_kind("2"), Primitive::Int);
        assert_eq!(super::numeric_literal_kind("42"), Primitive::Int);
    }

    /// End-to-end anchor for the same fix: a bare-`f` float literal
    /// flowing into a `float`-typed parameter must not raise an
    /// assignability diagnostic.
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
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let res = resolve(&hir);
        let (_arena, analysis) = analyze(&hir, &res);

        // Find the property ident `x` inside `p.x` — the second `x`
        // ident in the source (the first is the attr decl name).
        let x_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.text == "x")
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
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let res = resolve(&hir);
        let (_arena, analysis) = analyze(&hir, &res);

        let inner_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.text == "inner")
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
        let r = analyze_src_only(src);
        // Recording happens during analysis; the lint pipeline turns
        // this into a `non-exhaustive` LintDiagnostic later.
        assert_eq!(
            r.non_exhaustive_findings.len(),
            1,
            "expected one non-exhaustive finding, got: {:?}",
            r.non_exhaustive_findings
        );
        let finding = &r.non_exhaustive_findings[0];
        assert_eq!(finding.enum_name, "Color");
        assert_eq!(finding.missing, vec!["Blue".to_string()]);
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
        let r = analyze_src_only(src);
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
        let r = analyze_src_only(src);
        assert!(
            r.non_exhaustive_findings.is_empty(),
            "expected final-else to suppress finding, got: {:?}",
            r.non_exhaustive_findings
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
        let src = r#"
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
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let res = resolve(&hir);
        let (_arena, analysis) = analyze(&hir, &res);

        let bogus = hir
            .idents
            .iter()
            .find(|(_, i)| i.text == "bogus")
            .map(|(idx, _)| idx)
            .expect("bogus ident exists");
        assert!(analysis.member_lookup(bogus).is_none());
    }

    // -------------------------------------------------------------------
    // P16.7 — null-safe access notations + `??` widening
    // -------------------------------------------------------------------

    /// Resolve the inferred type for the `init` of `var <name> = …`.
    fn local_init_ty(src: &str, name: &str) -> Option<String> {
        let tree = parse(src);
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let res = resolve(&hir);
        let (arena, analysis) = analyze(&hir, &res);
        for (stmt_id, stmt) in hir.stmts.iter() {
            if let Stmt::Var(v) = stmt
                && hir.idents[v.name].text == name
                && let Some(init) = v.init
            {
                let _ = stmt_id;
                let ty = analysis.expr_types.get(&init).copied()?;
                return Some(greycat_analyzer_types::display(&arena, ty));
            }
        }
        None
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
        // is nullable.
        let src = r#"
type Foo { name: String; }
fn caller(n: node<Foo>?) {
    var s = n?->name;
}
"#;
        assert_eq!(local_init_ty(src, "s").as_deref(), Some("String?"));
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
        assert_eq!(display, "Foo | Bar?");
    }
}
