// P35.1
//! Project-wide stable handles for resolved type decls and the
//! "well-known" std/core slots the analyzer dispatches against.
//!
//! Decl-handle identity replaces SmolStr-name identity:
//!
//! - [`DeclRegistry`] interns `(Uri, Idx<Decl>)` pairs into dense
//!   `Copy` [`TypeDeclId`] handles. Used by the project orchestrator
//!   while lowering signatures.
//! - [`WellKnown`] holds one `Option<TypeDeclId>` slot per native type
//!   the analyzer special-cases (`node`, `Array`, `function`, etc.).
//!   Populated as decls flow through the signature lowering pass; a
//!   `Decl::Type` whose `(module.lib, module.name, decl_name)` matches
//!   `("std", "core", N)` stashes its handle into slot `N`.

use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::Decl;
use greycat_analyzer_types::TypeDeclId;
use rustc_hash::FxHashMap;

/// Append-only registry mapping `(Uri, Idx<Decl>)` pairs to dense
/// [`TypeDeclId`]s. Idempotent — the same pair always resolves to the
/// same handle within a single registry instance.
///
/// Two `TypeDeclId`s from the same registry compare equal iff they
/// were issued for the same `(uri, decl)` pair. Across registry
/// instances, handles are not comparable.
///
/// Decl *names* aren't stored here — the arena owns them via
/// [`greycat_analyzer_types::TypeArena::decl_name`], registered at
/// `alloc_type` / `alloc_generic_instance` time. This keeps the
/// registry to a single responsibility (handle identity) and lets
/// downstream consumers render types through `arena.display(id)` with
/// no registry borrow.
/// One entry per resolved `(Uri, Idx<Decl>)` pair.
#[derive(Debug, Clone)]
struct DeclEntry {
    uri: Uri,
    decl: Idx<Decl>,
}

#[derive(Debug, Default, Clone)]
pub struct DeclRegistry {
    items: Vec<DeclEntry>,
    intern: FxHashMap<(Uri, Idx<Decl>), TypeDeclId>,
}

