// P15.3
//! GreyCat registry walker.
//!
//! Drives the version-listing dance for `@library("name", "<cursor>")`
//! completion. The HTTP client lives behind a [`RegistryFetcher`] trait
//! so this crate stays I/O-free — the LSP server provides a real HTTP
//! impl, the WASM bridge can plug in a JS-side callback, and tests stub
//! the trait with scripted responses.
//!
//! The registry layout is a directory tree under
//! `https://get.greycat.io/files/<lib>/`:
//!
//! ```text
//! files/<lib>/
//!   <branch>/                    # stable, dev, testing, feature-branches…
//!     latest                     # not a directory — skipped
//!     <major.minor>/              # 7.8, 8.0, …
//!       <arch>/                  # x64-linux preferred, noarch fallback
//!         <version>.zip          # 7.8.166-stable.zip, 8.0.12-dev.zip, …
//! ```
//!
//! `Accept: application/json` makes the server return a flat array of
//! `{path, size, last_modification}` per directory level. The *path*
//! field is the full path with a trailing `/` for directories.
//!
//! Improvements over the TS reference (`packages/server/src/registry.ts`):
//!
//! - **Caching** at every level via [`CachingFetcher`] with a TTL —
//!   repeat completions in the same session are O(1) without any
//!   HTTP traffic. The TS reference re-fetches on every keystroke.
//! - **Parallelism** via [`RegistryFetcher::fetch_many`] — branch
//!   listings, per-major.minor file lists, and the arch probe all fan
//!   out concurrently in the native impl. The TS awaits each step
//!   serially, which dominates latency.
//! - **Single arch probe** — when both `x64-linux` and `noarch` exist
//!   we pick `x64-linux`; when neither exists we skip the branch
//!   instead of cascading the failure into the per-major.minor
//!   listing loop.
//!
//! Channel-aware filtering (`-dev` / `-beta` / …) lives one layer up
//! in the LSP completion path, since it depends on what the user has
//! already typed.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use serde::Deserialize;

const REGISTRY_BASE: &str = "https://get.greycat.io/files";

/// One row of a registry directory listing. The server returns these
/// inside a flat JSON array when `Accept: application/json` is set.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryItem {
    /// Full path including the lib name, with a trailing `/` for
    /// directories. Examples: `core/stable/`, `core/stable/7.8/`,
    /// `core/stable/7.8/x64-linux/7.8.166-stable.zip`.
    pub path: String,
    /// `null` for directories, byte size for files.
    #[serde(default)]
    pub size: Option<u64>,
    /// ISO-8601 UTC timestamp.
    pub last_modification: String,
}

/// Pluggable HTTP backing for the registry walker. Implementors must
/// be `Send + Sync` so a single fetcher instance can serve concurrent
/// completion requests on the LSP server.
pub trait RegistryFetcher: Send + Sync {
    /// Fetch one directory listing. Returns the parsed array on
    /// success, an empty `Vec` on any error (network, JSON parse,
    /// non-2xx). Errors aren't surfaced — completion just degrades
    /// to the empty list and the user sees no suggestions.
    fn fetch(&self, url: &str) -> Vec<RegistryItem>;

    /// Fetch a batch of URLs. Default impl is sequential; native
    /// fetchers should override with a thread-pool fan-out so the
    /// per-major.minor file listings don't dominate latency. The
    /// returned vector matches the input order.
    fn fetch_many(&self, urls: &[String]) -> Vec<Vec<RegistryItem>> {
        urls.iter().map(|u| self.fetch(u)).collect()
    }
}

/// One concrete version row surfaced to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibVersion {
    /// Version string without the `.zip` suffix, e.g. `7.8.166-stable`.
    pub text: String,
    /// ISO-8601 UTC timestamp from the registry listing.
    pub last_modification: String,
}

