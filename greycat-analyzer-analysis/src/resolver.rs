// P2.3 — initial drop. P6.2 — project-scope extension. P6.3 — member resolution lives elsewhere.
//! Symbol resolver / name binding.
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
//! - **Project scope**: consulted after every local scope
//!   misses. Names that match a top-level decl from another module
//!   (looked up through [`ProjectIndex::locate_decl`]) bind to the
//!   detailed [`Definition::ProjectDecl`] carrying the foreign module's
//!   `Uri` + `Idx<Decl>`. Names that the project knows but that have no
//!   `.gcl` decl (runtime-implemented types like `Array` / `Map`, native
//!   fn signatures, primitives by name) fall back to the unit
//!   [`Definition::Project`].
//!
//! Member-access (`a.b`) is *not* resolved here — the property `b` needs
//! the receiver's type, which lives in the analyzer. Only the head of
//! the chain (`a`) is bound now.

use rustc_hash::FxHashMap;

use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_core::{SymbolTable, TypeArena};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{
    AssignStmt, AtStmt, BinaryExpr, CallExpr, Decl, DoWhileStmt, Expr, FnDecl, ForInStmt, ForStmt,
    Ident, IfStmt, LambdaExpr, LocalVar, MemberExpr, ObjectExpr, OffsetExpr, Pragma, Stmt,
    StringExpr, TryStmt, TypeAttr, TypeDecl, TypeRef, UnaryExpr, VarDeclTop, WhileStmt,
};

use crate::stdlib::ProjectIndex;

/// Where a use of an `Ident` resolves to.
///
/// Not `Copy` — `ProjectDecl` carries an `Uri` which isn't `Copy`. Clone
/// at use sites where you need owned values.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Definition {
    /// A top-level declaration in the same module — `Idx<Decl>` indexes
    /// the HIR decls arena.
    Decl(Idx<Decl>),
    /// A locally-bound name (var, for-in iterator, catch param).
    Local(Idx<Ident>),
    /// A function parameter.
    Param(Idx<Ident>),
    // P7.4 — inference / constraint handling.
    /// A type-parameter declaration (`type Foo<T>` / `fn f<T>(...)`).
    /// Points back at the binding ident so capabilities can offer goto-
    /// definition.
    Generic(Idx<Ident>),
    // P11.2
    /// A name resolved through the shared [`ProjectIndex`] to a
    /// concrete top-level decl in another module. `uri` /
    /// `decl` together let cross-module capabilities (goto-def,
    /// references, rename, member access) skip text-equality fallbacks.
    /// When [`ProjectIndex::locate_decl`] returns multiple hits the
    /// resolver picks the first; lib/include-aware disambiguation
    /// rides on later phases.
    ProjectDecl { uri: Uri, decl: Idx<Decl> },
    /// A name the project knows but that has no `.gcl` decl in
    /// `decl_locations`: language primitives by name, native fn
    /// signatures, runtime globals (`Infinity`, `NaN`). Runtime
    /// types like `Array` / `Map` / `node` are no longer in this
    /// bucket — they have `native type` decls in `lib/std/core.gcl`
    /// and resolve through `ProjectDecl` once stdlib is loaded.
    Project,
}

/// Resolution table — built by [`resolve`].
#[derive(Debug, Default)]
pub struct Resolutions {
    /// For each *use* of an ident (by `Idx<Ident>`), where it resolved.
    /// Idents that are *definitions* (the name in `fn foo()` etc.) are
    /// *not* present here — only use sites.
    pub uses: FxHashMap<Idx<Ident>, Definition>,
    // P6.7
    /// Reverse-reference index: how many times each top-level
    /// `Decl` is referenced through a `Definition::Decl` use. Lets the
    /// `unused-decl` lint rule check at-a-glance whether a decl is
    /// never used outside its own declaration.
    pub references_to: FxHashMap<Idx<Decl>, usize>,
    // P2.5 — surface as "unresolved name" diagnostics.
    /// Idents the resolver couldn't bind.
    pub unresolved: Vec<Idx<Ident>>,
    // P38.4
    /// Idents that matched a name exported publicly by ≥2 distinct
    /// modules (with no local hit to resolve them). The runtime
    /// reports plain "unresolved function: <name>" on this shape; we
    /// surface a more helpful `ambiguous-symbol` Severity::Error
    /// diagnostic naming each candidate, with quick-fixes that
    /// rewrite the bare ident to one of `<module>::<name>`.
    pub ambiguous: FxHashMap<Idx<Ident>, Vec<(Uri, Idx<Decl>)>>,
}

