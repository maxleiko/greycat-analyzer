use greycat_analyzer_core::ItemKey;
use rustc_hash::FxHashMap;

use crate::{arena::Idx, hir::Decl};

/// Maps every interned [`ItemKey`] to the current HIR's `Idx<Decl>` in
/// the owning module. The `Idx<Decl>` is HIR-allocation-order — a
/// property of the *current* lower, not of the decl — so it gets
/// refreshed on every `record` call (which happens once per decl per
/// ingest). The URI of the owning module isn't stored here; recover
/// it via `ProjectIndex::module_names[item.module]`.
#[derive(Debug, Default, Clone)]
#[repr(transparent)]
pub struct DeclRegistry(FxHashMap<ItemKey, Idx<Decl>>);

impl DeclRegistry {
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Idempotent on `item` — re-calling with the same `ItemKey`
    /// refreshes the cached `Idx<Decl>` so [`Self::lookup`] stays
    /// valid against the most recently-ingested HIR.
    #[inline]
    pub fn record(&mut self, item: ItemKey, decl: Idx<Decl>) {
        self.0.insert(item, decl);
    }

    /// Current `Idx<Decl>` for `item` in its owning module's HIR.
    /// Only meaningful against the most recently-ingested HIR for
    /// `item.module`.
    #[inline]
    pub fn lookup(&self, item: ItemKey) -> Option<Idx<Decl>> {
        self.0.get(&item).copied()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