/// Look up every published version of `name` by walking the registry.
///
/// `std` is aliased to `core` at the registry root — this is a
/// historical naming quirk from when the standard library was renamed
/// in-language but not in the file layout.
///
/// Returns versions in semver-descending order (newest first), with
/// the prerelease tag breaking ties lexicographically. Branches that
/// fail to list (network error, malformed JSON, no arch dir) are
/// skipped silently — completion shows whatever survived.
pub fn get_lib_versions(name: &str, fetcher: &dyn RegistryFetcher) -> Vec<LibVersion> {
    let alias = if name == "std" { "core" } else { name };
    let base = format!("{REGISTRY_BASE}/{alias}/");
    let branch_items = fetcher.fetch(&base);
    let branches: Vec<String> = branch_items
        .iter()
        .filter_map(|i| trailing_dir_name(&i.path))
        .map(str::to_string)
        .collect();
    if branches.is_empty() {
        return Vec::new();
    }

    // Stage 1: list the major.minor dirs for every branch in parallel.
    let branch_urls: Vec<String> = branches.iter().map(|b| format!("{base}{b}/")).collect();
    let branch_listings = fetcher.fetch_many(&branch_urls);

    // Stage 2: probe arch (x64-linux vs noarch) once per branch using
    // the first major.minor as the probe target, then collect every
    // (branch, major.minor, arch) URL we'll ask for zips. Single-pass:
    // the per-arch probe URLs and the per-major.minor file listing
    // URLs are resolved together in one parallel fan-out.
    let mut per_branch_minors: Vec<(String, Vec<String>)> = Vec::with_capacity(branches.len());
    for (branch, listing) in branches.iter().zip(branch_listings.iter()) {
        let minors = listing
            .iter()
            .filter_map(|i| trailing_dir_name(&i.path))
            .filter(|n| n.bytes().next().is_some_and(|b| b.is_ascii_digit()))
            .map(str::to_string)
            .collect::<Vec<_>>();
        per_branch_minors.push((branch.clone(), minors));
    }

    // Probe arch in parallel — for each branch, ask both `x64-linux/`
    // and `noarch/` of the first major.minor concurrently. The TS does
    // this serially; one round-trip per branch instead of two.
    let mut probe_urls: Vec<String> = Vec::with_capacity(per_branch_minors.len() * 2);
    for (branch, minors) in &per_branch_minors {
        if let Some(first) = minors.first() {
            probe_urls.push(format!("{base}{branch}/{first}/x64-linux/"));
            probe_urls.push(format!("{base}{branch}/{first}/noarch/"));
        } else {
            // Push two empty placeholders so the index math below stays
            // aligned with `per_branch_minors`.
            probe_urls.push(String::new());
            probe_urls.push(String::new());
        }
    }
    let probe_results = fetcher.fetch_many(&probe_urls);

    let mut zip_urls: Vec<(usize, String)> = Vec::new();
    for (idx, (branch, minors)) in per_branch_minors.iter().enumerate() {
        let x64 = &probe_results[idx * 2];
        let noarch = &probe_results[idx * 2 + 1];
        let arch = if !x64.is_empty() {
            "x64-linux"
        } else if !noarch.is_empty() {
            "noarch"
        } else {
            continue;
        };
        for minor in minors {
            zip_urls.push((idx, format!("{base}{branch}/{minor}/{arch}/")));
        }
    }

    // Stage 3: list every `<branch>/<minor>/<arch>/` directory in
    // parallel. The flat URL list lets the fetcher saturate its
    // thread pool across branches, which is where the TS reference
    // wastes most of its latency.
    let urls_only: Vec<String> = zip_urls.iter().map(|(_, u)| u.clone()).collect();
    let zip_listings = fetcher.fetch_many(&urls_only);

    let mut versions = Vec::<LibVersion>::new();
    for listing in zip_listings {
        for item in listing {
            if let Some(file_name) = item.path.rsplit('/').next()
                && let Some(version) = file_name.strip_suffix(".zip")
            {
                versions.push(LibVersion {
                    text: version.to_string(),
                    last_modification: item.last_modification,
                });
            }
        }
    }
    sort_versions_desc(&mut versions);
    versions
}

/// Extract the final non-empty path segment from a registry path like
/// `core/stable/` → `stable`, or `core/stable/7.8/` → `7.8`. Returns
/// `None` for non-directory entries (no trailing `/`) and for empty
/// paths.
fn trailing_dir_name(path: &str) -> Option<&str> {
    let stripped = path.strip_suffix('/')?;
    let last = stripped.rsplit('/').next()?;
    if last.is_empty() { None } else { Some(last) }
}

/// Semver-descending comparator. Handles the standard `M.m.p[-suffix]`
/// shape registry entries use; falls back to lexicographic comparison
/// when a version doesn't parse (rare but cheap to defend against).
fn sort_versions_desc(versions: &mut [LibVersion]) {
    versions.sort_by(|a, b| {
        let ap = parse_semver(&a.text);
        let bp = parse_semver(&b.text);
        match (ap, bp) {
            (Some(a), Some(b)) => b.cmp(&a),
            // Parsed entries sort before non-parsed entries (standard
            // Rust ordering would put `Some` before `None` already in
            // ascending; we want parsed first in descending too).
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => b.text.cmp(&a.text),
        }
    });
}

