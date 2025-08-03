mod combi;
mod cursor;
mod display;
mod info;
mod node;
mod node_query;
mod parser;
mod visitor;

use std::path::Path;

pub use cursor::*;
pub use info::*;
pub use node::*;
pub use parser::*;
pub use visitor::*;

#[derive(Debug)]
pub struct SourceModule {
    pub source: String,
    pub module: Node,
}

pub fn parse_file(filepath: impl AsRef<Path>) -> Result<SourceModule, std::io::Error> {
    // let start = Instant::now();
    let source = std::fs::read_to_string(filepath.as_ref())?;
    // let read_file = start.elapsed();
    // let start = Instant::now();
    let module = parse(&crate::lexer::tokenize(&source));
    // let parse = start.elapsed();
    // println!("read_file={read_file:?}, parse={parse:?}");
    Ok(SourceModule { source, module })
}
