use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use greycat_analyzer_core::{SourceManager, resolver::FsContext};

use crate::utils::AnyError;

#[derive(clap::Parser)]
#[clap(
    about = "Format a GreyCat project. Loads the entrypoint and walks its \
                @library / @include pragmas to discover modules — only \
                reachable files are formatted."
)]
pub struct Fmt {
    #[clap(help = "Path to a project.gcl entrypoint (or any single .gcl \
                   file to format in isolation). When omitted, looks for \
                   `project.gcl` in the current working directory.")]
    project: Option<PathBuf>,
    #[clap(
        long,
        value_enum,
        default_value_t = FmtMode::Write,
        help = "Output mode. `write` rewrites resolved files in place. \
                `check` exits non-zero on drift, listing every file that \
                would change. `stdout` formats only the entrypoint and \
                prints to stdout (the @library / @include closure is \
                ignored). `diff` prints a unified diff per file (colored \
                when stdout is a TTY)."
    )]
    mode: FmtMode,
    #[clap(
        long,
        help = "Also format files in non-`project` libraries (modules \
                under `lib/<name>/`). Off by default — library code \
                isn't yours, and reformatting third-party stdlib is \
                rarely what you want. Mirrors `lint --lint-libs`."
    )]
    fmt_libs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FmtMode {
    Write,
    Check,
    Stdout,
    Diff,
}

impl Fmt {
    pub fn run(self) -> Result<ExitCode, AnyError> {
        env_logger::init();

        // Project-shaped discovery: optional positional accepting a
        // .gcl entrypoint OR a directory (auto `project.gcl`), defaults
        // to cwd. Mirrors `lint`.
        let initial = match self.project {
            Some(p) => p,
            None => std::env::current_dir()?,
        };
        let mut project_filepath = initial.canonicalize()?;
        if project_filepath.is_dir() {
            let candidate = project_filepath.join("project.gcl");
            if candidate.is_file() {
                project_filepath = candidate;
            } else {
                eprintln!(
                    "error: no project.gcl found in {}; pass a .gcl entrypoint or a directory containing project.gcl",
                    project_filepath.display(),
                );
                return Ok(ExitCode::FAILURE);
            }
        }

        // `stdout` mode short-circuits: only the entrypoint is emitted,
        // so loading the @library / @include closure would be wasted
        // work.
        if self.mode == FmtMode::Stdout {
            let source = std::fs::read_to_string(&project_filepath)?;
            let formatted = greycat_analyzer_fmt::format(&source);
            print!("{formatted}");
            return Ok(ExitCode::SUCCESS);
        }

        // Load the project closure for write / check / diff modes.
        let ctx = FsContext::new().unwrap_or_else(|_| FsContext::with_greycat_home(PathBuf::new()));
        let mut mgr = SourceManager::with_context(Arc::new(ctx));
        let report = mgr.load_project(&project_filepath);
        for err in &report.errors {
            eprintln!("warning: {err}");
        }

        // Per-file: skip non-project libs unless --fmt-libs, skip files
        // with parse errors (warn — tree-sitter recovers but the
        // formatted output can be garbage), then format. Defer the
        // mode-specific dispatch to a single match below so the loop
        // produces a uniform `Vec<FmtEntry>`.
        let mut entries: Vec<FmtEntry> = Vec::new();
        let mut any_skipped = false;
        for (_uri, cell) in mgr.iter() {
            let doc = cell.borrow();
            if !self.fmt_libs && doc.lib != "project" {
                continue;
            }
            let path = doc.filepath().to_path_buf();
            if doc.root_node().has_error() {
                eprintln!(
                    "warning: skipping {}: parse errors prevent safe formatting",
                    path.display(),
                );
                any_skipped = true;
                continue;
            }
            let formatted = greycat_analyzer_fmt::format_tree(&doc.text, doc.root_node());
            entries.push(FmtEntry {
                path,
                source: doc.text.clone(),
                formatted,
            });
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let drift: Vec<&FmtEntry> = entries.iter().filter(|e| e.source != e.formatted).collect();

        match self.mode {
            FmtMode::Stdout => unreachable!("handled above"),
            FmtMode::Write => {
                for e in &drift {
                    std::fs::write(&e.path, &e.formatted)?;
                }
                if !drift.is_empty() {
                    println!("formatted {} file(s)", drift.len());
                }
                Ok(if any_skipped {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                })
            }
            FmtMode::Check => {
                for e in &drift {
                    println!("would reformat: {}", e.path.display());
                }
                println!("{} file(s) would be reformatted", drift.len());
                Ok(if drift.is_empty() && !any_skipped {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                })
            }
            FmtMode::Diff => {
                let color = std::io::stdout().is_terminal();
                for e in &drift {
                    print_unified_diff(&e.path, &e.source, &e.formatted, color);
                }
                Ok(if drift.is_empty() && !any_skipped {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                })
            }
        }
    }
}

struct FmtEntry {
    path: PathBuf,
    source: String,
    formatted: String,
}

/// Unified diff via `similar`. Coloring is hand-rolled ANSI so the
/// output looks like `git diff` / `prettier --check --diff` and stays
/// usable in code review without an extra viewer.
fn print_unified_diff(path: &Path, old: &str, new: &str, color: bool) {
    use similar::{ChangeTag, TextDiff};
    const RED: &str = "\x1b[31m";
    const GREEN: &str = "\x1b[32m";
    const CYAN: &str = "\x1b[36m";
    const RESET: &str = "\x1b[0m";
    let header_old = format!("--- {}", path.display());
    let header_new = format!("+++ {} (formatted)", path.display());
    if color {
        println!("{CYAN}{header_old}{RESET}");
        println!("{CYAN}{header_new}{RESET}");
    } else {
        println!("{header_old}");
        println!("{header_new}");
    }
    let diff = TextDiff::from_lines(old, new);
    for hunk in diff.unified_diff().iter_hunks() {
        let header = hunk.header().to_string();
        if color {
            print!("{CYAN}{header}{RESET}");
        } else {
            print!("{header}");
        }
        if !header.ends_with('\n') {
            println!();
        }
        for change in hunk.iter_changes() {
            let (sign, style) = match change.tag() {
                ChangeTag::Delete => ('-', RED),
                ChangeTag::Insert => ('+', GREEN),
                ChangeTag::Equal => (' ', ""),
            };
            if color && !style.is_empty() {
                print!("{style}{sign}{}{RESET}", change.value());
            } else {
                print!("{sign}{}", change.value());
            }
            if change.missing_newline() {
                println!();
            }
        }
    }
}
