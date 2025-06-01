mod parser;

pub use parser::*;

use crate::{cst, span::Span};

#[derive(Debug)]
pub struct Module {
    pub name: String,
    pub pragmas: Vec<Pragma>,
    pub functions: Vec<Function>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Function {
    pub name: Span,
}

#[derive(Debug)]
pub struct Pragma {
    pub name: Span,
}
