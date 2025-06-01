#![allow(unused)] // TODO remove when stable

mod cmd;
mod utils;

use anyhow::Result;
use clap::{Parser, Subcommand};
use cmd::*;

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

fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();
    match cli.command {
        Command::Lint(cmd) => cmd.run(),
        Command::LangServer(cmd) => cmd.run(),
        Command::Cst(cmd) => cmd.run(),
    }
}
