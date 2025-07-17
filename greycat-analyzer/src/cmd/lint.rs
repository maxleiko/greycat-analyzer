use std::{path::PathBuf, time::Instant};

use crate::utils::{AnyError, for_each_valid_entry};
use greycat_analyzer_core::{Token, TokenKind, parse, tokenize};

#[derive(clap::Parser)]
#[clap(about = "Lints a project")]
pub struct Lint {
    #[clap(help = "Path to a project.gcl")]
    project: PathBuf,
}

impl Lint {
    pub fn run(self) -> Result<(), AnyError> {
        env_logger::init();
        let source = std::fs::read_to_string(&self.project)?;
        let start = Instant::now();
        let mut errors = Vec::new();
        let module =
            parse("project", &source, &mut errors).map_err(|err| err.to_source_error(&source))?;
        let took = start.elapsed();
        println!("{:#?}", module.to_pretty(&source));
        println!("Parsed in {took:?}, {} errors", errors.len());

        if !errors.is_empty() {
            println!("====================");
            for error in errors {
                println!(
                    "{}:{}:{}\n\t{}",
                    self.project.to_string_lossy(),
                    error.range.start.line,
                    error.range.start.character,
                    error.message
                );
            }
        }
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
            _ => eprint!("⌈{}⌉", &source[tok.span.as_range()]),
        }
    }
}