impl DeclRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `(uri, decl)`. Idempotent — re-calling with the same
    /// pair returns the previously-issued handle.
    pub fn get_or_insert(&mut self, uri: &Uri, decl: Idx<Decl>) -> TypeDeclId {
        let key = (uri.clone(), decl);
        if let Some(&id) = self.intern.get(&key) {
            return id;
        }
        let id = TypeDeclId::from_raw(self.items.len() as u32);
        self.items.push(DeclEntry {
            uri: uri.clone(),
            decl,
        });
        self.intern.insert(key, id);
        id
    }

    /// Read-only lookup: returns the handle for `(uri, decl)` or
    /// `None` if no one has interned it yet.
    pub fn lookup(&self, uri: &Uri, decl: Idx<Decl>) -> Option<TypeDeclId> {
        self.intern.get(&(uri.clone(), decl)).copied()
    }

    /// Resolve a handle back to its `(uri, decl)` source.
    pub fn resolve(&self, id: TypeDeclId) -> Option<(&Uri, Idx<Decl>)> {
        self.items.get(id.raw() as usize).map(|e| (&e.uri, e.decl))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Well-known std/core type decl handles. Each slot is `Some` once
/// the corresponding `Decl::Type` has been seen during signature
/// lowering; `None` when std hasn't been loaded yet (the
/// [`crate::project::ProjectAnalysis::analyze`] entry on a project
/// without std) or before that decl has flowed through the pipeline.
///
/// The slot list mirrors the `native type` decls declared in
/// [`lib/std/core.gcl`](../../lib/std/core.gcl) that the analyzer
/// dispatches on by identity (node-tag auto-deref, runtime-sentinel
/// types, common collections). Adding a slot is fine; removing one
/// only if every consumer that read it has migrated.
#[derive(Debug, Default, Clone)]
pub struct WellKnown {
    // Primitive-shaped natives. The analyzer also has
    // `TypeKind::Primitive` for the same conceptual things; the decl
    // handles let cross-module references know they're talking about
    // the std-core decl specifically, not an unrelated user-defined
    // type that happens to share the name.
    pub bool_decl: Option<TypeDeclId>,
    pub char_decl: Option<TypeDeclId>,
    pub int_decl: Option<TypeDeclId>,
    pub float_decl: Option<TypeDeclId>,
    pub string_decl: Option<TypeDeclId>,
    pub time_decl: Option<TypeDeclId>,
    pub duration_decl: Option<TypeDeclId>,
    pub geo_decl: Option<TypeDeclId>,

    // Top / bottom equivalents — `any` and `null` are also declared
    // as `native type` in std/core.
    pub any_decl: Option<TypeDeclId>,
    pub null_decl: Option<TypeDeclId>,

    // Runtime sentinels — `type`, `field`, `function`. The 15+
    // `arena.named("function")` / `"type"` / `"field"` sites in
    // [`crate::analyzer`] swap onto these handles in P35.4.
    pub type_decl: Option<TypeDeclId>,
    pub field_decl: Option<TypeDeclId>,
    pub function_decl: Option<TypeDeclId>,

    // Node-tag generics — the auto-deref family. P35.5 rewrites
    // [`greycat_analyzer_types::is_node_tag`] as a comparison against
    // these handles.
    pub node_decl: Option<TypeDeclId>,
    pub node_time_decl: Option<TypeDeclId>,
    pub node_index_decl: Option<TypeDeclId>,
    pub node_list_decl: Option<TypeDeclId>,
    pub node_geo_decl: Option<TypeDeclId>,

    // Common generic collections.
    pub array_decl: Option<TypeDeclId>,
    pub map_decl: Option<TypeDeclId>,
    pub buffer_decl: Option<TypeDeclId>,
    pub table_decl: Option<TypeDeclId>,
    pub tensor_decl: Option<TypeDeclId>,
    /// `Tuple<T, U>` from `lib/std/core.gcl`. `(x, y)` tuple-literal
    /// syntax desugars to `Tuple<T, U>{x, y}` per the compiler, so
    /// the analyzer's `Expr::Tuple` typing mints
    /// `GenericInstance(tuple_decl, [T, U])` when this slot is
    /// populated.
    pub tuple_decl: Option<TypeDeclId>,
}

impl WellKnown {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when `id` is one of the node-tag decl handles
    /// (`node`, `nodeTime`, `nodeIndex`, `nodeList`, `nodeGeo`).
    /// Direct replacement for [`greycat_analyzer_types::is_node_tag`]
    /// — handle-keyed rather than string-keyed, so a user-declared
    /// `type node<T>` is not mistaken for the std-core tag.
    pub fn is_node_tag(&self, id: TypeDeclId) -> bool {
        Some(id) == self.node_decl
            || Some(id) == self.node_time_decl
            || Some(id) == self.node_index_decl
            || Some(id) == self.node_list_decl
            || Some(id) == self.node_geo_decl
    }

    /// Stash `id` into the slot matching `name` when `(lib, module)`
    /// is `("std", "core")`. No-op otherwise — a user-defined `node`
    /// in their own module doesn't flow into the well-known slots.
    pub fn record(&mut self, lib: &str, module: &str, name: &str, id: TypeDeclId) {
        if lib != "std" || module != "core" {
            return;
        }
        let slot = match name {
            "bool" => &mut self.bool_decl,
            "char" => &mut self.char_decl,
            "int" => &mut self.int_decl,
            "float" => &mut self.float_decl,
            "String" => &mut self.string_decl,
            "time" => &mut self.time_decl,
            "duration" => &mut self.duration_decl,
            "geo" => &mut self.geo_decl,
            "any" => &mut self.any_decl,
            "null" => &mut self.null_decl,
            "type" => &mut self.type_decl,
            "field" => &mut self.field_decl,
            "function" => &mut self.function_decl,
            "node" => &mut self.node_decl,
            "nodeTime" => &mut self.node_time_decl,
            "nodeIndex" => &mut self.node_index_decl,
            "nodeList" => &mut self.node_list_decl,
            "nodeGeo" => &mut self.node_geo_decl,
            "Array" => &mut self.array_decl,
            "Map" => &mut self.map_decl,
            "Buffer" => &mut self.buffer_decl,
            "Table" => &mut self.table_decl,
            "Tensor" => &mut self.tensor_decl,
            "Tuple" => &mut self.tuple_decl,
            _ => return,
        };
        *slot = Some(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_core::SourceManager;
    use std::str::FromStr;

    /// Synthetic `std/core.gcl` with the well-known native types we
    /// dispatch against. Mirrors the real `lib/std/core.gcl` shape
    /// (`native type` decls in module `core`, lib `std`) without
    /// requiring a `greycat install`.
    fn synthetic_std_core() -> &'static str {
        "native type any {}\n\
         native type null {}\n\
         native type bool {}\n\
         native type int {}\n\
         native type float {}\n\
         native type String {}\n\
         native type time {}\n\
         native type duration {}\n\
         native type geo {}\n\
         native type type {}\n\
         native type field {}\n\
         native type function {}\n\
         native type node<T> {}\n\
         native type nodeTime<T> {}\n\
         native type nodeIndex<K, V> {}\n\
         native type nodeList<T> {}\n\
         native type nodeGeo<T> {}\n\
         native type Array<T> {}\n\
         native type Map<K, V> {}\n\
         native type Buffer {}\n\
         native type Table<T> {}\n\
         native type Tensor {}\n"
    }

    /// After running the project pipeline on a synthetic `std/core`,
    /// every well-known slot should be populated. Guards against the
    /// populate hook missing a slot or the recognizer matching the
    /// wrong `(lib, module)`.
    #[test]
    fn well_known_slots_populated_after_loading_std_core() {
        let mut mgr = SourceManager::new();
        let uri = Uri::from_str("file:///std/core.gcl").unwrap();
        mgr.add_simple(uri, synthetic_std_core(), "std", false);
        let pa = crate::project::ProjectAnalysis::analyze(&mgr);
        let w = &pa.well_known;
        assert!(w.bool_decl.is_some(), "bool slot");
        assert!(w.char_decl.is_none(), "char_decl is not in synthetic core");
        assert!(w.int_decl.is_some(), "int slot");
        assert!(w.float_decl.is_some(), "float slot");
        assert!(w.string_decl.is_some(), "String slot");
        assert!(w.time_decl.is_some(), "time slot");
        assert!(w.duration_decl.is_some(), "duration slot");
        assert!(w.geo_decl.is_some(), "geo slot");
        assert!(w.any_decl.is_some(), "any slot");
        assert!(w.null_decl.is_some(), "null slot");
        assert!(w.type_decl.is_some(), "type slot");
        assert!(w.field_decl.is_some(), "field slot");
        assert!(w.function_decl.is_some(), "function slot");
        assert!(w.node_decl.is_some(), "node slot");
        assert!(w.node_time_decl.is_some(), "nodeTime slot");
        assert!(w.node_index_decl.is_some(), "nodeIndex slot");
        assert!(w.node_list_decl.is_some(), "nodeList slot");
        assert!(w.node_geo_decl.is_some(), "nodeGeo slot");
        assert!(w.array_decl.is_some(), "Array slot");
        assert!(w.map_decl.is_some(), "Map slot");
        assert!(w.buffer_decl.is_some(), "Buffer slot");
        assert!(w.table_decl.is_some(), "Table slot");
        assert!(w.tensor_decl.is_some(), "Tensor slot");
        // node-tag helper agrees with the populated slots.
        let node_id = w.node_decl.unwrap();
        assert!(w.is_node_tag(node_id));
        assert!(w.is_node_tag(w.node_time_decl.unwrap()));
        assert!(!w.is_node_tag(w.array_decl.unwrap()));
    }

    /// A project with no std loaded leaves every slot empty. Guards
    /// against accidental seeding from a non-`std`/`core` source.
    #[test]
    fn well_known_slots_empty_without_std() {
        let mut mgr = SourceManager::new();
        let uri = Uri::from_str("file:///user/app.gcl").unwrap();
        // User-defined `node<T>` in a non-std module must NOT flow
        // into the well-known node slot — that's the soundness
        // guarantee P35 buys us.
        mgr.add_simple(uri, "type node<T> {}\nfn main() {}\n", "userlib", false);
        let pa = crate::project::ProjectAnalysis::analyze(&mgr);
        let w = &pa.well_known;
        assert!(
            w.node_decl.is_none(),
            "user `node` must not occupy the std-core slot"
        );
        assert!(w.int_decl.is_none(), "no std-core int decl seen");
        assert!(w.array_decl.is_none(), "no std-core Array decl seen");
    }

    /// `DeclRegistry::get_or_insert` is idempotent within a single
    /// registry instance.
    #[test]
    fn decl_registry_get_or_insert_is_idempotent() {
        let mut r = DeclRegistry::new();
        let uri = Uri::from_str("file:///x.gcl").unwrap();
        let decl = Idx::<Decl>::from_raw(0u32);
        let a = r.get_or_insert(&uri, decl);
        let b = r.get_or_insert(&uri, decl);
        assert_eq!(a, b);
        assert_eq!(r.len(), 1);
    }
}
