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
    pub params: FnParams,
    pub span: Span,
}

#[derive(Debug)]
pub struct FnParams {
    pub params: Vec<FnParam>,
    pub span: Span,
}

#[derive(Debug)]
pub struct FnParam {
    pub name: Span,
    pub span: Span,
}

#[derive(Debug)]
pub struct Pragma {
    pub name: Span,
    pub args: Option<Vec<ConstExpr>>,
    pub span: Span,
}

#[derive(Debug)]
pub enum ConstExpr {
    String(StringLiteral),
}

#[derive(Debug)]
pub struct StringLiteral {
    pub text: Span,
    pub span: Span,
}
