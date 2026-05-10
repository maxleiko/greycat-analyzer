//! Dogfood test: run `cli lint --fix` against the solarleb closed-source
//! corpus and assert no `.gcl` file in `backend/` ends up with a parse
//! error. The corpus lives at `~/dev/datathings/assaad/solarleb` and is
//! the project that originally exposed the unused-local / unused-param
//! / cascade bugs that motivated P22.
//!
//! The test no-ops when:
//!   - the corpus directory is missing, OR
//!   - `git` isn't on PATH (we use it to snapshot + restore), OR
//!   - the corpus has uncommitted changes (we refuse to clobber them).
//!
//! Otherwise it: snapshots via `git stash --include-untracked`, runs
//! the CLI binary against `backend/project.gcl`, walks every `.gcl`
//! file under `backend/`, asserts each parses without errors, then
//! restores the snapshot via `git stash pop`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn corpus_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let p = PathBuf::from(home).join("dev/datathings/assaad/solarleb");
    if p.join(".git").is_dir() && p.join("backend").is_dir() {
        Some(p)
    } else {
        None
    }
}

fn git_clean(repo: &Path) -> bool {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo)
        .output();
    matches!(out, Ok(o) if o.status.success() && o.stdout.is_empty())
}

fn git_stash(repo: &Path) -> bool {
    let out = Command::new("git")
        .args(["stash", "push", "--include-untracked", "-m", "p22-dogfood"])
        .current_dir(repo)
        .output();
    matches!(out, Ok(o) if o.status.success())
}

fn git_stash_pop(repo: &Path) {
    let _ = Command::new("git")
        .args(["stash", "pop"])
        .current_dir(repo)
        .output();
}

fn cli_binary() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo when running integration tests
    // for binary crates — points at the freshly-built binary. The crate
    // ships `greycat-lang` as the canonical bin name (P4.3); fall back
    // to env::current_exe's parent + the binary stem if that fails.
    if let Some(path) = option_env!("CARGO_BIN_EXE_greycat-lang") {
        return PathBuf::from(path);
    }
    PathBuf::from(env!("CARGO_BIN_EXE_greycat-analyzer"))
}

#[test]
fn solarleb_fix_does_not_break_parses() {
    let Some(repo) = corpus_root() else {
        eprintln!("[solarleb_fix] skipped: corpus not present");
        return;
    };
    if !git_clean(&repo) {
        eprintln!(
            "[solarleb_fix] skipped: {} has uncommitted changes (won't clobber)",
            repo.display()
        );
        return;
    }
    let project = repo.join("project.gcl");
    if !project.is_file() {
        eprintln!("[solarleb_fix] skipped: {} missing", project.display());
        return;
    }

    // Sentinel — no actual stash needed when tree is clean (just makes
    // the cleanup branch always-safe). We still call pop on exit to
    // recover any unexpected state.
    let _stashed = git_stash(&repo);

    let bin = cli_binary();
    let output = Command::new(&bin)
        .arg("lint")
        .arg("--fix")
        .arg(&project)
        .output()
        .expect("invoke cli");
    eprintln!(
        "[solarleb_fix] cli exit: {:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    // Walk every .gcl under backend/ and assert each parses cleanly.
    let mut broken: Vec<String> = Vec::new();
    walk_gcl(&repo.join("backend"), &mut |path| {
        let Ok(src) = std::fs::read_to_string(path) else {
            return;
        };
        let tree = greycat_analyzer_syntax::parse(&src);
        if tree.root_node().has_error() {
            broken.push(
                path.strip_prefix(&repo)
                    .unwrap_or(path)
                    .display()
                    .to_string(),
            );
        }
    });

    git_stash_pop(&repo);

    assert!(
        broken.is_empty(),
        "lint --fix produced parse errors in: {broken:?}"
    );
}

fn walk_gcl(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_gcl(&path, visit);
        } else if path.extension().and_then(|s| s.to_str()) == Some("gcl") {
            visit(&path);
        }
    }
}
