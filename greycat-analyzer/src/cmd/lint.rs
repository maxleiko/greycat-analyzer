use std::{path::PathBuf, time::Instant};

use anyhow::Result;
use clap::Parser;
use greycat_analyzer_core::{lexer::tokenize, parser};

use crate::utils::for_each_valid_entry;

#[derive(Parser)]
#[clap(about = "Lints a project")]
pub struct Lint {
    #[clap(help = "Path to a project.gcl")]
    project: PathBuf,
}

impl Lint {
    pub fn run(self) -> Result<()> {
        if self.project.is_dir() {
            for_each_valid_entry(
                &self.project,
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

        let source = std::fs::read_to_string(self.project)?;
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
}
