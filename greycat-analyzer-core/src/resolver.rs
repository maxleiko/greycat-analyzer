//! Module / library / include resolution and the filesystem abstraction
//! the rest of the analyzer reaches I/O through.
//!
//! Ported from the TS `packages/resolver/` (~92 LoC) plus the path-math
//! helpers and `lib/installed` parser scattered across
//! `packages/lang/src/project/analyze.ts` and `module_desc.ts`.
//!
//! Path math is pure (no I/O, no env reads) and lives at the top of the
//! module. The [`Context`] trait abstracts the filesystem so the analyzer
//! can run against either a real fs ([`FsContext`]) or an in-memory mock
//! (for wasm and tests).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

// =============================================================================
// Path math (pure, no I/O)
// =============================================================================

/// Resolve `@library("<name>", ...)` to its candidate directory under the
/// project. Mirrors `path.join(project_dir, 'lib', name)` from the TS port.
pub fn library_dir(project_dir: &Path, name: &str) -> PathBuf {
    project_dir.join("lib").join(name)
}

/// Resolve the global `std` directory under a GreyCat home.
/// Used as the fallback when a project doesn't ship a local `lib/std/`.
pub fn global_std_dir(greycat_home: &Path) -> PathBuf {
    greycat_home.join("lib").join("std")
}

/// Resolve `@include("<rel>")` against a project directory.
/// `rel` is taken verbatim — TS path.join treats it the same as a relative
/// path, with no traversal sanitization.
pub fn include_dir(project_dir: &Path, rel: &str) -> PathBuf {
    project_dir.join(rel)
}

/// Path to `<project_dir>/lib/installed`, the manifest `greycat install`
/// writes after fetching `@library` deps.
pub fn installed_file_path(project_dir: &Path) -> PathBuf {
    project_dir.join("lib").join("installed")
}

// =============================================================================
// `lib/installed` parser
// =============================================================================

/// Parsed view of `<project_dir>/lib/installed`. Each entry is `name → Some(version)`
/// for `name=version` lines, or `name → None` for `name=` lines (TS treats an
/// empty version as a sentinel meaning "installed but version-less"; we mirror).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Installed {
    pub entries: BTreeMap<String, Option<String>>,
}

/// Parse a `lib/installed` file body. Malformed lines (no `=`) are skipped
/// silently — same behavior as TS, which has a `TODO show the user the error`.
pub fn parse_installed_file(content: &str) -> Installed {
    let mut entries = BTreeMap::new();
    for line in content.trim().split('\n') {
        if line.is_empty() {
            continue;
        }
        let Some((name, version)) = line.split_once('=') else {
            // Malformed — skip.
            continue;
        };
        let v = if version.is_empty() {
            None
        } else {
            Some(version.to_string())
        };
        entries.insert(name.to_string(), v);
    }
    Installed { entries }
}

// =============================================================================
// `greycat_home` resolution
// =============================================================================

/// Errors produced when resolving the GreyCat home directory.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HomeError {
    #[error("environment variable GREYCAT_HOME must be absolute, got '{0}'")]
    NotAbsolute(String),
    #[error("cannot resolve GreyCat home: neither $GREYCAT_HOME nor $HOME is set")]
    NoHome,
}

/// Pure resolver: testable without env mutation. If `gc_home_env` is set,
/// it must be absolute. Otherwise fall back to `<home>/.greycat`.
fn resolve_greycat_home(
    gc_home_env: Option<&str>,
    home: Option<&Path>,
) -> Result<PathBuf, HomeError> {
    if let Some(env) = gc_home_env {
        let p = PathBuf::from(env);
        if !p.is_absolute() {
            return Err(HomeError::NotAbsolute(env.to_string()));
        }
        return Ok(p);
    }
    let home = home.ok_or(HomeError::NoHome)?;
    Ok(home.join(".greycat"))
}

