#[derive(Debug)]
pub struct Module {
    stmts: Vec<Stmt>,
}

#[derive(Debug)]
pub enum Stmt {
    Fn(Function),
}

#[derive(Debug)]
pub struct Function {}