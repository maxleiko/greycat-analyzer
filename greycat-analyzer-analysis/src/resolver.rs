//! Symbol resolver / name binding (P2.3, extended in P6.2).
//!
//! Walks an [`Hir`] and produces a [`Resolutions`] table that maps each
//! ident-use site to the declaration or local that introduces it. Builds
//! a scope tree on the way so editor features (hover / goto-def / find-
//! references) can ask "what's in scope at this position?".
//!
//! Scope semantics mirror the TS reference (`packages/lang/src/analysis/
//! environment.ts` + `resolver.ts`):
//! - Module scope: top-level decls (fn / type / enum / var).
//! - Function scope: parameters + locally-declared vars + the fn's own
//!   generic params.
//! - Type scope: the type's generic params (visible inside the type's
//!   attributes and methods).
//! - Block scope: nested var declarations, shadowing parent block.
//! - For / for-in / try-catch introduce their own scope for their bound
//!   names.
//! - **Project scope** (P6.2): consulted after every local scope misses
//!   — names registered in the shared [`ProjectIndex`] (runtime types,
//!   primitives by name, and decls from other modules) bind to
//!   [`Definition::Project`].
//!
//! Member-access (`a.b`) is *not* resolved here — the property `b` needs
//! the receiver's type, which is P6.3 territory. Only the head of
//! the chain (`a`) is bound now.

use std::collections::HashMap;

use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{
    AssignStmt, AtStmt, BinaryExpr, CallExpr, Decl, DoWhileStmt, Expr, FnDecl, ForInStmt, ForStmt,
    Ident, IfStmt, LambdaExpr, LiteralExpr, LocalVar, MemberExpr, ObjectExpr, OffsetExpr, Pragma,
    Stmt, StringExpr, TryStmt, TypeAttr, TypeDecl, TypeRef, UnaryExpr, VarDeclTop, WhileStmt,
};

use crate::stdlib::ProjectIndex;

/// Where a use of an `Ident` resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Definition {
    /// A top-level declaration in the same module — `Idx<Decl>` indexes
    /// the HIR decls arena.
    Decl(Idx<Decl>),
    /// A locally-bound name (var, for-in iterator, catch param).
    Local(Idx<Ident>),
    /// A function parameter.
    Param(Idx<Ident>),
    /// A type-parameter declaration (`type Foo<T>` / `fn f<T>(...)`).
    /// Points back at the binding ident so capabilities can offer goto-
    /// definition. Inference / constraint handling is **P7.4**.
    Generic(Idx<Ident>),
    /// A name resolved against the shared [`ProjectIndex`] — either a
    /// runtime-implemented type / native fn, a registered primitive
    /// name, or a top-level decl from another module. The variant
    /// carries no detail today; cross-module decl pointers + member
    /// resolution land in P6.3 / P8.2.
    Project,
}

/// Resolution table — built by [`resolve`].
#[derive(Debug, Default)]
pub struct Resolutions {
    /// For each *use* of an ident (by `Idx<Ident>`), where it resolved.
    /// Idents that are *definitions* (the name in `fn foo()` etc.) are
    /// *not* present here — only use sites.
    pub uses: HashMap<Idx<Ident>, Definition>,
    /// Reverse-reference index (P6.7): how many times each top-level
    /// `Decl` is referenced through a `Definition::Decl` use. Lets the
    /// `unused-decl` lint rule check at-a-glance whether a decl is
    /// never used outside its own declaration.
    pub references_to: HashMap<Idx<Decl>, usize>,
    /// Idents the resolver couldn't bind. Surface as "unresolved name"
    /// diagnostics in P2.5.
    pub unresolved: Vec<Idx<Ident>>,
}

impl Resolutions {
    pub fn lookup(&self, ident: Idx<Ident>) -> Option<Definition> {
        self.uses.get(&ident).copied()
    }
}

#[derive(Default)]
struct Scope {
    /// Lexical name → resolution.
    names: HashMap<String, Definition>,
}

impl Scope {
    fn insert(&mut self, name: String, def: Definition) {
        self.names.insert(name, def);
    }
}

struct Cx<'a> {
    hir: &'a Hir,
    scopes: Vec<Scope>,
    /// Project-level fallback for names that miss every local scope.
    /// Per-file callers pass an empty [`ProjectIndex::new`]; the project
    /// pipeline (P6.1) passes the index it just rebuilt.
    index: &'a ProjectIndex,
    res: Resolutions,
}