/// Read `$GREYCAT_HOME` (or fall back to `$HOME/.greycat`) and return the
/// resolved path. Mirrors the TS `greycatHome()` semantics.
pub fn try_greycat_home() -> Result<PathBuf, HomeError> {
    let env = std::env::var("GREYCAT_HOME").ok();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_greycat_home(env.as_deref(), home.as_deref())
}

// =============================================================================
// `Context` trait + fs-backed impl
// =============================================================================

/// Filesystem abstraction the analyzer reaches I/O through. Sync — the
/// analyzer doesn't need async I/O, and an `async fn` in trait would push
/// `Pin<Box<…>>` noise into every call site.
///
/// In-memory implementations (wasm playground, tests) can stub `read` from
/// a map and `iter_gcl` from the map's keys.
pub trait Context {
    /// Read `path` as UTF-8.
    fn read(&self, path: &Path) -> io::Result<String>;

    /// All `.gcl` files reachable under `dir`, recursively. Implementations
    /// must skip `node_modules/`, `gcdata/`, `.git/` to match TS glob ignore.
    /// Returns absolute, lexically-sorted paths.
    fn iter_gcl(&self, dir: &Path) -> Vec<PathBuf>;

    /// `true` iff `path` is a directory.
    fn is_dir(&self, path: &Path) -> bool;

    /// Absolute path to GreyCat's home directory.
    fn greycat_home(&self) -> &Path;
}

/// Default real-filesystem [`Context`]. Holds the resolved GreyCat home so
/// `greycat_home()` is allocation-free.
#[derive(Debug, Clone)]
pub struct FsContext {
    greycat_home: PathBuf,
}

impl FsContext {
    /// Resolve `$GREYCAT_HOME` (or default) and construct a context.
    pub fn new() -> Result<Self, HomeError> {
        Ok(Self {
            greycat_home: try_greycat_home()?,
        })
    }

    /// Construct a context with an explicit GreyCat home, bypassing env
    /// resolution. Useful for tests and CI environments that want to pin
    /// the home regardless of `$GREYCAT_HOME`.
    pub fn with_greycat_home(greycat_home: PathBuf) -> Self {
        Self { greycat_home }
    }
}

impl Context for FsContext {
    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn iter_gcl(&self, dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        walk_gcl(dir, &mut out);
        out.sort();
        out
    }

    fn is_dir(&self, path: &Path) -> bool {
        std::fs::metadata(path)
            .map(|m| m.is_dir())
            .unwrap_or(false)
    }

    fn greycat_home(&self) -> &Path {
        &self.greycat_home
    }
}

/// Directory names skipped by [`Context::iter_gcl`] — matches the TS
/// `IGNORE` list in `packages/resolver/src/{fs,sync-fs}.ts`.
const IGNORED_DIRS: &[&str] = &["node_modules", "gcdata", ".git"];

