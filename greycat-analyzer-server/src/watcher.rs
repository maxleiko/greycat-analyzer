// P34.1
//! In-process filesystem watcher glue.
//!
//! The LSP relies on the editor forwarding `workspace/didChangeWatchedFiles`
//! for disk-side changes (notably `greycat install` populating
//! `lib/<name>/...` and writing `lib/installed`). That path is entirely
//! at the mercy of the client: it's gated on `didChangeWatchedFiles`
//! dynamic-registration support, and editors vary in whether they watch
//! gitignored subtrees like `lib/` at all. This module adds a `notify`
//! watcher running inside the server process so disk deltas are picked
//! up uniformly regardless of client — the rust-analyzer model.
//!
//! Split of responsibility:
//! - This module owns the channel types, watcher construction, and the
//!   pure path predicates.
//! - [`crate::backend::Backend`] owns the debounce buffer, the re-stat
//!   classification, the watched-root set, and the dispatch into the
//!   shared `apply_fs_changes` helper (which the editor-driven
//!   `did_change_watched_files` path also funnels through).
//!
//! The watcher only ever forwards raw `notify` events over the channel;
//! all translation happens on the main thread so the watcher callback
//! stays trivial (and never touches `Backend` state).

use std::path::Path;
use std::time::Duration;

use crossbeam_channel::Sender;
use log::{info, warn};
use notify::{RecommendedWatcher, Watcher};

use crate::backend::SERVER_LOG_TAG;

/// Raw watcher payload: whatever `notify` hands the callback. Errors are
/// forwarded too so the main loop can log them (and, in principle, react
/// to a watcher going unhealthy) rather than swallowing them on the
/// watcher thread.
pub type RawFsEvent = notify::Result<notify::Event>;

/// Sender half plumbed into the `notify` callback.
pub type WatchTx = Sender<RawFsEvent>;

/// The concrete platform watcher (`inotify` on Linux, `FSEvents` on
/// macOS, `ReadDirectoryChangesW` on Windows). Held on the `Backend` so
/// its lifetime — and thus the OS watch — matches the server's.
pub type FsWatcher = RecommendedWatcher;

// P34.3
/// Debounce window for coalescing `notify`'s high-frequency event
/// stream into a single batched flush. Sized inside the ROADMAP's
/// 50–100ms band: long enough that a `greycat install` burst (many
/// file writes in quick succession) collapses into one closure reload,
/// short enough that a single external edit feels immediate.
pub const FS_DEBOUNCE_DEFAULT: Duration = Duration::from_millis(75);

/// Spawn the platform watcher, forwarding every event into `tx`.
///
/// Returns `None` when `notify` can't start (CI sandboxes with
/// `inotify` disabled, watch-descriptor exhaustion, unsupported
/// filesystems). The caller treats `None` as "operate as today" — the
/// editor-driven `didChangeWatchedFiles` path stays wired as the
/// fallback, so a failed watcher degrades rather than breaks.
pub fn start_watcher(tx: WatchTx) -> Option<FsWatcher> {
    // The callback runs on `notify`'s own thread; keep it to a bare
    // forward so no `Backend` state is touched off the main thread.
    match notify::recommended_watcher(move |res: RawFsEvent| {
        let _ = tx.send(res);
    }) {
        Ok(w) => {
            info!("[{SERVER_LOG_TAG}][watch] in-process notify watcher started");
            Some(w)
        }
        Err(e) => {
            warn!(
                "[{SERVER_LOG_TAG}][watch] notify watcher failed to start ({e}); \
                 relying on editor-driven didChangeWatchedFiles"
            );
            None
        }
    }
}

/// `true` for a `.gcl` source file.
pub fn is_gcl(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("gcl")
}

/// `true` for `<dir>/lib/installed`, the manifest `greycat install`
/// writes after fetching `@library` deps. Matched by shape (filename +
/// parent dir name) rather than a full path so it works under any
/// project root.
pub fn is_installed_manifest(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some("installed")
        && path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some("lib")
}

/// `true` for any path the watcher cares about — pre-filter applied at
/// buffer time so irrelevant churn (editor temp files, `target/`,
/// `.git/`) never enters the debounce buffer.
pub fn is_relevant(path: &Path) -> bool {
    is_gcl(path) || is_installed_manifest(path)
}

/// Re-establish a watch on `root` (recursive). Thin wrapper so the
/// `notify` import surface stays in this module.
pub fn watch(watcher: &mut FsWatcher, root: &Path) -> notify::Result<()> {
    watcher.watch(root, notify::RecursiveMode::Recursive)
}

/// Drop a watch on `root`. Errors are non-fatal (the path may already
/// be gone) and surfaced to the caller for debug logging.
pub fn unwatch(watcher: &mut FsWatcher, root: &Path) -> notify::Result<()> {
    watcher.unwatch(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn gcl_predicate() {
        assert!(is_gcl(Path::new("/p/a.gcl")));
        assert!(!is_gcl(Path::new("/p/a.rs")));
        assert!(!is_gcl(Path::new("/p/gcl")));
    }

    #[test]
    fn installed_manifest_predicate() {
        assert!(is_installed_manifest(Path::new("/p/lib/installed")));
        // Wrong parent dir.
        assert!(!is_installed_manifest(Path::new("/p/installed")));
        assert!(!is_installed_manifest(Path::new("/p/notlib/installed")));
        // Right name, but a directory deeper.
        assert!(!is_installed_manifest(Path::new("/p/lib/std/installed")));
    }

    #[test]
    fn relevant_is_union() {
        assert!(is_relevant(Path::new("/p/lib/installed")));
        assert!(is_relevant(Path::new("/p/src/a.gcl")));
        assert!(!is_relevant(Path::new("/p/target/x.o")));
    }

    // The watcher must actually deliver events on this platform — a
    // smoke test that `start_watcher` + a recursive watch surface a
    // file write. Polls with a generous budget to tolerate the OS
    // event latency (FSEvents in particular coalesces with a delay).
    #[test]
    fn watcher_delivers_real_fs_event() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let Some(mut watcher) = start_watcher(tx) else {
            // notify couldn't start (e.g. inotify-less sandbox) — the
            // fallback path is exercised elsewhere; nothing to assert.
            return;
        };
        let dir = std::env::temp_dir().join(format!(
            "gca_watch_smoke_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let canonical = dir.canonicalize().unwrap_or(dir.clone());
        watch(&mut watcher, &canonical).unwrap();

        let target = canonical.join("created.gcl");
        std::fs::write(&target, "fn f(): int { return 0; }\n").unwrap();

        // Drain up to ~3s looking for an event naming our file.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut saw = false;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Ok(ev)) => {
                    if ev.paths.iter().any(|p| {
                        p.file_name() == target.file_name()
                            || p.canonicalize().ok().as_deref() == Some(&target)
                    }) {
                        saw = true;
                        break;
                    }
                }
                Ok(Err(_)) => {}
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(saw, "notify watcher should have reported the new .gcl file");
    }

    #[test]
    fn relevant_filters_directory_paths() {
        // A bare directory path (no extension) isn't relevant.
        assert!(!is_relevant(&PathBuf::from("/p/lib")));
    }
}
