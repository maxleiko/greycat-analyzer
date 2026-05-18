// P35.1
//! Project-wide stable handles for resolved type decls and the
//! "well-known" std/core slots the analyzer dispatches against.
//!
//! Decl identity is the [`ItemId`] `(module_sym, name_sym)` pair —
//! globally unique per project because module names are unique (the
//! [`ProjectIndex::duplicate_modules`] gate enforces it at ingest).
//! Two `ItemId`s compare equal iff they refer to the same item in the
//! same module; a user-declared `type node<T>` and the std-core
//! `node<T>` therefore get distinct identities, closing the soundness
//! gap where the previous SmolStr-keyed identity collapsed them.
//!
//! - [`DeclRegistry`] maps `ItemId → Idx<Decl>` so consumers holding
//!   a type-system handle can navigate back to the source `Decl` in
//!   the owning module's HIR. Refreshed on every ingest so the cached
//!   `Idx<Decl>` stays valid against the current HIR.
//! - [`WellKnown`] holds one `Option<ItemId>` slot per native type the
//!   analyzer special-cases (`node`, `Array`, `function`, etc.).
//!   Populated as decls flow through ingest; a `Decl::Type` whose
//!   `(module.lib, module.name, decl_name)` matches
//!   `("std", "core", N)` stashes its identity into slot `N`.

use greycat_analyzer_core::ItemId;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::Decl;
use rustc_hash::FxHashMap;

/// Maps every interned [`ItemId`] to the current HIR's `Idx<Decl>` in
/// the owning module. The `Idx<Decl>` is HIR-allocation-order — a
/// property of the *current* lower, not of the decl — so it gets
/// refreshed on every `record` call (which happens once per decl per
/// ingest). The URI of the owning module isn't stored here; recover
/// it via `ProjectIndex::module_names[item.module]`.
#[derive(Debug, Default, Clone)]
pub struct DeclRegistry {
    indices: FxHashMap<ItemId, Idx<Decl>>,
}

impl DeclRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Idempotent on `item` — re-calling with the same `ItemId`
    /// refreshes the cached `Idx<Decl>` so [`Self::lookup`] stays
    /// valid against the most recently-ingested HIR.
    pub fn record(&mut self, item: ItemId, decl: Idx<Decl>) {
        self.indices.insert(item, decl);
    }

    /// Current `Idx<Decl>` for `item` in its owning module's HIR.
    /// Only meaningful against the most recently-ingested HIR for
    /// `item.module`.
    pub fn lookup(&self, item: ItemId) -> Option<Idx<Decl>> {
        self.indices.get(&item).copied()
    }

    pub fn len(&self) -> usize {
        self.indices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

/// Well-known std/core type decl identities. Each slot is `Some` once
/// the corresponding `Decl::Type` has been seen during ingest;
/// `None` when std hasn't been loaded yet (the
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
    // identities let cross-module references know they're talking
    // about the std-core decl specifically, not an unrelated user-
    // defined type that happens to share the name.
    pub bool_decl: Option<ItemId>,
    pub char_decl: Option<ItemId>,
    pub int_decl: Option<ItemId>,
    pub float_decl: Option<ItemId>,
    pub string_decl: Option<ItemId>,
    pub time_decl: Option<ItemId>,
    pub duration_decl: Option<ItemId>,
    pub geo_decl: Option<ItemId>,

    // Top / bottom equivalents — `any` and `null` are also declared
    // as `native type` in std/core.
    pub any_decl: Option<ItemId>,
    pub null_decl: Option<ItemId>,

    // Runtime sentinels — `type`, `field`, `function`. The
    // `function_ty()` / `type_ty()` / `field_ty()` minter sites in
    // [`crate::analyzer`] read from these identities.
    pub type_decl: Option<ItemId>,
    pub field_decl: Option<ItemId>,
    pub function_decl: Option<ItemId>,

    // Node-tag generics — the auto-deref family.
    // [`Self::is_node_tag`] is the comparison primitive.
    pub node_decl: Option<ItemId>,
    pub node_time_decl: Option<ItemId>,
    pub node_index_decl: Option<ItemId>,
    pub node_list_decl: Option<ItemId>,
    pub node_geo_decl: Option<ItemId>,

    // Common generic collections.
    pub array_decl: Option<ItemId>,
    pub map_decl: Option<ItemId>,
    pub buffer_decl: Option<ItemId>,
    pub table_decl: Option<ItemId>,
    pub tensor_decl: Option<ItemId>,
    /// `Tuple<T, U>` from `lib/std/core.gcl`. `(x, y)` tuple-literal
    /// syntax desugars to `Tuple<T, U>{x, y}` per the compiler, so
    /// the analyzer's `Expr::Tuple` typing mints
    /// `Generic(tuple_decl, [T, U])` when this slot is populated.
    pub tuple_decl: Option<ItemId>,
}

impl WellKnown {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when `id` is one of the node-tag decl identities
    /// (`node`, `nodeTime`, `nodeIndex`, `nodeList`, `nodeGeo`).
    /// Direct replacement for the SmolStr-keyed predicate this
    /// crate used to expose — handle-keyed, so a user-declared
    /// `type node<T>` is not mistaken for the std-core tag.
    pub fn is_node_tag(&self, id: ItemId) -> bool {
        Some(id) == self.node_decl
            || Some(id) == self.node_time_decl
            || Some(id) == self.node_index_decl
            || Some(id) == self.node_list_decl
            || Some(id) == self.node_geo_decl
    }

    /// Stash `id` into the slot matching `name` when `(lib, module)`
    /// is `("std", "core")`. No-op otherwise — a user-defined `node`
    /// in their own module doesn't flow into the well-known slots.
    pub fn record(&mut self, lib: &str, module: &str, name: &str, id: ItemId) {
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
    use greycat_analyzer_core::lsp_types::Uri;
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

    /// `DeclRegistry::record` is idempotent: re-recording the same
    /// `ItemId` refreshes the cached `Idx<Decl>` without bloating the
    /// map.
    #[test]
    fn decl_registry_record_is_idempotent() {
        use greycat_analyzer_core::SymbolTable;
        let mut r = DeclRegistry::new();
        let decl = Idx::<Decl>::from_raw(0u32);
        let symbols = SymbolTable::new();
        let item = ItemId::new(symbols.intern("m"), symbols.intern("Foo"));
        r.record(item, decl);
        r.record(item, decl);
        assert_eq!(r.lookup(item), Some(decl));
        assert_eq!(r.len(), 1);
    }
}