impl<'a> Cx<'a> {
    fn new(hir: &'a Hir, index: &'a ProjectIndex) -> Self {
        Self {
            hir,
            scopes: vec![Scope::default()],
            index,
            res: Resolutions::default(),
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn current_mut(&mut self) -> &mut Scope {
        self.scopes
            .last_mut()
            .expect("at least one scope is always live")
    }

    fn lookup_local(&self, name: &str) -> Option<Definition> {
        for scope in self.scopes.iter().rev() {
            if let Some(d) = scope.names.get(name) {
                return Some(*d);
            }
        }
        None
    }

    fn ident_text(&self, idx: Idx<Ident>) -> &str {
        &self.hir.idents[idx].text
    }

    fn record_use(&mut self, idx: Idx<Ident>) {
        let name = self.ident_text(idx).to_string();
        if let Some(def) = self.lookup_local(&name) {
            self.res.uses.insert(idx, def);
            // P6.7: bump the reverse-reference count for top-level decls.
            if let Definition::Decl(decl_id) = def {
                *self.res.references_to.entry(decl_id).or_insert(0) += 1;
            }
            return;
        }
        if self.index.has_name(&name) {
            self.res.uses.insert(idx, Definition::Project);
            return;
        }
        self.res.unresolved.push(idx);
    }
}

/// Run name resolution against `hir` with no cross-module context — the
/// fallback index is just [`ProjectIndex::new`], which knows the
/// language primitives and runtime-implemented type names but no
/// user-declared decls. Per-file callers (tests, per-request
/// capabilities) use this; the project pipeline uses
/// [`resolve_with_index`] so cross-module names also resolve.
pub fn resolve(hir: &Hir) -> Resolutions {
    let index = ProjectIndex::new();
    resolve_inner(hir, &index)
}

/// Run name resolution against `hir`, falling back to `index` for names
/// that aren't satisfied by any local scope. P6.2 entry point used by
/// the project pipeline.
pub fn resolve_with_index(hir: &Hir, index: &ProjectIndex) -> Resolutions {
    resolve_inner(hir, index)
}

fn resolve_inner(hir: &Hir, index: &ProjectIndex) -> Resolutions {
    let mut cx = Cx::new(hir, index);

    let Some(module) = hir.module.as_ref() else {
        return cx.res;
    };

    // Two-pass at module scope so forward references between top-level
    // decls work (TS reference does the same).
    for decl_id in &module.decls {
        seed_module_decl(&mut cx, *decl_id);
    }
    for decl_id in &module.decls {
        visit_decl(&mut cx, *decl_id);
    }

    cx.res
}

fn seed_module_decl(cx: &mut Cx, decl_id: Idx<Decl>) {
    let decl = &cx.hir.decls[decl_id];
    let Some(name_id) = decl.name() else {
        return;
    };
    let name = cx.ident_text(name_id).to_string();
    cx.current_mut().insert(name, Definition::Decl(decl_id));
}

fn visit_decl(cx: &mut Cx, decl_id: Idx<Decl>) {
    let decl = cx.hir.decls[decl_id].clone();
    match decl {
        Decl::Fn(d) => visit_fn_decl(cx, &d),
        Decl::Type(d) => visit_type_decl(cx, &d),
        Decl::Enum(_) => {
            // Enum declarations have no expressions to resolve at the
            // declaration site — field initializers (if present in
            // future) would visit here.
        }
        Decl::Var(d) => visit_top_var(cx, &d),
        Decl::Pragma(p) => visit_pragma(cx, &p),
    }
}

fn visit_fn_decl(cx: &mut Cx, d: &FnDecl) {
    cx.push_scope();
    // Generic params first so type-refs in param / return position can
    // see them.
    for g in &d.generics {
        let name = cx.ident_text(*g).to_string();
        cx.current_mut().insert(name, Definition::Generic(*g));
    }
    // Parameters become Param bindings in the function scope.
    for param_id in &d.params {
        let p = cx.hir.fn_params[*param_id].clone();
        let name = cx.ident_text(p.name).to_string();
        cx.current_mut().insert(name, Definition::Param(p.name));
        if let Some(ty) = p.ty {
            visit_type_ref(cx, ty);
        }
    }
    if let Some(rt) = d.return_type {
        visit_type_ref(cx, rt);
    }
    if let Some(body) = d.body {
        visit_stmt(cx, body);
    }
    cx.pop_scope();
}

fn visit_type_decl(cx: &mut Cx, d: &TypeDecl) {
    cx.push_scope();
    // Generic params visible inside attribute types and method bodies.
    for g in &d.generics {
        let name = cx.ident_text(*g).to_string();
        cx.current_mut().insert(name, Definition::Generic(*g));
    }
    if let Some(sup) = d.supertype {
        visit_type_ref(cx, sup);
    }
    for attr_id in &d.attrs {
        let a = cx.hir.type_attrs[*attr_id].clone();
        visit_type_attr(cx, &a);
    }
    for method_id in &d.methods {
        // Methods see the type's own attrs as `this.<attr>`. We don't
        // pre-register attrs as locals because they're accessed through
        // member-expressions (and member resolution is type-driven, P2.5).
        if let Decl::Fn(fnd) = cx.hir.decls[*method_id].clone() {
            visit_fn_decl(cx, &fnd);
        }
    }
    cx.pop_scope();
}

fn visit_type_attr(cx: &mut Cx, a: &TypeAttr) {
    if let Some(ty) = a.ty {
        visit_type_ref(cx, ty);
    }
    if let Some(init) = a.init {
        visit_expr(cx, init);
    }
}

fn visit_top_var(cx: &mut Cx, d: &VarDeclTop) {
    if let Some(ty) = d.ty {
        visit_type_ref(cx, ty);
    }
    if let Some(init) = d.init {
        visit_expr(cx, init);
    }
}

fn visit_pragma(cx: &mut Cx, p: &Pragma) {
    for arg in &p.args {
        visit_expr(cx, *arg);
    }
}

fn visit_stmt(cx: &mut Cx, stmt_id: Idx<Stmt>) {
    let stmt = cx.hir.stmts[stmt_id].clone();
    match stmt {
        Stmt::Block(stmts) => {
            cx.push_scope();
            for s in stmts {
                visit_stmt(cx, s);
            }
            cx.pop_scope();
        }
        Stmt::Expr(e) => visit_expr(cx, e),
        Stmt::Var(LocalVar { name, ty, init, .. }) => {
            if let Some(ty) = ty {
                visit_type_ref(cx, ty);
            }
            if let Some(init) = init {
                visit_expr(cx, init);
            }
            let n = cx.ident_text(name).to_string();
            cx.current_mut().insert(n, Definition::Local(name));
        }
        Stmt::Assign(AssignStmt { target, value, .. }) => {
            visit_expr(cx, target);
            visit_expr(cx, value);
        }
        Stmt::If(IfStmt {
            condition,
            then_branch,
            else_branch,
            ..
        }) => {
            visit_expr(cx, condition);
            visit_stmt(cx, then_branch);
            if let Some(eb) = else_branch {
                visit_stmt(cx, eb);
            }
        }
        Stmt::While(WhileStmt {
            condition, body, ..
        }) => {
            visit_expr(cx, condition);
            visit_stmt(cx, body);
        }
        Stmt::DoWhile(DoWhileStmt {
            body, condition, ..
        }) => {
            visit_stmt(cx, body);
            visit_expr(cx, condition);
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
            cx.push_scope();
            if let Some(t) = init_ty {
                visit_type_ref(cx, t);
            }
            if let Some(v) = init_value {
                visit_expr(cx, v);
            }
            if let Some(name) = init_name {
                let n = cx.ident_text(name).to_string();
                cx.current_mut().insert(n, Definition::Local(name));
            }
            if let Some(c) = condition {
                visit_expr(cx, c);
            }
            if let Some(i) = increment {
                visit_expr(cx, i);
            }
            visit_stmt(cx, body);
            cx.pop_scope();
        }
        Stmt::ForIn(ForInStmt {
            iterator_name,
            iterator_type,
            range,
            body,
            ..
        }) => {
            visit_expr(cx, range);
            cx.push_scope();
            if let Some(t) = iterator_type {
                visit_type_ref(cx, t);
            }
            let n = cx.ident_text(iterator_name).to_string();
            cx.current_mut().insert(n, Definition::Local(iterator_name));
            visit_stmt(cx, body);
            cx.pop_scope();
        }
        Stmt::Return(value) => {
            if let Some(v) = value {
                visit_expr(cx, v);
            }
        }
        Stmt::Break | Stmt::Continue => {}
        Stmt::Throw(e) => visit_expr(cx, e),
        Stmt::Try(TryStmt {
            try_block,
            error_param,
            catch_block,
            ..
        }) => {
            visit_stmt(cx, try_block);
            cx.push_scope();
            if let Some(name) = error_param {
                let n = cx.ident_text(name).to_string();
                cx.current_mut().insert(n, Definition::Local(name));
            }
            visit_stmt(cx, catch_block);
            cx.pop_scope();
        }
        Stmt::At(AtStmt { expr, block, .. }) => {
            visit_expr(cx, expr);
            visit_stmt(cx, block);
        }
    }
}

fn visit_expr(cx: &mut Cx, expr_id: Idx<Expr>) {
    let expr = cx.hir.exprs[expr_id].clone();
    match expr {
        Expr::Ident(idx) => cx.record_use(idx),
        Expr::Literal(_) | Expr::String(StringExpr { .. }) => {}
        Expr::Tuple(items, _) | Expr::Array(items, _) => {
            for e in items {
                visit_expr(cx, e);
            }
        }
        Expr::Object(ObjectExpr { ty, fields, .. }) => {
            if let Some(t) = ty {
                visit_type_ref(cx, t);
            }
            for f in fields {
                visit_expr(cx, f.value);
            }
        }
        Expr::Member(MemberExpr { receiver, .. }) | Expr::Arrow(MemberExpr { receiver, .. }) => {
            visit_expr(cx, receiver);
            // The `property` ident is intentionally *not* resolved here —
            // member access binds to a type member, which is type-driven
            // (P2.5).
        }
        Expr::Static(s) => visit_type_ref(cx, s.ty),
        Expr::Offset(OffsetExpr {
            receiver, index, ..
        }) => {
            visit_expr(cx, receiver);
            visit_expr(cx, index);
        }
        Expr::Call(CallExpr { callee, args, .. }) => {
            visit_expr(cx, callee);
            for a in args {
                visit_expr(cx, a);
            }
        }
        Expr::Binary(BinaryExpr { left, right, .. }) => {
            visit_expr(cx, left);
            visit_expr(cx, right);
        }
        Expr::Unary(UnaryExpr { operand, .. }) => visit_expr(cx, operand),
        Expr::Paren(inner, _) => visit_expr(cx, inner),
        Expr::Lambda(LambdaExpr { params, body, .. }) => {
            cx.push_scope();
            for param_id in params {
                let p = cx.hir.fn_params[param_id].clone();
                let name = cx.ident_text(p.name).to_string();
                cx.current_mut().insert(name, Definition::Param(p.name));
                if let Some(t) = p.ty {
                    visit_type_ref(cx, t);
                }
            }
            visit_expr(cx, body);
            cx.pop_scope();
        }
        Expr::Is { value, ty, .. } | Expr::Cast { value, ty, .. } => {
            visit_expr(cx, value);
            visit_type_ref(cx, ty);
        }
        Expr::Unsupported { .. } => {
            // Lowering hasn't expanded this shape yet; nothing to bind.
        }
    }
    // Suppress unused-import-of-LiteralExpr warning if never used.
    let _ = LiteralExpr {
        kind: greycat_analyzer_hir::types::LiteralKind::Null,
        text: String::new(),
        byte_range: 0..0,
    };
}

fn visit_type_ref(cx: &mut Cx, ty_id: Idx<TypeRef>) {
    let ty = cx.hir.type_refs[ty_id].clone();
    cx.record_use(ty.name);
    for p in ty.params {
        visit_type_ref(cx, p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_hir::lower_module;
    use greycat_analyzer_hir::types::{Decl, Expr};
    use greycat_analyzer_syntax::parse;

    fn analyze(src: &str) -> (Hir, Resolutions) {
        let tree = parse(src);
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let res = resolve(&hir);
        (hir, res)
    }

    #[test]
    fn param_use_resolves_to_param() {
        let src = "fn id(x: int): int { return x; }\n";
        let (hir, res) = analyze(src);

        // Find the use of `x` inside the body.
        let x_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.text == "x")
            .map(|(idx, _)| idx)
            .collect();
        // Two `x` idents: one is the parameter name (definition),
        // one is the use inside `return x`.
        let resolved: Vec<_> = x_uses.iter().filter_map(|idx| res.uses.get(idx)).collect();
        assert_eq!(resolved.len(), 1, "exactly one *use* of `x`");
        assert!(matches!(resolved[0], Definition::Param(_)));
        assert!(res.unresolved.is_empty());
    }

    #[test]
    fn forward_reference_at_module_scope() {
        let src = r#"
fn caller(): int { return helper(); }
fn helper(): int { return 1; }
"#;
        let (hir, res) = analyze(src);
        // The Ident for the use of `helper` in caller's body.
        let helper_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.text == "helper")
            .map(|(idx, _)| idx)
            .collect();
        let bound: Vec<_> = helper_uses
            .iter()
            .filter_map(|idx| res.uses.get(idx))
            .collect();
        assert_eq!(bound.len(), 1);
        assert!(matches!(bound[0], Definition::Decl(_)));
        assert!(res.unresolved.is_empty());
    }

    #[test]
    fn unresolved_name_reported() {
        let src = "fn f(): int { return missing; }\n";
        let (_hir, res) = analyze(src);
        assert_eq!(res.unresolved.len(), 1);
    }

    #[test]
    fn local_var_shadows_outer_binding() {
        let src = r#"
fn f(x: int): int {
    var x: int = 99;
    return x;
}
"#;
        let (hir, res) = analyze(src);
        let return_x_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.text == "x")
            .filter_map(|(idx, _)| res.uses.get(&idx))
            .collect();
        // Use site (return x) — we expect it to bind to the local, not the param.
        assert!(
            return_x_uses
                .iter()
                .any(|d| matches!(d, Definition::Local(_))),
            "expected a local binding for shadowed x: {return_x_uses:?}",
        );
    }

    #[test]
    fn type_ref_head_resolves_to_type_decl() {
        let src = r#"
type Foo {}
fn f(p: Foo): Foo { return p; }
"#;
        let (hir, res) = analyze(src);
        let foo_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.text == "Foo")
            .filter_map(|(idx, _)| res.uses.get(&idx))
            .collect();
        // Two uses of `Foo`: in param type and return type. Both should
        // resolve to the type decl.
        assert_eq!(foo_uses.len(), 2);
        for d in foo_uses {
            assert!(matches!(d, Definition::Decl(_)));
        }
        assert!(res.unresolved.is_empty());
        // Sanity: the resolved decl is in fact the Foo type_decl.
        if let Some(Definition::Decl(decl_id)) =
            res.uses.values().find(|d| matches!(d, Definition::Decl(_)))
        {
            assert!(matches!(hir.decls[*decl_id], Decl::Type(_)));
        }
        // Also: the function body's `return p` should resolve to a Param.
        let p_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.text == "p")
            .filter_map(|(idx, _)| res.uses.get(&idx))
            .collect();
        assert!(p_uses.iter().any(|d| matches!(d, Definition::Param(_))));
        let _ = Expr::Unsupported {
            kind: "",
            byte_range: 0..0,
        };
    }

    #[test]
    fn generic_param_resolves_to_generic_definition() {
        let src = "fn id<T>(x: T): T { return x; }\n";
        let (hir, res) = analyze(src);
        let t_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.text == "T")
            .filter_map(|(idx, _)| res.uses.get(&idx))
            .collect();
        // Two uses of `T` (param type, return type) — both bind to the
        // generic decl ident. The declaring `T` itself is a definition,
        // not a use, so it's not in res.uses.
        assert_eq!(t_uses.len(), 2);
        for d in t_uses {
            assert!(matches!(d, Definition::Generic(_)));
        }
        assert!(res.unresolved.is_empty());
    }

    #[test]
    fn project_index_fallback_resolves_cross_module_name() {
        use crate::stdlib::ProjectIndex;
        // Module A declares `Helper` as a top-level type. Module B
        // refers to `Helper` — without a ProjectIndex it'd be
        // unresolved; with one ingested from A it binds to Project.
        let other_src = "type Helper {}\n";
        let other_tree = parse(other_src);
        let other_hir = lower_module(other_src, "a", "p", other_tree.root_node());

        let mut idx = ProjectIndex::new();
        idx.ingest(&other_hir);

        let user_src = "fn use_helper(h: Helper) {}\n";
        let user_tree = parse(user_src);
        let user_hir = lower_module(user_src, "b", "p", user_tree.root_node());
        let res = resolve_with_index(&user_hir, &idx);

        let helper_uses: Vec<_> = user_hir
            .idents
            .iter()
            .filter(|(_, i)| i.text == "Helper")
            .filter_map(|(idx, _)| res.uses.get(&idx))
            .collect();
        assert_eq!(helper_uses.len(), 1);
        assert!(matches!(helper_uses[0], Definition::Project));
        assert!(res.unresolved.is_empty());
    }
}
