mod cmd;
mod utils;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use cmd::*;

use crate::utils::AnyError;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Lint(Lint),
    LangServer(LangServer),
    Cst(Cst),
}

fn main() -> Result<ExitCode, AnyError> {
    let cli = Cli::parse();
    match cli.command {
        Command::Lint(cmd) => cmd.run(),
        Command::LangServer(cmd) => cmd.run().map(|_| ExitCode::SUCCESS),
        Command::Cst(cmd) => cmd.run().map(|_| ExitCode::SUCCESS),
    }
}
