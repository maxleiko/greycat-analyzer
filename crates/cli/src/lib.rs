//! Library face of the `greycat-analyzer` CLI. The binary (`src/main.rs`)
//! and the C FFI cdylib (`greycat-analyzer-ffi`) both drive the same clap
//! dispatch through [`run_from_args`].

mod cmd;
mod utils;

use std::ffi::OsString;

use clap::{Parser, Subcommand};
use cmd::*;

// CI stamps the greycat-style branch-qualified version (e.g. `0.1.0-dev`) via
// this env at build time; plain `cargo build` falls back to Cargo.toml.
const VERSION: &str = match option_env!("GREYCAT_ANALYZER_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[clap(name = "greycat-analyzer", version = VERSION)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Lint a GreyCat project. Loads the entrypoint and walks its
    /// @library / @include pragmas to discover modules - only
    /// reachable files are analyzed.
    Lint(Lint),
    /// Format a GreyCat project (`--mode=write|check|stdout|diff`).
    Fmt(Fmt),
    /// Start the LSP server.
    Server(LangServer),
    /// Print the tree-sitter CST s-expression for a `.gcl` file (debug).
    Cst(Cst),
    /// Print the full HIR view of a project (debug). Use `--json` for
    /// machine-readable output; default is Rust Debug pretty-print.
    Hir(HirCmd),
}

/// Parse `args` (clap-style: first element is the program name) and run
/// the selected subcommand, returning the process exit code.
///
/// `--help` / `--version` render and return 0; other parse errors render
/// and return 2 (clap's convention); a subcommand error returns 1. This
/// never calls `process::exit`, so it is safe to invoke from a dlopen'd
/// library where exiting would kill the host process.
pub fn run_from_args<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return match err.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 2,
            };
        }
    };
    let outcome = match cli.command {
        Command::Lint(cmd) => cmd.run(),
        Command::Fmt(cmd) => cmd.run(),
        Command::Server(cmd) => cmd.run(),
        Command::Cst(cmd) => cmd.run(),
        Command::Hir(cmd) => cmd.run(),
    };
    match outcome {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    }
}
