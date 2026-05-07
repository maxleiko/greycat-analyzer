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
    Primitive, Type, TypeArena, TypeId, TypeKind, TypeRegistry, is_assignable_to,
};

use crate::resolver::{Definition, Resolutions};

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
}

/// Run the analyzer.
pub fn analyze(hir: &Hir, res: &Resolutions) -> AnalysisResult {
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
        narrows: Vec::new(),
        chain_member_ifs: HashSet::new(),
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
    /// Anonymous types, primitives, and cross-module receivers are
    /// out of scope here — `Definition::Project` plus P8.x cross-
    /// module work covers those later.
    fn resolve_member(&mut self, recv_ty: TypeId, property: Idx<Ident>) {
        let ty = self.out.types.get(recv_ty);
        let type_name = match &ty.kind {
            TypeKind::Named { name } => Some(name.clone()),
            TypeKind::Generic { name, .. } => Some(name.clone()),
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
                } else if let Some(id) = self.out.registry.lookup(&name) {
                    id
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
    }

    fn visit_type_decl(&mut self, d: &TypeDecl) {
        for attr_id in &d.attrs {
            let a = self.hir.type_attrs[*attr_id].clone();
            self.visit_type_attr(&a);
        }
        for method_id in &d.methods {
            if let Decl::Fn(fnd) = self.hir.decls[*method_id].clone() {
                self.visit_fn_decl(&fnd);
            }
        }
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

                if let Some(eb) = else_branch {
                    self.push_narrow();
                    for ident in &else_non_null {
                        if let Some(cur) = self.lookup_def_type(*ident) {
                            let stripped = self.strip_nullable(cur);
                            self.write_narrow(*ident, stripped);
                        }
                    }
                    self.visit_stmt(eb, return_ty);
                    self.pop_narrow();
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

    /// P6.4 narrowing analyzer for if-conditions. Recognizes the
    /// surface forms `x != null`, `x == null`, and their reversed
    /// twins (`null != x` / `null == x`). Conjunctive narrowings
    /// (`x != null && y != null`) are intentionally minimal here —
    /// extend in a follow-up if the corpus pushes for it.
    fn derive_cond_narrows(&self, cond_id: Idx<Expr>) -> CondNarrows {
        let mut out = CondNarrows::default();
        match &self.hir.exprs[cond_id] {
            Expr::Binary(BinaryExpr {
                op, left, right, ..
            }) => {
                let op = *op;
                if !matches!(op, BinOp::Eq | BinOp::Neq) {
                    return out;
                }
                let Some(name_idx) = self.ident_compared_to_null(*left, *right) else {
                    return out;
                };
                let Some(def) = (match self.res.lookup(name_idx) {
                    Some(Definition::Param(d)) | Some(Definition::Local(d)) => Some(d),
                    _ => None,
                }) else {
                    return out;
                };
                match op {
                    BinOp::Neq => out.then_non_null.push(def),
                    BinOp::Eq => out.else_non_null.push(def),
                    _ => {}
                }
            }
            // P6.5: `x is T` narrows x to T in the then-branch.
            Expr::Is { value, ty, .. } => {
                if let Expr::Ident(name_idx) = &self.hir.exprs[*value]
                    && let Some(Definition::Param(def) | Definition::Local(def)) =
                        self.res.lookup(*name_idx)
                {
                    out.then_typed.push((def, *ty));
                }
            }
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
                | Some(Definition::Project)
                | None => self.any(),
            },
            Expr::Literal(LiteralExpr { kind, .. }) => match kind {
                LiteralKind::Bool => self.primitive(Primitive::Bool),
                LiteralKind::Number => self.primitive(Primitive::Int),
                LiteralKind::Char => self.primitive(Primitive::Char),
                LiteralKind::Null => self.null(),
                LiteralKind::This => self.any(),
                LiteralKind::Duration => self.primitive(Primitive::Duration),
                LiteralKind::Iso8601 => self.primitive(Primitive::Time),
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
                self.any()
            }
            Expr::Static(s) => {
                let _ = self.lower_type_ref(s.ty);
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
                let _ = self.visit_expr(callee);
                for a in args.iter() {
                    let _ = self.visit_expr(*a);
                }
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
                let _ = self.visit_expr(value);
                self.lower_type_ref(ty)
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
