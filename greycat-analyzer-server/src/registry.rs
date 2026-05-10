//! Native [`RegistryFetcher`] backed by `ureq` (P15.3).
//!
//! Drops into [`greycat_analyzer_core::registry::CachingFetcher`] to
//! provide on-disk-free caching and parallel branch fan-out for
//! `@library` version completion. The WASM bridge plugs in a
//! different fetcher (a JS-side callback); the trait abstraction
//! keeps both backings interchangeable.
//!
//! Parallelism: [`UreqFetcher::fetch_many`] spawns one short-lived
//! `std::thread` per URL and joins them. The TS reference awaits
//! sequentially — most of the latency users see comes from that
//! serialization, not from a single round-trip. Threads are cheap
//! relative to the wall-clock cost of a TLS handshake; a per-call
//! pool would only matter at request rates we'll never hit from a
//! completion popup.

use std::sync::Arc;
use std::time::Duration;

use greycat_analyzer_core::registry::{CachingFetcher, RegistryFetcher, RegistryItem};

/// HTTP timeout for a single registry listing request. Conservative
/// enough to ride out a slow handshake without leaving the editor's
/// completion popup hanging if the registry is offline.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Cache TTL. Five minutes is long enough that a typical editing
/// session gets one cold walk and many warm hits, but short enough
/// that a freshly-published version surfaces without restarting the
/// LSP server.
const CACHE_TTL: Duration = Duration::from_secs(300);

/// Build the production registry fetcher: `ureq` wrapped in the
/// shared TTL cache. Returned as an `Arc<dyn>` so the [`Backend`]
/// can share it with every completion handler thread.
///
/// [`Backend`]: crate::backend::Backend
pub fn shared() -> Arc<dyn RegistryFetcher> {
    Arc::new(CachingFetcher::new(UreqFetcher::default(), CACHE_TTL))
}

/// `ureq`-backed [`RegistryFetcher`]. Constructed once and shared —
/// `ureq::Agent` reuses connections, which matters when we fan out
/// 50+ requests across the per-major.minor file listings.
pub struct UreqFetcher {
    agent: ureq::Agent,
}

impl Default for UreqFetcher {
    fn default() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl RegistryFetcher for UreqFetcher {
    fn fetch(&self, url: &str) -> Vec<RegistryItem> {
        match fetch_one(&self.agent, url) {
            Ok(items) => items,
            Err(e) => {
                log::debug!("[registry] fetch {url} failed: {e}");
                Vec::new()
            }
        }
    }

    fn fetch_many(&self, urls: &[String]) -> Vec<Vec<RegistryItem>> {
        // Sequential below the threshold: one round-trip is usually
        // cheaper than spinning up a thread.
        if urls.len() <= 1 {
            return urls.iter().map(|u| self.fetch(u)).collect();
        }
        let mut handles = Vec::with_capacity(urls.len());
        for url in urls {
            let agent = self.agent.clone();
            let url = url.clone();
            handles.push(std::thread::spawn(move || {
                if url.is_empty() {
                    return Vec::new();
                }
                fetch_one(&agent, &url).unwrap_or_else(|e| {
                    log::debug!("[registry] fetch {url} failed: {e}");
                    Vec::new()
                })
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect()
    }
}

fn fetch_one(agent: &ureq::Agent, url: &str) -> std::io::Result<Vec<RegistryItem>> {
    let body: String = agent
        .get(url)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| std::io::Error::other(format!("http: {e}")))?
        .body_mut()
        .read_to_string()
        .map_err(|e| std::io::Error::other(format!("body: {e}")))?;
    let items: Vec<RegistryItem> =
        serde_json::from_str(&body).map_err(|e| std::io::Error::other(format!("json: {e}")))?;
    Ok(items)
}
