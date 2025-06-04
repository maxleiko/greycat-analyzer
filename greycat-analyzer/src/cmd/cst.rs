use std::path::PathBuf;

use greycat_analyzer_core::CstParser;

use crate::utils::AnyError;

#[derive(clap::Parser)]
#[clap(about = "Prints the Cst s-expr")]
pub struct Cst {
    #[clap(help = "Path to a project.gcl")]
    project: PathBuf,
}

impl Cst {
    pub fn run(self) -> Result<(), AnyError> {
        let source = std::fs::read_to_string(self.project)?;
        let mut parser = CstParser::new(&source);
        let module = parser
            .parse_module(&source)
            .map_err(|err| err.to_source_error(&source))?;
        println!("{}", module.to_display_node(&source));
        Ok(())
    }
}
