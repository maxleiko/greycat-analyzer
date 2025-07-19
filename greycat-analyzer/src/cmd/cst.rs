use std::path::PathBuf;

use greycat_analyzer_core::tokenize;

use crate::utils::AnyError;

#[derive(clap::Parser)]
#[clap(about = "Prints the Cst s-expr")]
pub struct Cst {
    #[clap(help = "Path to a project.gcl")]
    project: PathBuf,

    #[clap(long, help = "Enables the display of trivia tokens in s-expr")]
    trivia: bool,
}

impl Cst {
    pub fn run(self) -> Result<(), AnyError> {
        env_logger::init();
        let source = std::fs::read_to_string(self.project)?;
        // let mut parser = CstParser::new(&source);
        // let module = parser
        //     .parse_module(&source)
        //     .map_err(|err| err.to_source_error(&source))?;
        // println!("{}", module.to_display_node(&source, self.trivia));
        let tokens = tokenize(&source);
        let module = greycat_analyzer_core::cst::parse(&tokens);
        println!("{}", module.to_display_node(&source, self.trivia));
        Ok(())
    }
}
