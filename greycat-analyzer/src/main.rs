mod cmd;
mod utils;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use cmd::*;

use crate::utils::AnyError;

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
    /// @library / @include pragmas to discover modules — only
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

fn main() -> Result<ExitCode, AnyError> {
    // Restore default SIGPIPE handler so piping into a pager (`… | less`)
    // and quitting early exits the process cleanly with status 141 instead
    // of panicking inside println!. Rust's runtime ignores SIGPIPE by
    // default, which surfaces every closed-pipe write as an io::Error that
    // print!/println! turn into "failed printing to stdout: Broken pipe".
    #[cfg(unix)]
    // SAFETY: main has not yet spawned threads; resetting a signal disposition
    // here is race-free.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    match cli.command {
        Command::Lint(cmd) => cmd.run(),
        Command::Fmt(cmd) => cmd.run(),
        Command::Server(cmd) => cmd.run().map(|_| ExitCode::SUCCESS),
        Command::Cst(cmd) => cmd.run().map(|_| ExitCode::SUCCESS),
        Command::Hir(cmd) => cmd.run(),
    }
}
