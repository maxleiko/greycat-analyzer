//! Foundational type analyzer (P2.5).
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

use std::collections::{HashMap, HashSet};
use std::ops::Range;

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
    is_assignable_to, is_castable,
};

use crate::resolver::{Definition, Resolutions};
use crate::stdlib::ProjectIndex;

/// P13.1 — does this statement always exit the enclosing control
/// flow (`return`, `throw`, `break`, `continue`)? `Block` recurses
/// into its last statement. `If` requires *both* branches to
/// terminate (no else → not terminal). Used by the analyzer to lift
/// the else-branch's narrowing into the post-if scope when the
/// then-branch always exits early — handles the `if (x == null)
/// { return; } use(x);` idiom.
fn stmt_terminates(hir: &Hir, stmt_id: Idx<Stmt>) -> bool {
    match &hir.stmts[stmt_id] {
        Stmt::Return(_) | Stmt::Throw(_) | Stmt::Break | Stmt::Continue => true,
        Stmt::Block(stmts) => stmts.last().is_some_and(|s| stmt_terminates(hir, *s)),
        Stmt::If(IfStmt {
            then_branch,
            else_branch,
            ..
        }) => {
            stmt_terminates(hir, *then_branch)
                && else_branch.is_some_and(|e| stmt_terminates(hir, e))
        }
        _ => false,
    }
}

