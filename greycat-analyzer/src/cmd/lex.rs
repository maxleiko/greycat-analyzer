use std::path::PathBuf;

use greycat_analyzer_core::Lexer;

use crate::utils::AnyError;

#[derive(clap::Parser)]
#[clap(about = "Prints the tokens of a module")]
pub struct Lex {
    #[clap(help = "Path to a module.gcl")]
    module: PathBuf,
}

impl Lex {
    pub fn run(self) -> Result<(), AnyError> {
        env_logger::init();
        let source = std::fs::read_to_string(self.module)?;
        let lexer = Lexer::new(&source);
        for token in lexer {
            println!("{}", token.to_source_token(&source));
        }
        Ok(())
    }
}