fn walk_gcl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if IGNORED_DIRS.contains(&name) {
            continue;
        }
        if path.is_dir() {
            walk_gcl(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("gcl")
            && let Ok(canonical) = path.canonicalize()
        {
            out.push(canonical);
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_dir_joins_under_lib() {
        let dir = library_dir(Path::new("/proj"), "std");
        assert_eq!(dir, PathBuf::from("/proj/lib/std"));
    }

    #[test]
    fn global_std_dir_under_greycat_home() {
        let dir = global_std_dir(Path::new("/home/u/.greycat"));
        assert_eq!(dir, PathBuf::from("/home/u/.greycat/lib/std"));
    }

    #[test]
    fn include_dir_relative_join() {
        assert_eq!(
            include_dir(Path::new("/proj"), "src"),
            PathBuf::from("/proj/src"),
        );
        assert_eq!(
            include_dir(Path::new("/proj"), "src/sub"),
            PathBuf::from("/proj/src/sub"),
        );
    }

    #[test]
    fn installed_file_path_under_lib() {
        assert_eq!(
            installed_file_path(Path::new("/proj")),
            PathBuf::from("/proj/lib/installed"),
        );
    }

    #[test]
    fn parse_installed_versioned_and_unversioned() {
        let content = "std=1.2.3\nmylib=\nfoo=4.5.6\n";
        let parsed = parse_installed_file(content);
        assert_eq!(parsed.entries.len(), 3);
        assert_eq!(parsed.entries.get("std"), Some(&Some("1.2.3".to_string())));
        assert_eq!(parsed.entries.get("mylib"), Some(&None));
        assert_eq!(parsed.entries.get("foo"), Some(&Some("4.5.6".to_string())));
    }

    #[test]
    fn parse_installed_skips_blank_and_malformed_lines() {
        let content = "std=1.0\n\nmalformed-no-equals\n=val-no-name\nfoo=2.0\n";
        let parsed = parse_installed_file(content);
        // `malformed-no-equals` skipped; `=val-no-name` parses as name="" (TS
        // does the same — split_once produces ("", "val-no-name")). The TS
        // port stores an empty-string key, which we mirror.
        assert!(parsed.entries.contains_key("std"));
        assert!(parsed.entries.contains_key("foo"));
        assert!(!parsed.entries.contains_key("malformed-no-equals"));
        assert_eq!(
            parsed.entries.get(""),
            Some(&Some("val-no-name".to_string())),
        );
    }

    #[test]
    fn parse_installed_empty_input() {
        assert_eq!(parse_installed_file("").entries.len(), 0);
        assert_eq!(parse_installed_file("\n\n").entries.len(), 0);
    }

    #[test]
    fn home_env_must_be_absolute() {
        let err = resolve_greycat_home(Some("relative/path"), None).unwrap_err();
        assert_eq!(err, HomeError::NotAbsolute("relative/path".into()));
    }

    #[test]
    fn home_env_absolute_wins() {
        let got = resolve_greycat_home(Some("/explicit"), Some(Path::new("/home/u")))
            .unwrap();
        assert_eq!(got, PathBuf::from("/explicit"));
    }

    #[test]
    fn home_falls_back_to_dot_greycat() {
        let got = resolve_greycat_home(None, Some(Path::new("/home/u"))).unwrap();
        assert_eq!(got, PathBuf::from("/home/u/.greycat"));
    }

    #[test]
    fn home_unresolvable_when_neither_set() {
        assert_eq!(
            resolve_greycat_home(None, None).unwrap_err(),
            HomeError::NoHome,
        );
    }

    #[test]
    fn fs_context_iter_gcl_finds_corpus() {
        let ws = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("tests/corpus");
        let ctx = FsContext::with_greycat_home(PathBuf::from("/nonexistent"));
        let files = ctx.iter_gcl(&ws);
        assert!(
            files.iter().all(|p| p.extension().and_then(|s| s.to_str()) == Some("gcl")),
            "iter_gcl returned a non-.gcl path: {files:?}"
        );
        assert!(
            files.len() >= 18,
            "expected at least 18 .gcl files in tests/corpus, got {}",
            files.len()
        );
    }

    #[test]
    fn fs_context_iter_gcl_skips_ignored_dirs() {
        let tmp = std::env::temp_dir().join(format!(
            "gcat-resolver-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("node_modules")).unwrap();
        std::fs::create_dir_all(tmp.join("gcdata")).unwrap();
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("node_modules/skipped.gcl"), "fn a(){}").unwrap();
        std::fs::write(tmp.join("gcdata/skipped.gcl"), "fn a(){}").unwrap();
        std::fs::write(tmp.join(".git/skipped.gcl"), "fn a(){}").unwrap();
        std::fs::write(tmp.join("src/kept.gcl"), "fn a(){}").unwrap();

        let ctx = FsContext::with_greycat_home(PathBuf::from("/nonexistent"));
        let files = ctx.iter_gcl(&tmp);
        assert_eq!(files.len(), 1, "got: {files:?}");
        assert!(files[0].ends_with("kept.gcl"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
