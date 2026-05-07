use std::path::PathBuf;

use crate::utils::AnyError;

#[derive(clap::Parser)]
#[clap(about = "Prints the CST s-expression of a .gcl file")]
pub struct Cst {
    #[clap(help = "Path to a .gcl file")]
    file: PathBuf,
}

impl Cst {
    pub fn run(self) -> Result<(), AnyError> {
        env_logger::init();
        let source = std::fs::read_to_string(self.file)?;
        let tree = greycat_analyzer_syntax::parse(&source);
        println!("{}", tree.root_node().to_sexp());
        Ok(())
    }
}
