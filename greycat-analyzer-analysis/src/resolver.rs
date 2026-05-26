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
use greycat_analyzer_core::{Symbol, SymbolTable, TypeArena};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{
    AssignStmt, AtStmt, BinaryExpr, CallExpr, Decl, DoWhileStmt, Expr, FnDecl, ForInStmt, ForStmt,
    Ident, IfStmt, LambdaExpr, LocalVar, MemberExpr, ObjectExpr, OffsetExpr, Pragma, Stmt,
    StringExpr, TryStmt, TypeAttr, TypeDecl, TypeRef, UnaryExpr, VarDeclTop, WhileStmt,
};

use crate::stdlib::{Namespace, ProjectIndex};

/// Where in source a name was used — drives the per-namespace lookup
/// order in [`Cx::record_use`]. The GreyCat runtime keeps three
/// top-level name slots ([`Namespace::Type`], [`Namespace::Fn`],
/// [`Namespace::Var`]); `Type` positions look in type-ns only, value
/// positions try fn-ns then var-ns. Within each namespace the
/// existing runtime-conformant ladder (nested → module-public →
/// global-public → module-private) runs unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    /// Bare ident at a type-annotation position (`var x: Foo`,
    /// `T extends Foo`, `Object<Foo>`).
    Type,
    /// Bare ident at a value-expression position (`Foo()`, bare
    /// `Foo` in expr / qualifier-segment / static-receiver).
    Value,
}

impl Position {
    fn namespaces(self) -> &'static [Namespace] {
        match self {
            Position::Type => &[Namespace::Type],
            // Fn before Var: matches the analyzer's existing
            // `contains_fn_signature`-first preference when typing a
            // bare ident, and the runtime's apparent behavior
            // (verified via `greycat build` on cross-module `fn`/`var`
            // mixes).
            //
            // Type is a last-resort fallback so a bare type / enum
            // ident in value position (passed as a runtime *type
            // literal*, e.g. `type::enum_by_name(DurationUnit, "ms")`
            // or `node::create(MyType)`) binds to its declaring decl
            // rather than falling out as an unresolved name. The
            // analyzer then types it as `TypeOf(<that decl>)` so the
            // typeof-aware generic inference rule can witness
            // `T := <that decl>`. Putting Type last preserves the
            // Fn-then-Var precedence when names overlap.
            Position::Value => &[Namespace::Fn, Namespace::Var, Namespace::Type],
        }
    }
}

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
    /// Idents inside a lambda body that resolved to a Local/Param bound
    /// in an enclosing scope. GreyCat lambdas don't capture — the
    /// runtime rejects these as `unresolved identifier`. The resolver
    /// still records a binding (so goto-def / hover work on the actual
    /// declaration) but the analyzer surfaces a `lambda-capture` error.
    pub captured: Vec<Idx<Ident>>,
    /// `this` byte ranges inside lambda bodies. The runtime segfaults
    /// on this shape today; we forbid it at analyze time.
    pub this_in_lambda: Vec<greycat_analyzer_hir::types::Span>,
    /// Idents that re-bind a name already declared as a `Local` or
    /// `Param` in the *same* lexical scope. The runtime rejects this
    /// shape with `already declared var` / `already declared param`;
    /// the analyzer surfaces it as `local-rebind`. Nested scopes can
    /// still shadow — only same-scope collisions are recorded here.
    /// `Generic` bindings live in a separate conceptual slot and don't
    /// collide with value bindings.
    pub rebound: Vec<Idx<Ident>>,
}

impl Resolutions {
    pub fn lookup(&self, ident: Idx<Ident>) -> Option<Definition> {
        self.uses.get(&ident).cloned()
    }
}

#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum ScopeKind {
    #[default]
    Default,
    /// The scope that holds a lambda's params + body locals. Treated as
    /// a hard capture boundary: lookups crossing it from inside that
    /// scope toward an outer one signal an illegal capture (GreyCat
    /// lambdas have a closed scope — only own params/locals + module-
    /// scope decls are reachable).
    LambdaBody,
}

