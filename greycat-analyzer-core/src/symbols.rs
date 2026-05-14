use std::hash::BuildHasherDefault;

use lasso::{Key, Spur, ThreadedRodeo};
use rustc_hash::FxHasher;

// P19.9
/// A project-wide interned identifier. `Copy`-able 32-bit
/// handle into a [`SymbolTable`]; comparing two `Symbol`s is one
/// integer compare regardless of source string length.
///
/// `Symbol`s are *not* comparable across `SymbolTable` instances
/// each table assigns its own dense numbering. The
/// [`crate::SymbolTable`] that issued a symbol must be the one used
/// to resolve it back to text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Symbol(Spur);

impl Symbol {
    pub fn raw(self) -> u32 {
        self.0.into_usize() as u32
    }
}

// P19.9
/// Append-only string interner. One allocation per unique
/// name across the project lifetime. Hot lookup paths (analyzer body
/// walker, project orchestrator) use `lookup` for read-only checks
/// and `intern` only when extending the index.
#[derive(Debug)]
pub struct SymbolTable {
    rodeo: ThreadedRodeo<Spur, BuildHasherDefault<FxHasher>>,
}

impl Default for SymbolTable {
    #[inline(always)]
    fn default() -> Self {
        Self {
            rodeo: ThreadedRodeo::with_hasher(BuildHasherDefault::<FxHasher>::default()),
        }
    }
}

impl SymbolTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `name`. Idempotent — the second call with the same
    /// `name` returns the same [`Symbol`] without allocating.
    pub fn intern(&self, name: &str) -> Symbol {
        Symbol(self.rodeo.get_or_intern(name))
    }

    /// Read-only lookup: returns the existing [`Symbol`] for `name`
    /// or `None` if no one has interned it yet. Use this in hot
    /// lookup paths where adding a stale entry would be incorrect.
    pub fn lookup(&self, name: &str) -> Option<Symbol> {
        self.rodeo.get(name).map(Symbol)
    }

    /// Resolve `sym` back to its text.
    pub fn resolve(&self, sym: &Symbol) -> &str {
        self.rodeo.resolve(&sym.0)
    }

    pub fn len(&self) -> usize {
        self.rodeo.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rodeo.is_empty()
    }
}

impl std::ops::Index<Symbol> for SymbolTable {
    type Output = str;

    fn index(&self, index: Symbol) -> &Self::Output {
        self.rodeo.resolve(&index.0)
    }
}
