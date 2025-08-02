mod combi;
mod cursor;
mod display;
mod node;
mod parser;
mod info;
mod node_query;

use std::path::Path;

pub use cursor::*;
pub use node::*;
pub use parser::*;
pub use info::*;

#[derive(Debug)]
pub struct SourceModule {
    source: String,
    module: Node,
}

pub fn parse_file(filepath: impl AsRef<Path>) -> Result<SourceModule, std::io::Error> {
    let source = std::fs::read_to_string(filepath.as_ref())?;
    let module = parse(&crate::lexer::tokenize(&source));
    Ok(SourceModule { source, module })
}