impl Resolutions {
    pub fn lookup(&self, ident: Idx<Ident>) -> Option<Definition> {
        self.uses.get(&ident).cloned()
    }
}

#[derive(Default)]
struct Scope {
    /// Lexical name → resolution.
    names: FxHashMap<String, Definition>,
}

impl Scope {
    fn insert(&mut self, name: String, def: Definition) {
        self.names.insert(name, def);
    }
}

struct Cx<'a> {
    hir: &'a Hir,
    // P38.3 — module-scope bindings split by visibility. `module_public`
    // participates in normal bare-name lookup (first-tier, alongside
    // nested scopes); `module_private` is a LAST-RESORT fallback,
    // consulted only after the global-public lookup misses. See the
    // commentary in `record_use` for the runtime-conformance rationale.
    /// Module-level decls without a `private` modifier.
    module_public: FxHashMap<String, Definition>,
    /// Module-level decls with a `private` modifier.
    module_private: FxHashMap<String, Definition>,
    /// Nested lexical scopes (fn / type / block / loop / try / catch).
    /// The module-level scope is *not* held here — see
    /// `module_public` / `module_private` above.
    scopes: Vec<Scope>,
    // P6.1 — project pipeline passes the rebuilt index.
    /// Project-level fallback for names that miss every local scope.
    /// Per-file callers pass an empty [`ProjectIndex::new`]; the project
    /// pipeline passes the index it just rebuilt.
    index: &'a ProjectIndex,
    // P38.3 — current module's URI, when known. The project pipeline
    // passes the module's URI through; per-file callers (tests, lint
    // pipeline without project context) pass `None`. Lets the global-
    // public lookup filter out the current module's own entries from
    // `ProjectIndex::locate_decl` so a same-module private decl
    // doesn't accidentally win at step 2 (it should reach the
    // last-resort step 3 instead).
    current_uri: Option<&'a Uri>,
    res: Resolutions,
}

