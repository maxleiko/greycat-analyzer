//! Parallelism shim.
//!
//! Single point of dispatch between rayon (on native targets) and a
//! serial fallback (on `wasm32-unknown-unknown`, where `std::thread::spawn`
//! doesn't work without `wasm-bindgen-rayon` + cross-origin-isolation
//! headers — the playground doesn't enable that setup).
//!
//! The dual path lives here, and only here. Every analyzer / CLI seam
//! that wants parallelism calls into one of the helpers below; no other
//! module in the workspace imports `rayon` or carries a
//! `cfg(target_arch = "wasm32")` branch for parallelism.
//!
//! Adding a new shape: write one free function, give it a single
//! `cfg(target_arch)` branch, ship it. The rayon-side branch uses
//! `rayon::prelude::*`; the wasm branch uses the equivalent serial
//! iterator method.

#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;

/// Map an owned `Vec<T>` to `Vec<U>`, in parallel on native and serial
/// on wasm. Closure must be `Sync + Send` so rayon can run it across
/// threads; the wasm branch silently relaxes those bounds at runtime
/// because there's only one thread.
pub fn par_map<T, U, F>(items: Vec<T>, f: F) -> Vec<U>
where
    T: Send,
    U: Send,
    F: Fn(T) -> U + Sync + Send,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        items.into_par_iter().map(f).collect()
    }
    #[cfg(target_arch = "wasm32")]
    {
        items.into_iter().map(f).collect()
    }
}

/// Map a borrowed slice `&[T]` to `Vec<U>`. Same dispatch as
/// [`par_map`] but borrows the input — handy when the caller still
/// needs the source vec after the parallel pass.
pub fn par_map_ref<T, U, F>(items: &[T], f: F) -> Vec<U>
where
    T: Sync,
    U: Send,
    F: Fn(&T) -> U + Sync + Send,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        items.par_iter().map(f).collect()
    }
    #[cfg(target_arch = "wasm32")]
    {
        items.iter().map(f).collect()
    }
}

/// Run `f` on every element of an owned `Vec<T>`. Returns nothing —
/// the closure's side effects (per-thread mutation through `&mut`
/// references it captured) are the whole point. On native this
/// distributes across the rayon work-stealing pool; on wasm it's a
/// plain serial loop.
pub fn par_for_each<T, F>(items: Vec<T>, f: F)
where
    T: Send,
    F: Fn(T) + Sync + Send,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        items.into_par_iter().for_each(f);
    }
    #[cfg(target_arch = "wasm32")]
    {
        items.into_iter().for_each(f);
    }
}
