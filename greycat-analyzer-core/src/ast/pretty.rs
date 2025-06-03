use core::fmt;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Module<'a> {
    pub name: &'a str,
    pub pragmas: Vec<Pragma<'a>>,
    pub functions: Vec<Function<'a>>,
}

#[derive(Debug, Serialize)]
pub struct Function<'a> {
    pub name: &'a str,
    pub params: FnParams<'a>,
}

#[derive(Debug, Serialize)]
pub struct FnParams<'a> {
    pub params: Vec<FnParam<'a>>,
}

#[derive(Debug, Serialize)]
pub struct FnParam<'a> {
    pub name: &'a str,
}

#[derive(Debug, Serialize)]
pub struct Pragma<'a> {
    pub name: &'a str,
    pub args: Option<Vec<ConstExpr<'a>>>,
}

#[derive(Debug, Serialize)]
pub enum ConstExpr<'a> {
    String(StringLiteral<'a>),
}

#[derive(Debug, Serialize)]
pub struct StringLiteral<'a>(pub(super) &'a str);
