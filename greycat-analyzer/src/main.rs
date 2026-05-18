mod cmd;
mod utils;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use cmd::*;

use crate::utils::AnyError;

#[derive(Parser)]
#[clap(name = "greycat-lang", version)]
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
    // P18.1
    /// Dump per-expression byte ranges and inferred type display strings as JSONL.
    #[clap(name = "dump-types")]
    DumpTypes(DumpTypes),
    // P18.1
    /// Dump per-ident-use byte ranges and decl pointers as JSONL.
    #[clap(name = "dump-resolutions")]
    DumpResolutions(DumpResolutions),
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
        Command::DumpTypes(cmd) => cmd.run(),
        Command::DumpResolutions(cmd) => cmd.run(),
    }
}
