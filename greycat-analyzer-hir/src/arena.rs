//! Tiny typed arena. Each `Arena<T>` is a `Vec<T>` indexed by a typed
//! `Idx<T>` newtype.

use std::marker::PhantomData;
use std::ops::{Index, IndexMut};

/// Stable index into an [`Arena`]. `Copy` + `Eq` regardless of `T`.
pub struct Idx<T> {
    raw: u32,
    _phantom: PhantomData<fn() -> T>,
}

impl<T> Idx<T> {
    pub const fn from_raw(raw: u32) -> Self {
        Self {
            raw,
            _phantom: PhantomData,
        }
    }
    pub const fn into_raw(self) -> u32 {
        self.raw
    }
}

impl<T> Clone for Idx<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Idx<T> {}
impl<T> PartialEq for Idx<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl<T> Eq for Idx<T> {}
impl<T> std::hash::Hash for Idx<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}
impl<T> std::fmt::Debug for Idx<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Idx({})", self.raw)
    }
}

/// Append-only typed arena.
#[derive(Debug)]
pub struct Arena<T> {
    items: Vec<T>,
}

impl<T> Arena<T> {
    pub const fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn alloc(&mut self, item: T) -> Idx<T> {
        let raw = self.items.len() as u32;
        self.items.push(item);
        Idx::from_raw(raw)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (Idx<T>, &T)> {
        self.items
            .iter()
            .enumerate()
            .map(|(i, v)| (Idx::from_raw(i as u32), v))
    }
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Index<Idx<T>> for Arena<T> {
    type Output = T;
    fn index(&self, idx: Idx<T>) -> &T {
        &self.items[idx.raw as usize]
    }
}

impl<T> IndexMut<Idx<T>> for Arena<T> {
    fn index_mut(&mut self, idx: Idx<T>) -> &mut T {
        &mut self.items[idx.raw as usize]
    }
}
