use std::{path::PathBuf, time::Instant};

use crate::utils::{AnyError, for_each_valid_entry};
use greycat_analyzer_core::{cst::{ModuleInfo, Node, SourceModule}, parse, tokenize, Token, TokenKind};

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
        println!("{}: {info:#?}", self.project.display());
        Ok(())
    }
}
