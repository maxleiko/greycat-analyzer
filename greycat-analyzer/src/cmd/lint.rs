use std::{path::PathBuf, time::Instant};

use crate::utils::{AnyError, for_each_valid_entry};
use greycat_analyzer_core::{
    Token, TokenKind,
    cst::{ModuleInfo, Node, SourceModule},
    parse, tokenize,
};

#[derive(clap::Parser)]
#[clap(about = "Lints a project")]
pub struct Lint {
    #[clap(help = "Path to a project.gcl")]
    project: PathBuf,
}

impl Lint {
    pub fn run(self) -> Result<(), AnyError> {
        env_logger::init();
        let start = Instant::now();
        let project = greycat_analyzer_core::cst::parse_file(&self.project)?;
        let took = start.elapsed();
        let info = ModuleInfo::from(&project);
        println!("Module: {}", self.project.display());
        println!("Libraries:");
        for lib in &info.libraries {
            println!(
                "  name={} version={:?}",
                lib.name.image,
                lib.version.map(|s| s.image)
            );
        }
        println!("Includes:");
        for include in &info.includes {
            println!("  dir={}", include.image);
        }
        Ok(())
    }
}