#[derive(Default)]
struct Scope {
    /// Lexical name → resolution. Keyed by [`Symbol`] (interned) so the
    /// hot insert / lookup path doesn't allocate per ident.
    names: FxHashMap<Symbol, Definition>,
    kind: ScopeKind,
}

impl Scope {
    fn insert(&mut self, name: Symbol, def: Definition) {
        self.names.insert(name, def);
    }
}

struct Cx<'a> {
    hir: &'a Hir,
    // Module-scope bindings split by visibility. Both tiers are
    // consulted in the same step of `record_use` — module-local decls
    // (public OR private) shadow cross-module hits. The split is kept
    // because privacy still gates *cross-module* visibility (the
    // `is_decl_private` filter in step 3 of `record_use`) and because
    // hover / goto-def want to know which tier a binding came from.
    //
    // Within each visibility tier, decls split by [`Namespace`] (type,
    // fn, var) — validated against `greycat build` 8.0.301-dev: every
    // cross-namespace pair coexists, every in-namespace pair errors.
    /// Module-level `type` / `enum` decls without `private`.
    module_public_type: FxHashMap<Symbol, Definition>,
    /// Module-level `fn` decls without `private`.
    module_public_fn: FxHashMap<Symbol, Definition>,
    /// Module-level `var` decls without `private`.
    module_public_var: FxHashMap<Symbol, Definition>,
    /// Module-level `type` / `enum` decls with `private`.
    module_private_type: FxHashMap<Symbol, Definition>,
    /// Module-level `fn` decls with `private`.
    module_private_fn: FxHashMap<Symbol, Definition>,
    /// Module-level `var` decls with `private`.
    module_private_var: FxHashMap<Symbol, Definition>,
    /// Nested lexical scopes (fn / type / block / loop / try / catch).
    /// The module-level scope is *not* held here — see
    /// `module_public_*` / `module_private_*` above.
    scopes: Vec<Scope>,
    // P6.1 — project pipeline passes the rebuilt index.
    /// Project-level fallback for names that miss every local scope.
    /// Per-file callers pass an empty [`ProjectIndex::new`]; the project
    /// pipeline passes the index it just rebuilt.
    index: &'a ProjectIndex,
    // Current module's URI, when known. The project pipeline passes
    // the module's URI through; per-file callers (tests, lint pipeline
    // without project context) pass `None`. Lets the cross-module
    // lookup filter out the current module's own entries from
    // `ProjectIndex::locate_decl` so module-local decls are always
    // served from `module_public_*` / `module_private_*` rather than
    // re-entering through the cross-module path.
    current_uri: Option<&'a Uri>,
    res: Resolutions,
}

impl<'a> Cx<'a> {
    fn new(hir: &'a Hir, index: &'a ProjectIndex, current_uri: Option<&'a Uri>) -> Self {
        Self {
            hir,
            module_public_type: FxHashMap::default(),
            module_public_fn: FxHashMap::default(),
            module_public_var: FxHashMap::default(),
            module_private_type: FxHashMap::default(),
            module_private_fn: FxHashMap::default(),
            module_private_var: FxHashMap::default(),
            scopes: Vec::new(),
            index,
            current_uri,
            res: Resolutions::default(),
        }
    }

    fn module_public(&self, ns: Namespace) -> &FxHashMap<Symbol, Definition> {
        match ns {
            Namespace::Type => &self.module_public_type,
            Namespace::Fn => &self.module_public_fn,
            Namespace::Var => &self.module_public_var,
        }
    }

    fn module_public_mut(&mut self, ns: Namespace) -> &mut FxHashMap<Symbol, Definition> {
        match ns {
            Namespace::Type => &mut self.module_public_type,
            Namespace::Fn => &mut self.module_public_fn,
            Namespace::Var => &mut self.module_public_var,
        }
    }

