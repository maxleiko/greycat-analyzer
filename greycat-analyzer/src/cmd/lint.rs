use std::{path::PathBuf, time::Instant};

use crate::utils::for_each_valid_entry;
use anyhow::Result;
use greycat_analyzer_core::{Token, TokenKind, parse, tokenize};

#[derive(clap::Parser)]
#[clap(about = "Lints a project")]
pub struct Lint {
    #[clap(help = "Path to a project.gcl")]
    project: PathBuf,
}

impl Lint {
    pub fn run(self) -> Result<()> {
        let source = std::fs::read_to_string(self.project)?;
        let start = Instant::now();
        let mut errors = Vec::new();
        let module =
            parse("project", &source, &mut errors).map_err(|err| err.to_source_error(&source))?;
        let took = start.elapsed();
        println!("{:#?}", module.to_pretty(&source));
        println!("Parsed in {took:?}, {} errors", errors.len());
        Ok(())
    }
}

fn dump_tokens(source: &str, tokens: &[Token]) {
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
