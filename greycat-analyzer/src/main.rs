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
    /// Lint a project — parse + semantic + lint diagnostics.
    Lint(Lint),
    /// Format a GreyCat project (`--mode=write|check|stdout|diff`).
    Fmt(Fmt),
    /// Start the LSP server. Alias: `lang-server`.
    #[clap(alias = "lang-server")]
    Server(LangServer),
    /// Print the tree-sitter CST s-expression for a `.gcl` file (debug).
    Cst(Cst),
    /// Dump per-expression byte ranges and inferred type display strings as JSONL (P18.1).
    #[clap(name = "dump-types")]
    DumpTypes(DumpTypes),
    /// Dump per-ident-use byte ranges and decl pointers as JSONL (P18.1).
    #[clap(name = "dump-resolutions")]
    DumpResolutions(DumpResolutions),
}

fn main() -> Result<ExitCode, AnyError> {
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
