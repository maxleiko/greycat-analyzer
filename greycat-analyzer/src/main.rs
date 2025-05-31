mod utils;

use std::{path::PathBuf, time::Instant};

use anyhow::Result;
use clap::Parser;
use greycat_analyzer_core::{
    lexer::{self, TokenKind, tokenize},
    parser,
};
use utils::for_each_valid_entry;

#[derive(Debug, Parser)]
struct Args {
    #[clap(help = "Path to a project.gcl")]
    project: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.project.is_dir() {
        for_each_valid_entry(
            &args.project,
            &|entry| entry.extension().is_some_and(|ext| ext == "gcl"),
            &|entry| {
                let source = std::fs::read_to_string(entry)?;
                let start = Instant::now();
                let tokens = tokenize(&source);
                println!(
                    "{:>10.2?} {:6} {}",
                    start.elapsed(),
                    tokens.len(),
                    entry.to_string_lossy()
                );
                Ok(())
            },
        )?;
        return Ok(());
    }

    let source = std::fs::read_to_string(args.project)?;
    let start = Instant::now();
    let mut parser = parser::Parser::new(&source);
    let module = parser
        .parse(&source)
        .map_err(|err| err.to_source_error(&source))?;

    println!(
        "[{:?}]\n{}",
        start.elapsed(),
        module.to_display_node(&source)
    );

    Ok(())
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