/// P12.4 — classify a numeric literal's source text as `int` or
/// `float`. Returns `Primitive::Float` for literals that contain a
/// decimal point, scientific notation (`1e3`, `1.5E-2`), or trailing
/// `_f` suffix; everything else falls back to `Primitive::Int`. Other
/// typed suffixes (`_time`, `_duration`, …) leave `LiteralKind::Number`
/// untyped today; P13.3 promotes those to dedicated `LiteralKind`
/// variants so this helper only sees float / int candidates.
fn numeric_literal_kind(text: &str) -> Primitive {
    if text.ends_with("_f") {
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
/// at the LSP boundary (P2.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

#[derive(Debug, Clone)]
pub struct SemanticDiagnostic {
    pub severity: Severity,
    pub message: String,
    pub byte_range: Range<usize>,
}

/// Output of the analyzer for a single module.
#[derive(Debug, Default)]
pub struct AnalysisResult {
    pub types: TypeArena,
    pub registry: TypeRegistry,
    /// Per-expression inferred type (subset — entries only for expressions
    /// the analyzer actually visited).
    pub expr_types: HashMap<Idx<Expr>, TypeId>,
    /// Per-binding inferred type. Keyed by the *defining* `Idx<Ident>`
    /// (e.g. the param name in `fn f(x: int)`, the local name in
    /// `var y: T = …`).
    pub def_types: HashMap<Idx<Ident>, TypeId>,
    /// Module-local map from declared type name to its HIR `TypeDecl`.
    /// Built when the analyzer walks top-level decls — lets P6.3
    /// member resolution navigate from a receiver's `TypeId` back to
    /// the declaring node so attr / method idents can be bound.
    pub type_decls: HashMap<String, Idx<Decl>>,
    /// Member-access bindings produced by P6.3: each property ident in
    /// `a.b` / `a->b` that resolves to a [`TypeAttr`] or to a
    /// `TypeDecl::methods` entry, keyed by the property `Idx<Ident>`.
    /// Capabilities consult this in addition to [`Resolutions`] so
    /// goto-definition / hover work on member access.
    pub member_uses: HashMap<Idx<Ident>, MemberDef>,
    /// P11.5 cross-module member bindings — same keying as `member_uses`
    /// but the resolved attr / method lives in another module's HIR.
    /// Populated by [`crate::project::ProjectAnalysis::rebuild`] after
    /// every module's analyzer pass runs, by walking each module's
    /// [`Self::deferred_member_uses`] against the global decl table.
    pub foreign_member_uses: HashMap<Idx<Ident>, ForeignMember>,
    /// P11.5 — `(property_ident, receiver_type_name)` pairs the analyzer
    /// couldn't bind locally because the receiver's type isn't declared
    /// in this module. The project pipeline drains these in a post-pass
    /// against [`crate::stdlib::ProjectIndex::decl_locations`].
    pub deferred_member_uses: Vec<(Idx<Ident>, String)>,
    /// P15.x — chain-segment bindings populated by `ProjectAnalysis`
    /// pass 3.5 for `Expr::QualifiedStatic` shapes. Each segment ident
    /// (chain[1] = the type, chain[2] = the member when length is 3)
    /// binds to the foreign top-level decl that declares it. Lets
    /// hover / goto-def show the right content for each segment of
    /// `runtime::Identity::create`.
    pub foreign_decl_uses: HashMap<Idx<Ident>, ForeignDecl>,
    pub diagnostics: Vec<SemanticDiagnostic>,
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

/// P11.5 — a member-access binding that resolves into another module.
/// `uri` names the home module of the foreign type's declaration; the
/// `member` indices reference that module's HIR arenas, not the
/// analyzed module's.
#[derive(Debug, Clone)]
pub struct ForeignMember {
    pub uri: greycat_analyzer_core::lsp_types::Uri,
    pub member: MemberDef,
}

/// P15.x — a top-level decl reference resolved into another module.
/// Used for chain-segment bindings (`runtime::Identity::create` —
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

    /// Look up a member-access ident's binding (P6.3). Returns the
    /// declaring `TypeAttr` or method `Decl` if member resolution
    /// succeeded for this ident.
    pub fn member_lookup(&self, ident: Idx<Ident>) -> Option<MemberDef> {
        self.member_uses.get(&ident).copied()
    }

    /// P11.5 — look up a cross-module member-access binding for `ident`.
    /// Falls back to `None` for members that are intra-module
    /// ([`Self::member_lookup`]) or unresolved.
    pub fn foreign_member_lookup(&self, ident: Idx<Ident>) -> Option<&ForeignMember> {
        self.foreign_member_uses.get(&ident)
    }

    /// P15.x — look up a chain-segment binding (e.g. `Identity` in
    /// `runtime::Identity::create` -> the foreign type decl).
    pub fn foreign_decl_lookup(&self, ident: Idx<Ident>) -> Option<&ForeignDecl> {
        self.foreign_decl_uses.get(&ident)
    }
}

/// Run the analyzer with no cross-module project context. Falls back
/// to an empty [`ProjectIndex`]; cross-module type names lower to
/// `any` and `deferred_member_uses` gets nothing the project pipeline
/// can resolve. Used by per-file capabilities and unit tests.
pub fn analyze(hir: &Hir, res: &Resolutions) -> AnalysisResult {
    let index = ProjectIndex::new();
    analyze_with_index(hir, res, &index)
}

/// Run the analyzer with a shared project index. The index is read-
/// only — it's only consulted when `lower_type_ref` doesn't find a
/// name in the per-module registry, so cross-module type references
/// (`p: Point` where `Point` is declared in another module) lower to
/// the right `Named` shape and `resolve_member` can defer
/// `(property, type_name)` for the project's cross-module member
/// post-pass (P11.5).
pub fn analyze_with_index(hir: &Hir, res: &Resolutions, index: &ProjectIndex) -> AnalysisResult {
    let mut out = AnalysisResult::default();
    seed_builtins(&mut out.types);
    register_module_types(hir, &mut out);

    let Some(module) = hir.module.as_ref() else {
        return out;
    };
    let mut cx = Cx {
        hir,
        res,
        out: &mut out,
        index,
        narrows: Vec::new(),
        chain_member_ifs: HashSet::new(),
        generics_in_scope: Vec::new(),
    };
    for d in &module.decls {
        cx.visit_decl(*d);
    }

    // Surface resolver's unresolved-name list as analyzer diagnostics so
    // P2.7 (LSP publish) only needs one list per file.
    let unresolved = res.unresolved.clone();
    for ident_idx in unresolved {
        let ident = &hir.idents[ident_idx];
        out.diagnostics.push(SemanticDiagnostic {
            severity: Severity::Error,
            message: format!("unresolved name `{}`", ident.text),
            byte_range: ident.byte_range.clone(),
        });
    }

    out
}

/// Seed primitive type ids in the arena so cx.{int, bool, ...} are cheap.
fn seed_builtins(arena: &mut TypeArena) {
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
/// `TypeDecl` index) so P6.3 member resolution can navigate from a
/// receiver's `TypeId` back to the declaring node.
fn register_module_types(hir: &Hir, out: &mut AnalysisResult) {
    let Some(module) = hir.module.as_ref() else {
        return;
    };
    for d in &module.decls {
        let decl = &hir.decls[*d];
        match decl {
            Decl::Type(td) => {
                let name = hir.idents[td.name].text.clone();
                let id = out.types.named(&name);
                out.registry.register(name.clone(), id);
                out.type_decls.insert(name, *d);
            }
            Decl::Enum(ed) => {
                let name = hir.idents[ed.name].text.clone();
                let variants: Vec<String> = ed
                    .fields
                    .iter()
                    .map(|f| hir.idents[hir.enum_fields[*f].name].text.clone())
                    .collect();
                let id = out.types.alloc(Type {
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

/// Narrowings derived from an `if` condition (P6.4 / P6.5). Each list
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
}

/// One arm in an enum-equality chain (P6.6).
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

struct Cx<'a> {
    hir: &'a Hir,
    res: &'a Resolutions,
    out: &'a mut AnalysisResult,
    /// P11.5: cross-module project index. Per-file callers pass an
    /// empty [`ProjectIndex::new`]; the project pipeline passes the
    /// index it just rebuilt. Used by `lower_type_ref` to recognize
    /// type names that aren't declared in this module.
    index: &'a ProjectIndex,
    /// Null-flow narrowing stack (P6.4). Each frame is a binding ident
    /// → temporary `TypeId` override. Frames are pushed on block /
    /// then-branch / else-branch entry and popped on exit, so a
    /// narrowing introduced inside a block stays alive for the rest
    /// of that block but doesn't leak to siblings.
    narrows: Vec<HashMap<Idx<Ident>, TypeId>>,
    /// `Stmt::If` ids already accounted for as nested members of an
    /// enclosing exhaustiveness chain (P6.6). Suppresses duplicate
    /// "non-exhaustive" diagnostics on inner `else if` arms.
    chain_member_ifs: HashSet<Idx<Stmt>>,
    /// P12.1 generic-context stack: type-parameter names visible at the
    /// current scope, mapped to their declaring [`GenericOwner`].
    /// Entered on `fn f<T>(...)` / `type Foo<T> {}` and used by
    /// `lower_type_ref` to mint `GenericParam(name, owner)` instead
    /// of `Named(name)` / `Any` for in-scope generics. The stack is a
    /// `Vec<HashMap>` so nested fns inside a generic type see both
    /// outer and inner names.
    generics_in_scope: Vec<HashMap<String, GenericOwner>>,
}

impl<'a> Cx<'a> {
    fn primitive(&mut self, p: Primitive) -> TypeId {
        self.out.types.primitive(p)
    }
    fn any(&mut self) -> TypeId {
        self.out.types.any()
    }
    fn null(&mut self) -> TypeId {
        self.out.types.null()
    }
    fn record(&mut self, expr: Idx<Expr>, ty: TypeId) {
        self.out.expr_types.insert(expr, ty);
    }
    fn diag(&mut self, severity: Severity, message: impl Into<String>, range: Range<usize>) {
        self.out.diagnostics.push(SemanticDiagnostic {
            severity,
            message: message.into(),
            byte_range: range,
        });
    }
    fn ident_text(&self, idx: Idx<Ident>) -> &str {
        &self.hir.idents[idx].text
    }

    fn push_narrow(&mut self) {
        self.narrows.push(HashMap::new());
    }
    fn pop_narrow(&mut self) {
        self.narrows.pop();
    }
    fn write_narrow(&mut self, name: Idx<Ident>, ty: TypeId) {
        if let Some(top) = self.narrows.last_mut() {
            top.insert(name, ty);
        }
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
    fn strip_nullable(&mut self, ty: TypeId) -> TypeId {
        let mut t = self.out.types.get(ty).clone();
        if !t.nullable {
            return ty;
        }
        t.nullable = false;
        self.out.types.alloc(t)
    }

    /// P6.3 member resolution: bind the property ident in `a.b` /
    /// `a->b` to the matching `TypeAttr` or method `Decl` whenever the
    /// receiver's type names a `TypeDecl` declared in this module.
    /// Anonymous types and primitives stay no-binding; cross-module
    /// receivers (P11.5) are recorded into `deferred_member_uses` so
    /// the project pipeline can resolve them in a post-pass.
    fn resolve_member(&mut self, recv_ty: TypeId, property: Idx<Ident>) {
        let ty = self.out.types.get(recv_ty);
        let type_name = match &ty.kind {
            TypeKind::Named { name } => Some(name.clone()),
            TypeKind::Generic { name, .. } => Some(name.clone()),
            // P16.2 — primitives (`String`, `int`, ...) carry methods
            // declared as `native type String { ... }` in stdlib.
            // Map the primitive back to its name and fall through to
            // the same `type_decls` / `decl_locations` lookup path so
            // `"hello".size()` and friends bind correctly.
            TypeKind::Primitive(p) => Some(p.name().to_string()),
            TypeKind::Anonymous { fields } => {
                // Anonymous types don't have a backing TypeDecl, so we
                // resolve their fields directly from the type shape.
                let prop = self.hir.idents[property].text.clone();
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
        let Some(decl_id) = self.out.type_decls.get(&name).copied() else {
            // P11.5: type isn't declared in this module. Defer to the
            // project pipeline's cross-module post-pass.
            self.out.deferred_member_uses.push((property, name));
            return;
        };
        let Decl::Type(type_decl) = self.hir.decls[decl_id].clone() else {
            return;
        };
        let prop_text = self.ident_text(property).to_string();

        for attr_id in &type_decl.attrs {
            let attr = &self.hir.type_attrs[*attr_id];
            if self.hir.idents[attr.name].text == prop_text {
                self.out
                    .member_uses
                    .insert(property, MemberDef::Attr(*attr_id));
                return;
            }
        }
        for method_id in &type_decl.methods {
            let Decl::Fn(m) = &self.hir.decls[*method_id] else {
                continue;
            };
            if self.hir.idents[m.name].text == prop_text {
                self.out
                    .member_uses
                    .insert(property, MemberDef::Method(*method_id));
                return;
            }
        }
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
                    self.out.types.generic(name.clone(), args)
                } else if let Some(owner) = self.lookup_generic(&name) {
                    // P12.1: name matches a fn / type generic param in
                    // scope — produce a `GenericParam` rather than a
                    // bare `Named`, so call-site inference can record
                    // witnesses for it.
                    self.out.types.generic_param(name.clone(), owner)
                } else if let Some(id) = self.out.registry.lookup(&name) {
                    id
                } else if self.index.has_name(&name) {
                    // P11.5: name is known to the project but not to
                    // this module's registry — i.e. a type declared
                    // elsewhere. Lower to `Named(name)` so receivers
                    // typed against it carry a name `resolve_member`
                    // can defer for the cross-module post-pass.
                    self.out.types.named(name.clone())
                } else {
                    // Unknown type — fall back to Any so downstream rules don't
                    // mass-cascade. Resolver already emitted "unresolved name".
                    self.any()
                }
            }
        };
        if tr.optional {
            base = self.out.types.nullable(base);
        }
        base
    }

    /// P12.1 — call-site generic inference. Returns `Some(return_ty)`
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
        let Expr::Ident(name_idx) = self.hir.exprs[callee].clone() else {
            return None;
        };
        let Definition::Decl(decl_id) = self.res.lookup(name_idx)? else {
            // Cross-module / param-as-fn / lambda callees aren't
            // handled in this pass — they fall back to `any`.
            return None;
        };
        let Decl::Fn(fnd) = self.hir.decls[decl_id].clone() else {
            return None;
        };
        if fnd.generics.is_empty() {
            return None;
        }
        // Lower the declared signature with the fn's generics in scope.
        let owner = GenericOwner::Function(self.hir.idents[fnd.name].text.clone());
        self.push_generic_scope(&fnd.generics, owner);
        let declared_params: Vec<TypeId> = fnd
            .params
            .iter()
            .map(|p_id| {
                self.hir.fn_params[*p_id]
                    .ty
                    .map(|t| self.lower_type_ref(t))
                    .unwrap_or_else(|| self.any())
            })
            .collect();
        let declared_return = fnd
            .return_type
            .map(|t| self.lower_type_ref(t))
            .unwrap_or_else(|| self.any());
        self.pop_generic_scope();

        let mut tbl = InferenceTable::new();
        let pair_count = declared_params.len().min(arg_tys.len());
        for i in 0..pair_count {
            self.collect_witnesses(declared_params[i], arg_tys[i], &mut tbl, &call_range);
        }
        Some(tbl.substitute(&mut self.out.types, declared_return))
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
        let pk = self.out.types.get(param_ty).clone();
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
                        greycat_analyzer_types::display(&self.out.types, prior),
                        greycat_analyzer_types::display(&self.out.types, witness),
                    );
                    self.diag(Severity::Error, msg, call_range.clone());
                }
                return;
            }
            tbl.bind(name.clone(), witness);
            return;
        }
        let ak = self.out.types.get(arg_ty).clone();
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
        let owner = GenericOwner::Function(self.hir.idents[d.name].text.clone());
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
        let owner = GenericOwner::Type(self.hir.idents[d.name].text.clone());
        self.push_generic_scope(&d.generics, owner);
        for attr_id in &d.attrs {
            let a = self.hir.type_attrs[*attr_id].clone();
            self.visit_type_attr(&a);
        }
        for method_id in &d.methods {
            if let Decl::Fn(fnd) = self.hir.decls[*method_id].clone() {
                self.visit_fn_decl(&fnd);
            }
        }
        self.pop_generic_scope();
    }

    fn push_generic_scope(&mut self, generics: &[Idx<Ident>], owner: GenericOwner) {
        let mut frame = HashMap::new();
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
        let declared = a.ty.map(|t| self.lower_type_ref(t));
        if let Some(init) = a.init {
            let init_ty = self.visit_expr(init);
            if let Some(declared) = declared
                && !is_assignable_to(&self.out.types, init_ty, declared)
            {
                let msg = format!(
                    "attribute initializer of type `{}` is not assignable to declared type `{}`",
                    greycat_analyzer_types::display(&self.out.types, init_ty),
                    greycat_analyzer_types::display(&self.out.types, declared),
                );
                self.diag(Severity::Error, msg, a.byte_range.clone());
            }
        }
    }

    fn visit_top_var(&mut self, d: &VarDeclTop) {
        let declared = d.ty.map(|t| self.lower_type_ref(t));
        if let Some(init) = d.init {
            let init_ty = self.visit_expr(init);
            if let Some(declared) = declared
                && !is_assignable_to(&self.out.types, init_ty, declared)
            {
                let msg = format!(
                    "initializer of type `{}` is not assignable to declared type `{}`",
                    greycat_analyzer_types::display(&self.out.types, init_ty),
                    greycat_analyzer_types::display(&self.out.types, declared),
                );
                self.diag(Severity::Error, msg, d.byte_range.clone());
            }
        }
    }

    fn visit_pragma(&mut self, p: &Pragma) {
        for a in &p.args {
            let _ = self.visit_expr(*a);
        }
    }

    fn visit_stmt(&mut self, stmt_id: Idx<Stmt>, return_ty: Option<TypeId>) {
        let stmt = self.hir.stmts[stmt_id].clone();
        match stmt {
            Stmt::Block(stmts) => {
                self.push_narrow();
                for s in stmts {
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
                if let (Some(declared), Some(init_ty)) = (declared, init_ty)
                    && !is_assignable_to(&self.out.types, init_ty, declared)
                {
                    let msg = format!(
                        "var initializer of type `{}` is not assignable to declared type `{}`",
                        greycat_analyzer_types::display(&self.out.types, init_ty),
                        greycat_analyzer_types::display(&self.out.types, declared),
                    );
                    let r = self.hir.exprs[init.unwrap()].byte_range();
                    self.diag(Severity::Error, msg, r);
                }
                let var_ty = declared.or(init_ty).unwrap_or_else(|| self.any());
                self.out.def_types.insert(name, var_ty);
            }
            Stmt::Assign(AssignStmt {
                target,
                value,
                byte_range,
                ..
            }) => {
                let target_ty = self.visit_expr(target);
                let value_ty = self.visit_expr(value);
                if !is_assignable_to(&self.out.types, value_ty, target_ty) {
                    let msg = format!(
                        "value of type `{}` is not assignable to target of type `{}`",
                        greycat_analyzer_types::display(&self.out.types, value_ty),
                        greycat_analyzer_types::display(&self.out.types, target_ty),
                    );
                    self.diag(Severity::Error, msg, byte_range);
                }
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
                self.visit_stmt(then_branch, return_ty);
                self.pop_narrow();
                let then_terminates = stmt_terminates(self.hir, then_branch);

                let else_terminates = if let Some(eb) = else_branch {
                    self.push_narrow();
                    for ident in &else_non_null {
                        if let Some(cur) = self.lookup_def_type(*ident) {
                            let stripped = self.strip_nullable(cur);
                            self.write_narrow(*ident, stripped);
                        }
                    }
                    self.visit_stmt(eb, return_ty);
                    self.pop_narrow();
                    stmt_terminates(self.hir, eb)
                } else {
                    false
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
                }
            }
            Stmt::While(WhileStmt {
                condition, body, ..
            }) => {
                self.expect_bool(condition, "while condition");
                self.visit_stmt(body, return_ty);
            }
            Stmt::DoWhile(DoWhileStmt {
                condition, body, ..
            }) => {
                self.visit_stmt(body, return_ty);
                self.expect_bool(condition, "do-while condition");
            }
            Stmt::For(ForStmt {
                init_value,
                condition,
                increment,
                body,
                ..
            }) => {
                if let Some(v) = init_value {
                    let _ = self.visit_expr(v);
                }
                if let Some(c) = condition {
                    self.expect_bool(c, "for condition");
                }
                if let Some(i) = increment {
                    let _ = self.visit_expr(i);
                }
                self.visit_stmt(body, return_ty);
            }
            Stmt::ForIn(ForInStmt { range, body, .. }) => {
                let _ = self.visit_expr(range);
                self.visit_stmt(body, return_ty);
            }
            Stmt::Return(value) => {
                if let Some(v) = value {
                    let value_ty = self.visit_expr(v);
                    if let Some(rt) = return_ty
                        && !is_assignable_to(&self.out.types, value_ty, rt)
                    {
                        let msg = format!(
                            "return value of type `{}` is not assignable to declared return type `{}`",
                            greycat_analyzer_types::display(&self.out.types, value_ty),
                            greycat_analyzer_types::display(&self.out.types, rt),
                        );
                        self.diag(Severity::Error, msg, self.hir.exprs[v].byte_range());
                    }
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
                self.visit_stmt(try_block, return_ty);
                self.visit_stmt(catch_block, return_ty);
            }
            Stmt::At(AtStmt { expr, block, .. }) => {
                let _ = self.visit_expr(expr);
                self.visit_stmt(block, return_ty);
            }
        }
    }

    /// Narrowing analyzer for if-conditions.
    ///
    /// Recognizes (P6.4) `x != null` / `x == null` and (P6.5) `x is T`,
    /// plus (P13.2) conjunctive / disjunctive combinations:
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
                    // Else: at least one failed — can't narrow confidently.
                }
                BinOp::Or => {
                    let l = self.derive_cond_narrows(*left);
                    let r = self.derive_cond_narrows(*right);
                    // Else: NOT(A || B) ≡ !A AND !B — union else narrows.
                    out.else_non_null.extend(l.else_non_null);
                    out.else_non_null.extend(r.else_non_null);
                    // Then: at least one held — can't narrow either.
                }
                BinOp::Eq | BinOp::Neq => {
                    let Some(name_idx) = self.ident_compared_to_null(*left, *right) else {
                        return out;
                    };
                    let Some(def) = (match self.res.lookup(name_idx) {
                        Some(Definition::Param(d)) | Some(Definition::Local(d)) => Some(d),
                        _ => None,
                    }) else {
                        return out;
                    };
                    match *op {
                        BinOp::Neq => out.then_non_null.push(def),
                        BinOp::Eq => out.else_non_null.push(def),
                        _ => {}
                    }
                }
                _ => {}
            },
            // P6.5: `x is T` narrows x to T in the then-branch.
            Expr::Is { value, ty, .. } => {
                if let Expr::Ident(name_idx) = &self.hir.exprs[*value]
                    && let Some(Definition::Param(def) | Definition::Local(def)) =
                        self.res.lookup(*name_idx)
                {
                    out.then_typed.push((def, *ty));
                }
            }
            // Strip parens before re-deriving.
            Expr::Paren(inner, _) => return self.derive_cond_narrows(*inner),
            _ => {}
        }
        out
    }

    /// P6.6 exhaustiveness: if `head_id` is the start of an
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
        if chain.has_final_else {
            return;
        }
        let Some(enum_id) = self.out.registry.lookup(&chain.enum_name) else {
            return;
        };
        let enum_ty = self.out.types.get(enum_id);
        let TypeKind::Enum { variants, .. } = &enum_ty.kind else {
            return;
        };
        let variants = variants.clone();
        let covered: HashSet<&str> = chain.arms.iter().map(|a| a.variant.as_str()).collect();
        let missing: Vec<&str> = variants
            .iter()
            .map(String::as_str)
            .filter(|v| !covered.contains(v))
            .collect();
        if missing.is_empty() {
            return;
        }
        let msg = format!(
            "non-exhaustive match over `{}` (missing: {})",
            chain.enum_name,
            missing.join(", "),
        );
        self.diag(Severity::Warning, msg, head_range);
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
        let Expr::Ident(name_idx) = &self.hir.exprs[ident_side] else {
            return None;
        };
        let binding = match self.res.lookup(*name_idx)? {
            Definition::Param(d) | Definition::Local(d) => d,
            _ => return None,
        };
        let Expr::Static(StaticExpr { ty, property, .. }) = &self.hir.exprs[static_side] else {
            return None;
        };
        let enum_name = self.hir.idents[self.hir.type_refs[*ty].name].text.clone();
        let variant = self.hir.idents[*property].text.clone();
        Some((binding, enum_name, variant))
    }

    fn ident_compared_to_null(&self, l: Idx<Expr>, r: Idx<Expr>) -> Option<Idx<Ident>> {
        let le = &self.hir.exprs[l];
        let re = &self.hir.exprs[r];
        if let (
            Expr::Ident(name),
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
            Expr::Ident(name),
        ) = (le, re)
        {
            return Some(*name);
        }
        None
    }

    fn expect_bool(&mut self, expr: Idx<Expr>, label: &str) {
        let ty = self.visit_expr(expr);
        let bool_t = self.primitive(Primitive::Bool);
        if !is_assignable_to(&self.out.types, ty, bool_t) {
            let msg = format!(
                "{label} must be `bool`, got `{}`",
                greycat_analyzer_types::display(&self.out.types, ty),
            );
            let r = self.hir.exprs[expr].byte_range();
            self.diag(Severity::Error, msg, r);
        }
    }

    fn visit_expr(&mut self, expr_id: Idx<Expr>) -> TypeId {
        let ty = self.infer_expr(expr_id);
        self.record(expr_id, ty);
        ty
    }

    fn infer_expr(&mut self, expr_id: Idx<Expr>) -> TypeId {
        let expr = self.hir.exprs[expr_id].clone();
        match expr {
            Expr::Ident(idx) => match self.res.lookup(idx) {
                Some(Definition::Param(def)) | Some(Definition::Local(def)) => {
                    self.lookup_def_type(def).unwrap_or_else(|| self.any())
                }
                Some(Definition::Decl(_))
                | Some(Definition::Generic(_))
                | Some(Definition::ProjectDecl { .. })
                | Some(Definition::Project)
                | None => self.any(),
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
                LiteralKind::This => self.any(),
                LiteralKind::Duration => self.primitive(Primitive::Duration),
                LiteralKind::Time | LiteralKind::Iso8601 => self.primitive(Primitive::Time),
            },
            Expr::String(StringExpr { .. }) => self.primitive(Primitive::String),
            Expr::Tuple(items, _) => {
                let elems: Vec<TypeId> = items.iter().map(|i| self.visit_expr(*i)).collect();
                self.out.types.tuple(elems)
            }
            Expr::Array(items, _) => {
                for i in items.iter() {
                    let _ = self.visit_expr(*i);
                }
                let any = self.any();
                self.out.types.generic("Array", vec![any])
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
                receiver, property, ..
            })
            | Expr::Arrow(MemberExpr {
                receiver, property, ..
            }) => {
                let recv_ty = self.visit_expr(receiver);
                self.resolve_member(recv_ty, property);
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
                if let Some(member) = self.out.member_uses.get(&property).copied() {
                    match member {
                        MemberDef::Attr(attr_id) => {
                            let attr = self.hir.type_attrs[attr_id].clone();
                            if let Some(ty) = attr.ty {
                                return self.lower_type_ref(ty);
                            }
                            return self.any();
                        }
                        MemberDef::Method(_) => {
                            return self.out.types.named("function");
                        }
                    }
                }
                self.any()
            }
            Expr::Static(s) => {
                // P15.6 — `Type::method` resolution. Lower the receiver
                // type so cross-module receivers land as `Named(name)`
                // (via `lower_type_ref`'s `index.has_name(&name)` arm),
                // then run `resolve_member` on the property: in-module
                // hits land in `member_uses`; cross-module hits get
                // deferred to the project pipeline's pass 3 via
                // `deferred_member_uses` (P11.5).
                let recv_ty = self.lower_type_ref(s.ty);
                self.resolve_member(recv_ty, s.property);
                self.any()
            }
            Expr::QualifiedStatic { .. } => {
                // P15.8 — chained `module::Type::name` shapes.
                // Member binding + return-type / static-shape inference
                // happen in `ProjectAnalysis` pass 3.5 because they need
                // cross-module HIR access. Default to `any` here.
                self.any()
            }
            Expr::Offset(OffsetExpr {
                receiver, index, ..
            }) => {
                let _ = self.visit_expr(receiver);
                let _ = self.visit_expr(index);
                self.any()
            }
            Expr::Call(CallExpr { callee, args, .. }) => {
                let callee_ty = self.visit_expr(callee);
                let arg_tys: Vec<TypeId> = args.iter().map(|a| self.visit_expr(*a)).collect();
                // P12.1: if the callee resolves to an in-module fn decl
                // with generics, run constraint-based inference. Cross-
                // module generic inference (callee is `ProjectDecl`)
                // and method-call generic inference are deferred —
                // they need foreign HIR access the analyzer doesn't yet
                // carry.
                let call_range = self.hir.exprs[expr_id].byte_range();
                if let Some(ret) = self.try_generic_call_inference(callee, &arg_tys, call_range) {
                    return ret;
                }
                // P15.10: pairwise arg-type validation runs in
                // `ProjectAnalysis` pass 3.6 (after pass 3.5 settles
                // static-expr call return types) so outer calls whose
                // args contain inner static-expr calls validate
                // against the *post-pass-3.5* arg types. Doing it here
                // would surface false positives for arg shapes whose
                // type isn't known until pass 3.5 fixes them up.
                let _ = callee_ty;
                self.any()
            }
            Expr::Binary(BinaryExpr {
                op, left, right, ..
            }) => {
                let lt = self.visit_expr(left);
                let rt = self.visit_expr(right);
                self.infer_binary(op, lt, rt)
            }
            Expr::Unary(UnaryExpr { op, operand, .. }) => {
                let inner = self.visit_expr(operand);
                match op {
                    UnaryOp::Not => self.primitive(Primitive::Bool),
                    UnaryOp::Neg | UnaryOp::BitNot => inner,
                    UnaryOp::NonNullAssert => {
                        // `x!!` strips nullable from the result and (P6.4)
                        // narrows the operand binding for the rest of the
                        // enclosing block when the operand is an Ident
                        // bound to a Param/Local.
                        let result = self.strip_nullable(inner);
                        if let Expr::Ident(name_idx) = self.hir.exprs[operand].clone()
                            && let Some(Definition::Param(def) | Definition::Local(def)) =
                                self.res.lookup(name_idx)
                        {
                            self.write_narrow(def, result);
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
                self.out.types.lambda(param_tys, body_ty)
            }
            Expr::Is { value, .. } => {
                let _ = self.visit_expr(value);
                self.primitive(Primitive::Bool)
            }
            Expr::Cast { value, ty, .. } => {
                let from_ty = self.visit_expr(value);
                let to_ty = self.lower_type_ref(ty);
                // P12.3: validate the cast against the GreyCat `as`
                // rules (mirrors TS `isCastable`). Surfaces invalid
                // casts as a diagnostic; the resulting expression
                // type is still `to_ty` so downstream inference
                // doesn't cascade.
                if !is_castable(&self.out.types, from_ty, to_ty) {
                    let r = self.hir.exprs[expr_id].byte_range();
                    let msg = format!(
                        "cannot cast `{}` to `{}`",
                        greycat_analyzer_types::display(&self.out.types, from_ty),
                        greycat_analyzer_types::display(&self.out.types, to_ty),
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
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
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
                // T? ?? T -> T (drop nullable)
                let mut ty = self.out.types.get(rt).clone();
                ty.nullable = false;
                self.out.types.alloc(ty)
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

    fn analyze_src(src: &str) -> AnalysisResult {
        let tree = parse(src);
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let res = resolve(&hir);
        analyze(&hir, &res)
    }

    #[test]
    fn clean_function_no_diagnostics() {
        let r = analyze_src("fn add(a: int, b: int): int { return a + b; }\n");
        assert!(r.diagnostics.is_empty(), "unexpected: {:?}", r.diagnostics);
    }

    #[test]
    fn return_type_mismatch_surfaces() {
        let src = "fn bad(): int { return \"hi\"; }\n";
        let r = analyze_src(src);
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable to declared return type")),
            "expected return-type error, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn if_condition_must_be_bool() {
        // GreyCat's `if` requires parentheses (`if (cond) { ... }`).
        let src = "fn f(x: int): int { if (x) { return 1; } else { return 0; } }\n";
        let r = analyze_src(src);
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.message.contains("if condition must be `bool`")),
            "expected condition error, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn unresolved_name_promoted_to_diagnostic() {
        let r = analyze_src("fn f(): int { return missing; }\n");
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
        let r = analyze_src(src);
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
        let r = analyze_src(src);
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
        let r = analyze_src(src);
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
        let r = analyze_src(src);
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
        let r = analyze_src(src);
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
        let analysis = analyze(&hir, &res);

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
        let analysis = analyze(&hir, &res);

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
        let r = analyze_src(src);
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
        let r = analyze_src(src);
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
        let r = analyze_src(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected no nullability error in conjunctive then-branch, got: {:?}",
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
        let r = analyze_src(src);
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
        let r = analyze_src(src);
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
        let r = analyze_src(src);
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
        let r = analyze_src(src);
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
        let r = analyze_src(src);
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
        let r = analyze_src(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("not assignable")),
            "expected `as Foo` to type as Foo, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn non_exhaustive_enum_chain_warns() {
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
        let r = analyze_src(src);
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.message.contains("non-exhaustive match over `Color`")
                    && d.message.contains("Blue")),
            "expected non-exhaustive diag mentioning Blue, got: {:?}",
            r.diagnostics
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
        let r = analyze_src(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("non-exhaustive")),
            "expected no exhaustiveness diag, got: {:?}",
            r.diagnostics
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
        let r = analyze_src(src);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("non-exhaustive")),
            "expected final-else to suppress diag, got: {:?}",
            r.diagnostics
        );
    }

    /// P16.1 — `Expr::Member` resolving to an `Attr` reports the
    /// attr's declared type as the expression type, not `any`. Closes
    /// the project.gcl bug where `var s = x.s.size();` typed `x.s` as
    /// `any` even though `s: String` was bound.
    #[test]
    fn member_attr_typing_matches_attr_decl_type() {
        let src = r#"
type Foo { s: String; }
fn f(x: Foo): String { return x.s; }
"#;
        let r = analyze_src(src);
        assert!(
            r.diagnostics.is_empty(),
            "x.s should type as String matching the return type, got diagnostics: {:?}",
            r.diagnostics
        );
    }

    /// P16.1 — `Expr::Member` resolving to a `Method` reports
    /// `function`-typed (gcl's first-class function type).
    #[test]
    fn member_method_ref_types_as_function() {
        let src = r#"
type Foo { fn run(): int { return 0; } }
fn caller(x: Foo): function { return x.run; }
"#;
        let r = analyze_src(src);
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
        let analysis = analyze(&hir, &res);

        let bogus = hir
            .idents
            .iter()
            .find(|(_, i)| i.text == "bogus")
            .map(|(idx, _)| idx)
            .expect("bogus ident exists");
        assert!(analysis.member_lookup(bogus).is_none());
    }
}