impl<'a> Cx<'a> {
    fn new(hir: &'a Hir, index: &'a ProjectIndex, current_uri: Option<&'a Uri>) -> Self {
        Self {
            hir,
            module_public: FxHashMap::default(),
            module_private: FxHashMap::default(),
            scopes: Vec::new(),
            index,
            current_uri,
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
            .expect("at least one nested scope is live (push_scope must precede insert)")
    }

    // P38.3 — first-tier lookup: nested scopes (params / locals /
    // generics / for-in bindings / catch params) innermost-first,
    // falling through to module-level *public* decls. Module-level
    // private decls are NOT consulted here — they live in
    // `module_private` and are reached only via the last-resort
    // fallback in `record_use`.
    fn lookup_nested_or_public(&self, name: &str) -> Option<Definition> {
        for scope in self.scopes.iter().rev() {
            if let Some(d) = scope.names.get(name) {
                return Some(d.clone());
            }
        }
        self.module_public.get(name).cloned()
    }

    fn ident_text(&self, idx: Idx<Ident>) -> &str {
        &self.index.symbols[self.hir.idents[idx].symbol]
    }

    fn record_use(&mut self, idx: Idx<Ident>) {
        let name = self.ident_text(idx).to_string();
        // P38.3 — Bare-name resolution order matches the GreyCat
        // runtime oracle (8.0.291-dev), validated against
        // `greycat build` on a multi-module project. The order is:
        //
        //   1. Non-module scopes + module-level PUBLIC decls.
        //   2. Global PUBLIC across the project closure
        //      (`ProjectIndex::locate_decl`). Multiple hits collapse to
        //      *unresolved* — 38.4 emits the helpful diagnostic naming
        //      each candidate; the runtime reports plain "unresolved
        //      function: <name>" and we surface the modules with FQN
        //      quick-fixes, but the severity stays Error.
        //   3. Module-level PRIVATE decls — LAST-RESORT FALLBACK.
        //   4. `Project` placeholder (runtime-implemented types,
        //      primitives by name, native fns), known module names
        //      (left segment of a `module::Decl` chain), or unresolved.
        //
        // We intentionally match the runtime even though step 3 is
        // design-questionable: a local PUBLIC same-named decl WILL
        // shadow a remote public (step 1 wins), but a local PRIVATE
        // same-named decl will NOT shadow it (step 2 fires before
        // step 3). The asymmetry would surprise most language
        // designers — most expect "module-local first, regardless of
        // visibility." We doubted this choice loudly before
        // implementing it; the runtime is the oracle and we conform.
        // If the runtime ever flips the order to "local-private
        // shadows remote-public," this is the single place to swap
        // steps 2 and 3.
        if let Some(def) = self.lookup_nested_or_public(&name) {
            // P6.7: bump the reverse-reference count for top-level decls.
            if let Definition::Decl(decl_id) = &def {
                *self.res.references_to.entry(*decl_id).or_insert(0) += 1;
            }
            self.res.uses.insert(idx, def);
            return;
        }
        // P11.2 + P38.3 + P38.4 — global PUBLIC lookup across
        // distinct modules. Filters in order:
        //   - exclude entries from the current module (so a same-
        //     module decl reaches the local-private fallback below
        //     rather than self-referring via `ProjectDecl`);
        //   - exclude entries flagged `private` by the index (private
        //     decls don't participate in bare cross-module lookup —
        //     the FQN form is the runtime-allowed escape hatch).
        //
        // Outcomes:
        //   - exactly 1 hit          → `ProjectDecl { uri, decl }`.
        //   - ≥2 hits across modules → record in `ambiguous` for
        //                              `ambiguous-symbol` Severity::Error
        //                              emission; bare ident does NOT
        //                              fall through to local-private
        //                              (the runtime treats the clash
        //                              as unresolved at this layer).
        //   - 0 hits                 → continue to the last-resort
        //                              local-private step below.
        let cross_module_hits: Vec<(Uri, Idx<Decl>)> = self
            .index
            .locate_decl(&name)
            .iter()
            .filter(|(uri, decl)| {
                let from_other_module = self.current_uri.map(|cur| uri != cur).unwrap_or(true);
                from_other_module && !self.index.is_decl_private(uri, *decl)
            })
            .map(|(uri, decl)| (uri.clone(), *decl))
            .collect();
        match cross_module_hits.len() {
            0 => {}
            1 => {
                let (uri, decl) = cross_module_hits.into_iter().next().unwrap();
                self.res
                    .uses
                    .insert(idx, Definition::ProjectDecl { uri, decl });
                return;
            }
            _ => {
                // ≥2 distinct-module public hits. Record the
                // candidates so the project pipeline can emit
                // `ambiguous-symbol` with FQN quick-fixes; mark the
                // ident as unresolved so downstream typing doesn't
                // arbitrarily pick one of the candidates and mask
                // the error.
                self.res.ambiguous.insert(idx, cross_module_hits);
                self.res.unresolved.push(idx);
                return;
            }
        }
        // P38.3 — module-private last-resort fallback. Reached only
        // when both step 1 (nested + module-public) and step 2 (global
        // public) missed. A local `private` decl shadows nothing
        // outside its module; inside its module it's reachable only
        // when no public match exists anywhere.
        if let Some(def) = self.module_private.get(&name).cloned() {
            if let Definition::Decl(decl_id) = &def {
                *self.res.references_to.entry(*decl_id).or_insert(0) += 1;
            }
            self.res.uses.insert(idx, def);
            return;
        }
        if self.index.has_name(&name) {
            self.res.uses.insert(idx, Definition::Project);
            return;
        }
        // P15.x — known module name (the leftmost segment of a
        // `module::Decl` chain). Bind to `Project` so it's not
        // flagged unresolved; goto-def hits via `goto_module_segment`
        // (P15.9), inference via pass 3.5.
        if self.index.has_module(&name) {
            self.res.uses.insert(idx, Definition::Project);
            return;
        }
        self.res.unresolved.push(idx);
    }

    /// Bind the leaf ident of a qualified `TypeRef` (`b::Foo`,
    /// `a::b::Foo`, …) to the foreign decl named by its rightmost
    /// qualifier segment. Skips the bare-name resolution ladder
    /// entirely — qualified leaves never participate in the
    /// `ambiguous-symbol` collapse, since the user has already
    /// disambiguated by writing the qualifier.
    ///
    /// Outcomes:
    /// - module exists AND exports leaf → `ProjectDecl { uri, decl }`.
    /// - module exists, leaf not in module → leaf marked unresolved
    ///   (the regular "unresolved name" diagnostic surfaces).
    /// - module name unknown → leaf marked unresolved; the qualifier
    ///   ident's own `record_use` will have flagged the unknown name.
    fn bind_qualified_type_leaf(&mut self, ty: &TypeRef) {
        let module_segment = *ty
            .qualifier
            .last()
            .expect("bind_qualified_type_leaf called with empty qualifier");
        let module_name = self.ident_text(module_segment).to_string();
        let leaf = ty.name;
        let leaf_name = self.ident_text(leaf).to_string();
        let Some(module_uri) = self.index.module_uri(&module_name).cloned() else {
            self.res.unresolved.push(leaf);
            return;
        };
        let hit = self
            .index
            .locate_decl(&leaf_name)
            .iter()
            .find(|(uri, _)| uri == &module_uri)
            .map(|(uri, decl)| (uri.clone(), *decl));
        match hit {
            Some((uri, decl)) => {
                self.res
                    .uses
                    .insert(leaf, Definition::ProjectDecl { uri, decl });
            }
            None => {
                self.res.unresolved.push(leaf);
            }
        }
    }
}

/// Run name resolution against `hir` with no cross-module context — the
/// fallback index is just [`ProjectIndex::new`], which knows the
/// language primitives but no user-declared decls and no runtime
/// types (those come through the stdlib `.gcl` ingest). Per-file
/// callers (tests, per-request capabilities) use this; the project
/// pipeline uses [`resolve_with_index_for`] so cross-module names
/// also resolve and the current module's own entries are excluded
/// from the global public-lookup tier.
pub fn resolve(hir: &Hir, symbols: &SymbolTable) -> Resolutions {
    let mut arena = TypeArena::new();
    let index = ProjectIndex::with_symbols(symbols.clone(), &mut arena);
    resolve_inner(hir, &index, None)
}

// P6.2
/// Run name resolution against `hir`, falling back to `index` for names
/// that aren't satisfied by any local scope. Project-pipeline callers
/// should prefer [`resolve_with_index_for`] so the current module's
/// own decls can be filtered out of the global-public lookup; this
/// shim survives for callers that don't have a URI handy.
pub fn resolve_with_index(hir: &Hir, index: &ProjectIndex) -> Resolutions {
    resolve_inner(hir, index, None)
}

// P38.3
/// Run name resolution with both cross-module context and the
/// current module's URI. The URI lets the global-public lookup at
/// step 2 of `record_use` skip entries declared in the current
/// module, so same-module private decls fall through to the
/// last-resort step 3 instead of accidentally binding to themselves
/// via `ProjectDecl`.
pub fn resolve_with_index_for(hir: &Hir, index: &ProjectIndex, current_uri: &Uri) -> Resolutions {
    resolve_inner(hir, index, Some(current_uri))
}

fn resolve_inner(hir: &Hir, index: &ProjectIndex, current_uri: Option<&Uri>) -> Resolutions {
    let mut cx = Cx::new(hir, index, current_uri);

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
    // P38.3 — route on visibility. Public decls join the first-tier
    // lookup namespace alongside nested scopes; private decls go to
    // the last-resort fallback table. See the order doctrine in
    // `record_use`.
    if decl_is_private(decl) {
        cx.module_private.insert(name, Definition::Decl(decl_id));
    } else {
        cx.module_public.insert(name, Definition::Decl(decl_id));
    }
}

// P38.3
/// Returns `true` iff the decl carries the `private` modifier.
/// Pragmas have no visibility concept; they're treated as public so
/// they continue to participate in normal name resolution (unchanged
/// from pre-P38 behavior).
fn decl_is_private(decl: &Decl) -> bool {
    match decl {
        Decl::Fn(d) => d.modifiers.private,
        Decl::Type(d) => d.modifiers.private,
        Decl::Enum(d) => d.modifiers.private,
        Decl::Var(d) => d.modifiers.private,
        Decl::Pragma(_) => false,
    }
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

/// Walk a `BlockStmt` body in its own scope. Body-bearing statements
/// (`If::then_branch`, `While::body`, `Try::try_block`, …) hold the
/// `BlockStmt` directly post-refactor — calling [`visit_stmt`] on
/// `Idx<Stmt>` no longer works for those bodies.
fn visit_block(cx: &mut Cx, block: &greycat_analyzer_hir::types::BlockStmt) {
    cx.push_scope();
    for s in &block.stmts {
        visit_stmt(cx, *s);
    }
    cx.pop_scope();
}

fn visit_stmt(cx: &mut Cx, stmt_id: Idx<Stmt>) {
    let stmt = cx.hir.stmts[stmt_id].clone();
    match stmt {
        Stmt::Block(b) => visit_block(cx, &b),
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
            visit_block(cx, &then_branch);
            if let Some(eb) = else_branch {
                visit_stmt(cx, eb);
            }
        }
        Stmt::While(WhileStmt {
            condition, body, ..
        }) => {
            visit_expr(cx, condition);
            visit_block(cx, &body);
        }
        Stmt::DoWhile(DoWhileStmt {
            body, condition, ..
        }) => {
            visit_block(cx, &body);
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
            visit_block(cx, &body);
            cx.pop_scope();
        }
        Stmt::ForIn(ForInStmt {
            params,
            range,
            body,
            ..
        }) => {
            visit_expr(cx, range);
            cx.push_scope();
            for p in &params {
                if let Some(t) = p.ty {
                    visit_type_ref(cx, t);
                }
                let n = cx.ident_text(p.name).to_string();
                cx.current_mut().insert(n, Definition::Local(p.name));
            }
            visit_block(cx, &body);
            cx.pop_scope();
        }
        Stmt::Return(value) => {
            if let Some(v) = value {
                visit_expr(cx, v);
            }
        }
        Stmt::Break | Stmt::Continue | Stmt::Breakpoint => {}
        Stmt::Throw(e) => visit_expr(cx, e),
        Stmt::Try(TryStmt {
            try_block,
            error_param,
            catch_block,
            ..
        }) => {
            visit_block(cx, &try_block);
            cx.push_scope();
            if let Some(name) = error_param {
                let n = cx.ident_text(name).to_string();
                cx.current_mut().insert(n, Definition::Local(name));
            }
            visit_block(cx, &catch_block);
            cx.pop_scope();
        }
        Stmt::At(AtStmt { expr, block, .. }) => {
            visit_expr(cx, expr);
            visit_block(cx, &block);
        }
    }
}

fn visit_expr(cx: &mut Cx, expr_id: Idx<Expr>) {
    let expr = cx.hir.exprs[expr_id].clone();
    match expr {
        Expr::Ident { name, .. } => cx.record_use(name),
        Expr::Literal(_) => {}
        Expr::String(StringExpr { parts, .. }) => {
            // P17.5 — recurse into `${expr}` interpolations so inner
            // idents are bound (otherwise variables referenced only
            // inside template strings stay `unresolved`).
            for part in parts {
                if let greycat_analyzer_hir::types::StringPart::Interp { expr, .. } = part {
                    visit_expr(cx, expr);
                }
            }
        }
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
        Expr::QualifiedStatic { chain, .. } => {
            // P15.8 — bind the leftmost segment as a regular use
            // (typically a module name or a type name). Subsequent
            // segments are members and bind via type-driven resolution
            // in the analyzer / pass 3.5, not here.
            if let Some(first) = chain.first() {
                cx.record_use(*first);
            }
        }
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
        Expr::Range { from, to, .. } => {
            if let Some(f) = from {
                visit_expr(cx, f);
            }
            if let Some(t) = to {
                visit_expr(cx, t);
            }
        }
        Expr::Unsupported { .. } => {
            // Lowering hasn't expanded this shape yet; nothing to bind.
        }
        Expr::Null { .. } | Expr::This { .. } => {
            // Keyword literals with no name to resolve.
        }
    }
}

fn visit_type_ref(cx: &mut Cx, ty_id: Idx<TypeRef>) {
    let ty = cx.hir.type_refs[ty_id].clone();
    if ty.qualifier.is_empty() {
        // Bare reference: full bare-name resolution, including the
        // `ambiguous-symbol` collapse when ≥2 modules export the leaf.
        cx.record_use(ty.name);
    } else {
        // Qualified reference (`b::Foo`, `a::b::Foo`, …): the user has
        // already disambiguated. Bind each qualifier segment as a
        // normal use (so module names get a binding for hover / goto)
        // and resolve the leaf in the named module specifically — the
        // bare-name path's ambiguous-symbol collapse must NOT fire on
        // a leaf the user explicitly qualified.
        for q in ty.qualifier.iter() {
            cx.record_use(*q);
        }
        cx.bind_qualified_type_leaf(&ty);
    }
    for p in ty.params {
        visit_type_ref(cx, p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_core::SymbolTable;
    use greycat_analyzer_hir::lower_module;
    use greycat_analyzer_hir::types::{Decl, Expr};
    use greycat_analyzer_syntax::parse;

    fn analyze(src: &str) -> (Hir, Resolutions, SymbolTable) {
        let tree = parse(src);
        let s = SymbolTable::default();
        let hir = lower_module(src, &s, "mod", "project", tree.root_node());
        let res = resolve(&hir, &s);
        (hir, res, s)
    }

    #[test]
    fn forward_ref_to_type_in_nested_generic_param() {
        // P14.9 regression: `type T { paths: Wrap<String, Inner>?; }`
        // followed by `type Inner {}` and `type Wrap<K, V> {}` — the
        // forward reference to `Inner` in the second generic-param
        // slot should resolve via the two-pass module-scope seed.
        // Uses a local `Wrap` rather than the runtime `Map` so the
        // per-file resolver (no stdlib ingest) recognises the head.
        let src = "type T { paths: Wrap<String, Inner>?; }\ntype Inner {}\ntype Wrap<K, V> {}\n";
        let (hir, res, s) = analyze(src);
        let inner_sym = s.lookup("Inner").expect("Inner interned");
        let inner_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.symbol == inner_sym)
            .filter_map(|(idx, _)| res.uses.get(&idx))
            .collect();
        assert_eq!(inner_uses.len(), 1, "Inner used once: {:?}", res.unresolved);
        assert!(matches!(inner_uses[0], Definition::Decl(_)));
        assert!(
            res.unresolved.is_empty(),
            "unresolved: {:?}",
            res.unresolved
        );
    }

    #[test]
    fn param_use_resolves_to_param() {
        let src = "fn id(x: int): int { return x; }\n";
        let (hir, res, s) = analyze(src);
        let x_sym = s.lookup("x").expect("x interned");

        // Find the use of `x` inside the body.
        let x_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.symbol == x_sym)
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
        let (hir, res, s) = analyze(src);
        let helper_sym = s.lookup("helper").expect("helper interned");
        // The Ident for the use of `helper` in caller's body.
        let helper_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.symbol == helper_sym)
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
        let (_hir, res, _s) = analyze(src);
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
        let (hir, res, s) = analyze(src);
        let x_sym = s.lookup("x").expect("x interned");
        let return_x_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.symbol == x_sym)
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
        let (hir, res, s) = analyze(src);
        let foo_sym = s.lookup("Foo").expect("Foo interned");
        let p_sym = s.lookup("p").expect("p interned");
        let foo_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.symbol == foo_sym)
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
            .filter(|(_, i)| i.symbol == p_sym)
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
        let (hir, res, s) = analyze(src);
        let t_sym = s.lookup("T").expect("T interned");
        let t_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.symbol == t_sym)
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
        use std::str::FromStr;
        // Module A declares `Helper` as a top-level type. Module B
        // refers to `Helper` — without a ProjectIndex it'd be
        // unresolved; with one ingested from A it binds to ProjectDecl
        // carrying A's URI + the Helper decl id (P11.2).
        let mut arena = TypeArena::new();
        let mut decl_registry = crate::well_known::DeclRegistry::default();
        let mut well_known = crate::well_known::WellKnown::default();
        let mut idx = ProjectIndex::new(&mut arena);

        let other_src = "type Helper {}\n";
        let other_tree = parse(other_src);
        let other_hir = lower_module(other_src, &idx.symbols, "a", "p", other_tree.root_node());

        let other_uri = Uri::from_str("file:///proj/a.gcl").unwrap();
        idx.ingest(
            &other_uri,
            &other_hir,
            &mut arena,
            &mut decl_registry,
            &mut well_known,
        );

        let user_src = "fn use_helper(h: Helper) {}\n";
        let user_tree = parse(user_src);
        let user_hir = lower_module(user_src, &idx.symbols, "b", "p", user_tree.root_node());
        let res = resolve_with_index(&user_hir, &idx);
        let helper_sym = idx.symbols.lookup("Helper").expect("Helper interned");

        let helper_uses: Vec<_> = user_hir
            .idents
            .iter()
            .filter(|(_, i)| i.symbol == helper_sym)
            .filter_map(|(idx, _)| res.uses.get(&idx))
            .collect();
        assert_eq!(helper_uses.len(), 1);
        let Definition::ProjectDecl { uri, decl } = helper_uses[0] else {
            panic!("expected ProjectDecl, got {:?}", helper_uses[0]);
        };
        assert_eq!(uri, &other_uri);
        assert!(matches!(other_hir.decls[*decl], Decl::Type(_)));
        assert!(res.unresolved.is_empty());
    }

    // P17.2
    /// `for (i, x in xs) { ... i ... x ... }` should bind both
    /// `i` and `x` as locals in the body. Was silently dropping the
    /// entire `for_in_stmt` because lowering misread the iterator
    /// expression as a param wrapper (the `?` short-circuit on the
    /// non-existent `name` field returned `None`).
    #[test]
    fn for_in_tuple_form_binds_both_params() {
        // `Xs` is a local type so the per-file resolver (no stdlib
        // ingest) can still recognise the iterator's type — runtime
        // types like `Array` only land in scope through stdlib.
        let src = "type Xs {}\nfn f(xs: Xs) { for (i, x in xs) { var s = i + x; } }\n";
        let (hir, res, s) = analyze(src);
        for name in ["i", "x"] {
            let needle_sym = s.lookup(name).expect("name interned");
            let uses: Vec<_> = hir
                .idents
                .iter()
                .filter(|(_, id)| id.symbol == needle_sym)
                .filter_map(|(idx, _)| res.uses.get(&idx))
                .collect();
            assert!(
                uses.iter().any(|d| matches!(d, Definition::Local(_))),
                "expected `{name}` use to bind to a Local, got {uses:?}"
            );
        }
        assert!(
            res.unresolved.is_empty(),
            "no idents should be unresolved, got {:?}",
            res.unresolved
        );
    }

    // P17.3
    /// `try { ... } catch (ex) { ... ex ... }` should bind
    /// `ex` as a Local in the catch block. Was silently unresolved
    /// because lowering asked for a `name` sub-field on `_catch_param`,
    /// which the grammar doesn't declare; the hidden-rule inlining
    /// also meant `child_by_field_name` returned the `(` token, not
    /// the ident — so the binding ended up empty.
    #[test]
    fn catch_param_binds_in_catch_block() {
        let src = "fn f() { try { } catch (ex) { throw ex; } }\n";
        let (hir, res, s) = analyze(src);
        let ex_sym = s.lookup("ex").expect("ex interned");
        let ex_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.symbol == ex_sym)
            .filter_map(|(idx, _)| res.uses.get(&idx))
            .collect();
        assert_eq!(
            ex_uses.len(),
            1,
            "expected exactly one `ex` use, got {ex_uses:?}"
        );
        assert!(
            matches!(ex_uses[0], Definition::Local(_)),
            "expected Local binding for catch param, got {:?}",
            ex_uses[0]
        );
        assert!(res.unresolved.is_empty(), "no idents should be unresolved");
    }

    // P35.8 removed `project_index_fallback_keeps_unit_project_for_runtime_types`:
    // the previous behavior seeded runtime-type names (`Array`, `Map`,
    // `node`, …) into `type_names` without an `.gcl` decl, so the
    // resolver answered `Definition::Project` for them in unit tests
    // that skipped stdlib ingest. After the seeding list was deleted,
    // those names only become known when stdlib is loaded — the
    // `ProjectDecl` answer is then richer than `Project`. Coverage
    // for the cross-module fallback lives in
    // `project_index_fallback_resolves_cross_module_name`.
}