    fn module_private(&self, ns: Namespace) -> &FxHashMap<Symbol, Definition> {
        match ns {
            Namespace::Type => &self.module_private_type,
            Namespace::Fn => &self.module_private_fn,
            Namespace::Var => &self.module_private_var,
        }
    }

    fn module_private_mut(&mut self, ns: Namespace) -> &mut FxHashMap<Symbol, Definition> {
        match ns {
            Namespace::Type => &mut self.module_private_type,
            Namespace::Fn => &mut self.module_private_fn,
            Namespace::Var => &mut self.module_private_var,
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }
    fn push_lambda_scope(&mut self) {
        self.scopes.push(Scope {
            names: FxHashMap::default(),
            kind: ScopeKind::LambdaBody,
        });
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn current_mut(&mut self) -> &mut Scope {
        self.scopes
            .last_mut()
            .expect("at least one nested scope is live (push_scope must precede insert)")
    }

    /// Bind a `Local` or `Param` into the current scope, surfacing a
    /// `local-rebind` if its name is already present in the *same*
    /// scope as a value binding. Nested-scope shadowing stays silent —
    /// only same-scope collisions are flagged, mirroring the runtime
    /// (`already declared var` / `already declared param`).
    ///
    /// On collision the original binding is preserved so subsequent
    /// uses of the name still resolve to the first declaration (matching
    /// the user's likely intent: the param `x` keeps meaning the param,
    /// even when a later `var x` shadows it textually).
    fn bind_value(&mut self, name: Idx<Ident>, def: Definition) {
        debug_assert!(matches!(def, Definition::Local(_) | Definition::Param(_)));
        let sym = self.hir.idents[name].symbol;
        let scope = self.current_mut();
        match scope.names.get(&sym) {
            Some(Definition::Local(_) | Definition::Param(_)) => {
                self.res.rebound.push(name);
            }
            _ => {
                scope.insert(sym, def);
            }
        }
    }

    /// Walk nested lexical scopes (params / locals / generics /
    /// for-in / catch) innermost-first. Scopes are not partitioned by
    /// namespace — the grammar disallows re-using a single scope's
    /// name across kinds. Module-level tables are *not* consulted
    /// here; `record_use` handles them per-namespace.
    ///
    /// Returns `(definition, captured_from_outside_lambda)`. The boolean
    /// is true when the match was found in a scope *above* a
    /// [`ScopeKind::LambdaBody`] boundary — i.e. the use site is inside
    /// a lambda body and the binding lives in an enclosing function's
    /// scope. Callers surface that case as a `lambda-capture` error.
    fn lookup_scope(&self, name: Symbol) -> Option<(Definition, bool)> {
        let mut crossed_lambda = false;
        for scope in self.scopes.iter().rev() {
            if let Some(d) = scope.names.get(&name) {
                return Some((d.clone(), crossed_lambda));
            }
            if scope.kind == ScopeKind::LambdaBody {
                crossed_lambda = true;
            }
        }
        None
    }

    fn record_use(&mut self, idx: Idx<Ident>, pos: Position) {
        let name_sym = self.hir.idents[idx].symbol;
        // Bare-name resolution order, applied per namespace in the
        // priority list for `pos`:
        //
        //   1. Non-module scopes (universal, walked once at the top).
        //   2. Module-local decls in this namespace — PUBLIC or
        //      PRIVATE. Module-local always shadows cross-module.
        //   3. Cross-module PUBLIC across the project closure
        //      (`ProjectIndex::locate_decl_in_ns`, filtered to drop
        //      foreign private decls). Multiple hits in the same
        //      namespace collapse to *ambiguous-symbol*
        //      (Severity::Error with FQN quick-fixes).
        //   4. `Project` placeholder (runtime-implemented types,
        //      primitives by name, native fns), known module names,
        //      or unresolved.
        //
        // Module-local-first matches the GreyCat runtime: e.g. a
        // `private type Load` declared in module M shadows a public
        // `type Load` declared elsewhere in the project closure when
        // resolution happens *inside* M. The previous ordering (which
        // demoted module-local PRIVATE below cross-module PUBLIC) was
        // wrong about runtime conformance — see the now-inverted
        // `cross_module_private_fallback.rs::local_private_shadows_remote_public`.
        //
        // Across namespaces: at a value position the priority is fn
        // before var (matches the analyzer's
        // `contains_fn_signature`-first preference at
        // `Definition::ProjectDecl` value typing). A miss in one
        // namespace falls through to the next; an ambiguity in any
        // namespace short-circuits the whole walk.

        // 1. Nested scopes are namespace-agnostic.
        if let Some((def, captured)) = self.lookup_scope(name_sym) {
            if let Definition::Decl(decl_id) = &def {
                *self.res.references_to.entry(*decl_id).or_insert(0) += 1;
            }
            // Locals / params bound outside the enclosing lambda are
            // captures — GreyCat doesn't allow them. Record the use
            // anyway so goto-def / hover still point at the real
            // declaration; the analyzer surfaces a `lambda-capture`
            // error from the `captured` list.
            if captured && matches!(&def, Definition::Local(_) | Definition::Param(_)) {
                self.res.captured.push(idx);
            }
            self.res.uses.insert(idx, def);
            return;
        }

        // 2 + 3, per namespace in the position's priority order.
        for &ns in pos.namespaces() {
            // Step 2: module-local (public OR private) in this
            // namespace. Module-local always shadows cross-module.
            if let Some(def) = self
                .module_public(ns)
                .get(&name_sym)
                .or_else(|| self.module_private(ns).get(&name_sym))
                .cloned()
            {
                if let Definition::Decl(decl_id) = &def {
                    *self.res.references_to.entry(*decl_id).or_insert(0) += 1;
                }
                self.res.uses.insert(idx, def);
                return;
            }
            // Step 3: cross-module PUBLIC in this namespace, with
            // current-module and `private` filters.
            let cross_module_hits: Vec<(Uri, Idx<Decl>)> = self
                .index
                .locate_decl_in_ns(name_sym, ns)
                .filter(|(uri, decl)| {
                    let from_other_module = self.current_uri.map(|cur| uri != &cur).unwrap_or(true);
                    from_other_module && !self.index.is_decl_private(uri, *decl)
                })
                .map(|(uri, decl)| (uri.clone(), decl))
                .collect();
            match cross_module_hits.len() {
                0 => continue,
                1 => {
                    let (uri, decl) = cross_module_hits.into_iter().next().unwrap();
                    self.res
                        .uses
                        .insert(idx, Definition::ProjectDecl { uri, decl });
                    return;
                }
                _ => {
                    self.res.ambiguous.insert(idx, cross_module_hits);
                    self.res.unresolved.push(idx);
                    return;
                }
            }
        }

        // 4. Project-level fallback.
        if self.index.has_name(name_sym) {
            self.res.uses.insert(idx, Definition::Project);
            return;
        }
        // P15.x — known module name (the leftmost segment of a
        // `module::Decl` chain). Bind to `Project` so it's not
        // flagged unresolved; goto-def hits via `goto_module_segment`
        // (P15.9), inference via pass 3.5.
        if self.index.module_names.contains_key(&name_sym) {
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
        let module_sym = self.hir.idents[module_segment].symbol;
        let leaf = ty.name;
        let leaf_sym = self.hir.idents[leaf].symbol;
        let Some(module_uri) = self.index.module_names.get(&module_sym).cloned() else {
            self.res.unresolved.push(leaf);
            return;
        };
        // Leaf of a qualified TypeRef is unambiguously a type — filter
        // out same-named values declared in the module.
        let hit = self
            .index
            .locate_decl_in_ns(leaf_sym, Namespace::Type)
            .find(|(uri, _)| *uri == &module_uri)
            .map(|(uri, decl)| (uri.clone(), decl));
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
    let Some(ns) = Namespace::of_decl(decl) else {
        return;
    };
    let name_sym = cx.hir.idents[name_id].symbol;
    // P38.3 — route on visibility, then on namespace. Public decls
    // join the first-tier lookup namespace alongside nested scopes;
    // private decls go to the last-resort fallback table. See the
    // order doctrine in `record_use`. Three namespaces (type, fn,
    // var) — validated against the runtime — let same-name decls
    // across different kinds coexist.
    let table = if decl_is_private(decl) {
        cx.module_private_mut(ns)
    } else {
        cx.module_public_mut(ns)
    };
    table.insert(name_sym, Definition::Decl(decl_id));
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
        let sym = cx.hir.idents[*g].symbol;
        cx.current_mut().insert(sym, Definition::Generic(*g));
    }
    // Parameters become Param bindings in the function scope.
    for param_id in &d.params {
        let p = cx.hir.fn_params[*param_id].clone();
        cx.bind_value(p.name, Definition::Param(p.name));
        if let Some(ty) = p.ty {
            visit_type_ref(cx, ty);
        }
    }
    if let Some(rt) = d.return_type {
        visit_type_ref(cx, rt);
    }
    // The body block is the *same* scope as the params (matches the
    // runtime: `fn foo(x) { var x = …; }` is rejected). Walk the body
    // block's stmts inline; nested blocks still introduce their own
    // scopes via the regular `visit_block` path.
    if let Some(body) = d.body
        && let Stmt::Block(b) = cx.hir.stmts[body].clone()
    {
        visit_block_inline(cx, &b);
    }
    cx.pop_scope();
}

fn visit_type_decl(cx: &mut Cx, d: &TypeDecl) {
    cx.push_scope();
    // Generic params visible inside attribute types and method bodies.
    for g in &d.generics {
        let sym = cx.hir.idents[*g].symbol;
        cx.current_mut().insert(sym, Definition::Generic(*g));
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
    visit_block_inline(cx, block);
    cx.pop_scope();
}

/// Walk a `BlockStmt`'s stmts in the *current* scope (no push/pop).
/// Used by `visit_fn_decl` / `Expr::Lambda` so the params and the
/// immediate body block share one scope — matching the runtime, which
/// rejects `fn foo(x) { var x = …; }` as `already declared var`.
fn visit_block_inline(cx: &mut Cx, block: &greycat_analyzer_hir::types::BlockStmt) {
    for s in &block.stmts {
        visit_stmt(cx, *s);
    }
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
            cx.bind_value(name, Definition::Local(name));
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
                cx.bind_value(name, Definition::Local(name));
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
                cx.bind_value(p.name, Definition::Local(p.name));
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
            // Catch param shares scope with the catch block body — the
            // runtime rejects `catch (e) { var e = …; }` as
            // `already declared var`.
            cx.push_scope();
            if let Some(name) = error_param {
                cx.bind_value(name, Definition::Local(name));
            }
            visit_block_inline(cx, &catch_block);
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
        Expr::Ident { name, .. } => cx.record_use(name, Position::Value),
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
                // Leftmost segment of `Foo::bar` is either a type
                // (static method / static attr / enum variant access)
                // or a module name (`std::Foo::bar`). Module names
                // aren't in `decl_locations`; the namespaced lookup
                // misses cleanly and the existing `has_module`
                // fallback at the tail of `record_use` catches them.
                cx.record_use(*first, Position::Type);
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
        Expr::Lambda(LambdaExpr {
            params,
            return_type,
            body,
            ..
        }) => {
            cx.push_lambda_scope();
            for param_id in params {
                let p = cx.hir.fn_params[param_id].clone();
                cx.bind_value(p.name, Definition::Param(p.name));
                if let Some(t) = p.ty {
                    visit_type_ref(cx, t);
                }
            }
            if let Some(t) = return_type {
                visit_type_ref(cx, t);
            }
            // Lambda params and body share one scope (mirrors fn-decl).
            visit_block_inline(cx, &body);
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
        Expr::Null { .. } => {
            // Keyword literal with no name to resolve.
        }
        Expr::This { byte_range } => {
            // `this` inside a lambda body is forbidden — the runtime
            // segfaults on this shape, and lambdas have a closed scope
            // by design. Detect via the [`ScopeKind::LambdaBody`]
            // marker on the scope stack rather than a parallel counter.
            if cx.scopes.iter().any(|s| s.kind == ScopeKind::LambdaBody) {
                cx.res.this_in_lambda.push(byte_range);
            }
        }
    }
}

fn visit_type_ref(cx: &mut Cx, ty_id: Idx<TypeRef>) {
    let ty = cx.hir.type_refs[ty_id].clone();
    if ty.qualifier.is_empty() {
        // Bare reference: full bare-name resolution, including the
        // `ambiguous-symbol` collapse when ≥2 modules export the leaf.
        // Type-position — only type/enum decls participate.
        cx.record_use(ty.name, Position::Type);
    } else {
        // Qualified reference (`b::Foo`, `a::b::Foo`, …): the user has
        // already disambiguated. Bind each qualifier segment as a
        // normal use (so module names get a binding for hover / goto)
        // and resolve the leaf in the named module specifically — the
        // bare-name path's ambiguous-symbol collapse must NOT fire on
        // a leaf the user explicitly qualified. Qualifier segments
        // are module names; `has_module` catches them at the tail of
        // `record_use`.
        for q in ty.qualifier.iter() {
            cx.record_use(*q, Position::Type);
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
    fn local_var_rebinding_param_is_recorded_and_param_wins() {
        // `var x` after param `x` in the same fn body is a rebind error
        // (runtime: `already declared var`). The resolver records the
        // rebind in `res.rebound` and keeps the param as the active
        // binding so the use site `return x` resolves to the param.
        let src = r#"
fn f(x: int): int {
    var x: int = 99;
    return x;
}
"#;
        let (hir, res, s) = analyze(src);
        let x_sym = s.lookup("x").expect("x interned");
        assert_eq!(
            res.rebound.len(),
            1,
            "expected exactly one rebound ident, got: {:?}",
            res.rebound
        );
        let return_x_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.symbol == x_sym)
            .filter_map(|(idx, _)| res.uses.get(&idx))
            .collect();
        assert!(
            return_x_uses
                .iter()
                .any(|d| matches!(d, Definition::Param(_))),
            "expected the param to win on collision: {return_x_uses:?}",
        );
        assert!(
            !return_x_uses
                .iter()
                .any(|d| matches!(d, Definition::Local(_))),
            "no use site should bind to the rejected local: {return_x_uses:?}",
        );
    }

    #[test]
    fn local_var_shadows_outer_binding_in_nested_block() {
        // Nested scopes can still shadow — `var x` in an `if` body
        // doesn't collide with an outer param `x`.
        let src = r#"
fn f(x: int): int {
    if (x > 0) {
        var x: int = 99;
        return x;
    }
    return x;
}
"#;
        let (hir, res, s) = analyze(src);
        let x_sym = s.lookup("x").expect("x interned");
        assert!(
            res.rebound.is_empty(),
            "nested-scope shadow should not record a rebind: {:?}",
            res.rebound
        );
        let all_x_uses: Vec<_> = hir
            .idents
            .iter()
            .filter(|(_, i)| i.symbol == x_sym)
            .filter_map(|(idx, _)| res.uses.get(&idx))
            .collect();
        assert!(
            all_x_uses.iter().any(|d| matches!(d, Definition::Local(_))),
            "expected the inner `return x` to bind the shadowing local: {all_x_uses:?}",
        );
        assert!(
            all_x_uses.iter().any(|d| matches!(d, Definition::Param(_))),
            "expected the outer `return x` to bind the param: {all_x_uses:?}",
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
