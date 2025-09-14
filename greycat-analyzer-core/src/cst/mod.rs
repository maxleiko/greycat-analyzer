mod combi;
// mod cursor;
mod display;
mod info;
mod node;
mod node_query;
mod parser;
mod visitor;

use std::path::Path;

use bumpalo::Bump;
// pub use cursor::*;
pub use info::*;
pub use node::*;
pub use parser::*;
pub use visitor::*;

pub use crate::cst::combi::ParserCtx;

#[derive(Debug)]
pub struct SourceModule<'arena> {
    pub source: String,
    pub module: Node<'arena>,
}

pub fn parse_file(filepath: impl AsRef<Path>, arena: &Bump) -> Result<SourceModule<'_>, std::io::Error> {
    // let start = std::time::Instant::now();
    let source = std::fs::read_to_string(filepath.as_ref())?;
    // let read_file = start.elapsed();
    // let start = std::time::Instant::now();
    let tokens = crate::lexer::tokenize(&source);
    let module = parse(ParserCtx { arena, tokens: &tokens });
    // let parse = start.elapsed();
    // println!("{} read_file={read_file:?}, parse={parse:?}", filepath.as_ref().display());
    Ok(SourceModule { source, module })
}