/// Parsed semver tuple used by [`sort_versions_desc`]. `Ord` derives
/// the natural numeric-major / numeric-minor / numeric-patch /
/// lex-suffix ordering.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Semver {
    major: u32,
    minor: u32,
    patch: u32,
    /// Empty string for release versions; sorted lexicographically
    /// for prereleases. `"-stable"` > `"-dev"` lexicographically.
    suffix: String,
}

fn parse_semver(s: &str) -> Option<Semver> {
    let (core, suffix) = match s.split_once('-') {
        Some((c, rest)) => (c, rest.to_string()),
        None => (s, String::new()),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Semver {
        major,
        minor,
        patch,
        suffix,
    })
}

/// Extract the prerelease tag from a partially-typed version string
/// like `8.0.0-dev` → `Some("dev")`. Returns `None` when the user
/// hasn't typed a `-` yet, in which case all versions are surfaced.
pub fn prerelease_tag(text: &str) -> Option<&str> {
    let (_, suffix) = text.split_once('-')?;
    if suffix.is_empty() {
        None
    } else {
        Some(suffix)
    }
}

// =============================================================================
// Caching wrapper
// =============================================================================

/// Wraps any [`RegistryFetcher`] with a per-URL TTL cache. The TS
/// reference has no caching at all — every keystroke re-walks the
/// whole tree, which is most of the perceived latency. With caching,
/// repeated completions in the same session are free.
///
/// Cache entries expire after [`CachingFetcher::ttl`]. Concurrent
/// requests for the same URL aren't deduplicated (no in-flight
/// joining) — the simpler `RwLock<HashMap>` keeps the impl small and
/// the duplicate work amortizes to zero after the first hit.
pub struct CachingFetcher<F> {
    inner: F,
    cache: RwLock<HashMap<String, (Instant, Vec<RegistryItem>)>>,
    ttl: Duration,
}

