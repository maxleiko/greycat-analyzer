#![allow(unused)] // TODO remove when stable

mod cmd;
mod utils;

use std::error::Error;

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
    Lex(Lex),
}

fn main() -> Result<(), AnyError> {
    let cli = Cli::parse();
    match cli.command {
        Command::Lint(cmd) => cmd.run(),
        Command::LangServer(cmd) => cmd.run(),
        Command::Cst(cmd) => cmd.run(),
        Command::Lex(cmd) => cmd.run(),
    }
}
