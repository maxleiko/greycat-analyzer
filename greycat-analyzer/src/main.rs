use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use greycat_lang::lexer::{TokenKind, tokenize};

#[derive(Debug, Parser)]
struct Args {
    #[clap(help = "Path to a project.gcl")]
    project: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let source = std::fs::read_to_string(args.project)?;
    let tokens = tokenize(&source);
    dump_tokens(&source, &tokens);

    

    Ok(())
}

#[allow(dead_code)]
fn dump_tokens(source: &str, tokens: &[greycat_lang::lexer::Token]) {
    for tok in tokens {
        match tok.kind {
            TokenKind::NewLine(n) => {
                for _ in 0..n {
                    println!();
                }
            }
            _ => print!("⌈{}⌉", &source[tok.span.as_range(source)]),
        }
    }
}