impl<F: RegistryFetcher> CachingFetcher<F> {
    pub fn new(inner: F, ttl: Duration) -> Self {
        Self {
            inner,
            cache: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    fn cached(&self, url: &str) -> Option<Vec<RegistryItem>> {
        let now = Instant::now();
        let guard = self.cache.read().ok()?;
        let (ts, items) = guard.get(url)?;
        if now.duration_since(*ts) < self.ttl {
            Some(items.clone())
        } else {
            None
        }
    }

    fn store(&self, url: &str, items: Vec<RegistryItem>) {
        if let Ok(mut guard) = self.cache.write() {
            guard.insert(url.to_string(), (Instant::now(), items));
        }
    }
}

impl<F: RegistryFetcher> RegistryFetcher for CachingFetcher<F> {
    fn fetch(&self, url: &str) -> Vec<RegistryItem> {
        if let Some(hit) = self.cached(url) {
            return hit;
        }
        let items = self.inner.fetch(url);
        self.store(url, items.clone());
        items
    }

    fn fetch_many(&self, urls: &[String]) -> Vec<Vec<RegistryItem>> {
        // Split into cache hits (resolved up front) and misses
        // (delegated to the inner fetcher in one batch). This keeps
        // the inner pool's parallelism focused on URLs that actually
        // need network work.
        let mut results: Vec<Option<Vec<RegistryItem>>> = vec![None; urls.len()];
        let mut miss_idx: Vec<usize> = Vec::new();
        let mut miss_urls: Vec<String> = Vec::new();
        for (i, url) in urls.iter().enumerate() {
            if let Some(hit) = self.cached(url) {
                results[i] = Some(hit);
            } else {
                miss_idx.push(i);
                miss_urls.push(url.clone());
            }
        }
        if !miss_urls.is_empty() {
            let fetched = self.inner.fetch_many(&miss_urls);
            for (i, items) in miss_idx.into_iter().zip(fetched) {
                self.store(&urls[i], items.clone());
                results[i] = Some(items);
            }
        }
        results.into_iter().map(Option::unwrap_or_default).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Scripted fetcher: maps URL -> JSON string. `fetch` parses the
    /// JSON; missing URLs return an empty `Vec`.
    struct StubFetcher {
        scripted: HashMap<String, &'static str>,
        calls: Mutex<Vec<String>>,
    }

    impl StubFetcher {
        fn new(pairs: &[(&'static str, &'static str)]) -> Self {
            Self {
                scripted: pairs.iter().map(|(u, j)| ((*u).to_string(), *j)).collect(),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl RegistryFetcher for StubFetcher {
        fn fetch(&self, url: &str) -> Vec<RegistryItem> {
            self.calls.lock().unwrap().push(url.to_string());
            let json = match self.scripted.get(url) {
                Some(j) => *j,
                None => return Vec::new(),
            };
            serde_json::from_str(json).unwrap_or_default()
        }
    }

    #[test]
    fn parses_semver_with_suffix() {
        let v = parse_semver("7.8.166-stable").unwrap();
        assert_eq!(v.major, 7);
        assert_eq!(v.minor, 8);
        assert_eq!(v.patch, 166);
        assert_eq!(v.suffix, "stable");

        let v = parse_semver("8.0.12").unwrap();
        assert_eq!(v.suffix, "");

        assert!(parse_semver("not-a-version").is_none());
        assert!(parse_semver("7.8").is_none());
        assert!(parse_semver("7.8.1.2").is_none());
    }

    #[test]
    fn sort_descending_by_semver_then_suffix() {
        let mut v = vec![
            LibVersion {
                text: "7.8.10-dev".into(),
                last_modification: "".into(),
            },
            LibVersion {
                text: "8.0.0-stable".into(),
                last_modification: "".into(),
            },
            LibVersion {
                text: "7.8.166-stable".into(),
                last_modification: "".into(),
            },
            LibVersion {
                text: "7.8.166-dev".into(),
                last_modification: "".into(),
            },
        ];
        sort_versions_desc(&mut v);
        let texts: Vec<&str> = v.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "8.0.0-stable",
                "7.8.166-stable",
                "7.8.166-dev",
                "7.8.10-dev",
            ]
        );
    }

    #[test]
    fn prerelease_tag_extraction() {
        assert_eq!(prerelease_tag(""), None);
        assert_eq!(prerelease_tag("8.0.0"), None);
        assert_eq!(prerelease_tag("8.0.0-dev"), Some("dev"));
        assert_eq!(prerelease_tag("8-stable"), Some("stable"));
    }

    #[test]
    fn get_lib_versions_walks_full_tree() {
        let stub = StubFetcher::new(&[
            (
                "https://get.greycat.io/files/core/",
                r#"[{"path":"core/stable/","size":null,"last_modification":"2026-04-09T00:00:00Z"},
                    {"path":"core/dev/","size":null,"last_modification":"2026-04-09T00:00:00Z"}]"#,
            ),
            (
                "https://get.greycat.io/files/core/stable/",
                r#"[{"path":"core/stable/latest","size":18,"last_modification":"2026-04-09T00:00:00Z"},
                    {"path":"core/stable/7.8/","size":null,"last_modification":"2026-04-09T00:00:00Z"}]"#,
            ),
            (
                "https://get.greycat.io/files/core/dev/",
                r#"[{"path":"core/dev/8.0/","size":null,"last_modification":"2026-04-09T00:00:00Z"}]"#,
            ),
            (
                "https://get.greycat.io/files/core/stable/7.8/x64-linux/",
                r#"[{"path":"core/stable/7.8/x64-linux/7.8.166-stable.zip","size":1,"last_modification":"2026-04-09T00:00:00Z"}]"#,
            ),
            (
                "https://get.greycat.io/files/core/stable/7.8/noarch/",
                r#"[]"#,
            ),
            (
                "https://get.greycat.io/files/core/dev/8.0/x64-linux/",
                r#"[{"path":"core/dev/8.0/x64-linux/8.0.0-dev.zip","size":1,"last_modification":"2026-04-09T00:00:00Z"},
                    {"path":"core/dev/8.0/x64-linux/8.0.5-dev.zip","size":1,"last_modification":"2026-04-09T00:00:00Z"}]"#,
            ),
            ("https://get.greycat.io/files/core/dev/8.0/noarch/", r#"[]"#),
        ]);
        let v = get_lib_versions("core", &stub);
        let texts: Vec<&str> = v.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, vec!["8.0.5-dev", "8.0.0-dev", "7.8.166-stable"]);
    }

    #[test]
    fn std_aliases_to_core() {
        let stub = StubFetcher::new(&[(
            "https://get.greycat.io/files/core/",
            // Empty branches list — we only care that the URL was
            // rewritten to `core/`, not `std/`.
            r#"[]"#,
        )]);
        let _ = get_lib_versions("std", &stub);
        let calls = stub.calls();
        assert_eq!(calls, vec!["https://get.greycat.io/files/core/"]);
    }

    #[test]
    fn caching_skips_repeat_fetch() {
        let inner = StubFetcher::new(&[(
            "https://get.greycat.io/files/foo/",
            r#"[{"path":"foo/stable/","size":null,"last_modification":""}]"#,
        )]);
        let cache = CachingFetcher::new(inner, Duration::from_secs(60));
        let _ = cache.fetch("https://get.greycat.io/files/foo/");
        let _ = cache.fetch("https://get.greycat.io/files/foo/");
        let calls = cache.inner.calls();
        assert_eq!(calls.len(), 1, "second fetch should hit the cache");
    }
}
