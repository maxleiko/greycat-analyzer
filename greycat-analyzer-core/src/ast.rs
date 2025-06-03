mod parser;
mod pretty;

pub use parser::*;
use serde::Serialize;

use crate::{cst, span::Span};

#[derive(Debug, Serialize)]
pub struct Module {
    pub name: String,
    pub pragmas: Vec<Pragma>,
    pub functions: Vec<Function>,
    pub span: Span,
}

impl Module {
    pub fn to_pretty<'a>(&'a self, source: &'a str) -> pretty::Module<'a> {
        pretty::Module {
            name: &self.name,
            pragmas: self.pragmas.iter().map(|p| p.to_pretty(source)).collect(),
            functions: self.functions.iter().map(|f| f.to_pretty(source)).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Function {
    pub name: Span,
    pub params: FnParams,
    pub span: Span,
}

impl Function {
    fn to_pretty<'a>(&self, source: &'a str) -> pretty::Function<'a> {
        pretty::Function {
            name: &source[self.name.as_range(source)],
            params: self.params.to_pretty(source),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FnParams {
    pub params: Vec<FnParam>,
    pub span: Span,
}

impl FnParams {
    fn to_pretty<'a>(&self, source: &'a str) -> pretty::FnParams<'a> {
        pretty::FnParams {
            params: self.params.iter().map(|p| p.to_pretty(source)).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FnParam {
    pub name: Span,
    pub span: Span,
}

impl FnParam {
    fn to_pretty<'a>(&self, source: &'a str) -> pretty::FnParam<'a> {
        pretty::FnParam {
            name: &source[self.name.as_range(source)],
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Pragma {
    pub name: Span,
    pub args: Option<Vec<ConstExpr>>,
    pub span: Span,
}

impl Pragma {
    fn to_pretty<'a>(&self, source: &'a str) -> pretty::Pragma<'a> {
        pretty::Pragma {
            name: &source[self.name.as_range(source)],
            args: self
                .args
                .as_deref()
                .map(|args| args.iter().map(|e| e.to_pretty(source)).collect()),
        }
    }
}

#[derive(Debug, Serialize)]
pub enum ConstExpr {
    String(StringLiteral),
}

impl ConstExpr {
    fn to_pretty<'a>(&self, source: &'a str) -> pretty::ConstExpr<'a> {
        match self {
            ConstExpr::String(lit) => pretty::ConstExpr::String(lit.to_pretty(source)),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct StringLiteral {
    pub text: Span,
    pub span: Span,
}

impl StringLiteral {
    fn to_pretty<'a>(&self, source: &'a str) -> pretty::StringLiteral<'a> {
        pretty::StringLiteral(&source[self.text.as_range(source)])
    }
}
