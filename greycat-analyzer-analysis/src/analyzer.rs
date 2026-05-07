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

use std::collections::HashMap;
use std::ops::Range;

use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{
    AssignStmt, AtStmt, BinOp, BinaryExpr, CallExpr, Decl, DoWhileStmt, Expr, FnDecl, ForInStmt,
    ForStmt, Ident, IfStmt, LambdaExpr, LiteralExpr, LiteralKind, LocalVar, MemberExpr, ObjectExpr,
    OffsetExpr, Pragma, Stmt, StringExpr, TryStmt, TypeAttr, TypeDecl, TypeRef, UnaryExpr, UnaryOp,
    VarDeclTop, WhileStmt,
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
    pub diagnostics: Vec<SemanticDiagnostic>,
}

impl AnalysisResult {
    pub fn type_of(&self, expr: Idx<Expr>) -> Option<TypeId> {
        self.expr_types.get(&expr).copied()
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
                out.registry.register(name, id);
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
                out.registry.register(name, id);
            }
            _ => {}
        }
    }
}

struct Cx<'a> {
    hir: &'a Hir,
    res: &'a Resolutions,
    out: &'a mut AnalysisResult,
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
                for s in stmts {
                    self.visit_stmt(s, return_ty);
                }
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
                ..
            }) => {
                self.expect_bool(condition, "if condition");
                self.visit_stmt(then_branch, return_ty);
                if let Some(eb) = else_branch {
                    self.visit_stmt(eb, return_ty);
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
                Some(Definition::Param(def)) | Some(Definition::Local(def)) => self
                    .out
                    .def_types
                    .get(&def)
                    .copied()
                    .unwrap_or_else(|| self.any()),
                Some(Definition::Decl(_)) | Some(Definition::Builtin(_)) | None => self.any(),
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
            Expr::Member(MemberExpr { receiver, .. })
            | Expr::Arrow(MemberExpr { receiver, .. }) => {
                let _ = self.visit_expr(receiver);
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
                        // `x!!` — strip nullable.
                        let mut ty = self.out.types.get(inner).clone();
                        ty.nullable = false;
                        self.out.types.alloc(ty)
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
}
