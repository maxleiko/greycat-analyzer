mod cmd;
mod utils;

use anyhow::Result;
use clap::{Parser, Subcommand};
use cmd::*;
use greycat_analyzer_core::lexer::{self, TokenKind};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Lint(Lint),
    LangServer(LangServer),
}

fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();
    match cli.command {
        Command::Lint(cmd) => cmd.run(),
        Command::LangServer(cmd) => cmd.run(),
    }
}

#[allow(dead_code)]
fn dump_tokens(source: &str, tokens: &[lexer::Token]) {
    for tok in tokens {
        match tok.kind {
            TokenKind::NewLine(n) => {
                for _ in 0..n {
                    eprintln!();
                }
            }
            _ => eprint!("⌈{}⌉", &source[tok.span.as_range(source)]),
        }
    }
}
