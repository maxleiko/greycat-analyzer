#![allow(clippy::ptr_arg)] // TODO remove (this is for 'errors: &mut Vec<Diagnostic>')

use lsp_types::{Diagnostic, DiagnosticSeverity, Range};

use crate::{TokenKind, ast::*, cst::*, error::ParseError};

type ParserResult<T> = std::result::Result<T, ParseError>;

pub fn parse(name: &str, source: &str, errors: &mut Vec<Diagnostic>) -> ParserResult<Module> {
    let mut parser = CstParser::new(source);
    let root = parser.parse_module(source)?;

    let span = root.span;
    let mut functions = Vec::new();
    let mut pragmas = Vec::new();

    for child in &root.children {
        match child {
            Node::Rule(node) => match node.rule {
                Rule::Function => functions.push(parse_function(source, node, errors)?),
                Rule::PragmaStmt => pragmas.push(parse_pragma_stmt(source, node, errors)?),
                _ => errors.push(Diagnostic {
                    range: node.span.to_range(),
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: format!("unexpected rule '{:?}'", node.rule),
                    ..Default::default()
                }),
            },
            Node::Token(token) => errors.push(Diagnostic {
                range: token.span.to_range(),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("unexpected token '{:?}'", token.kind),
                ..Default::default()
            }),
            Node::Error(err) => errors.push(Diagnostic::from(err)),
        }
    }

    Ok(Module {
        name: name.to_string(),
        pragmas,
        functions,
        span,
    })
}

fn parse_pragma_stmt(
    source: &str,
    node: &NodeRule,
    errors: &mut Vec<Diagnostic>,
) -> ParserResult<Pragma> {
    let mut cursor = node.cursor();
    let _ = cursor.expect_token(TokenKind::At)?;
    let name = cursor.expect_rule(Rule::Name)?;
    let args = cursor.expect_rule(Rule::PragmaArgs)?;
    let args = parse_pragma_args(source, args, errors)?;
    Ok(Pragma {
        name: name.span,
        args: Some(args),
        span: node.span,
    })
}

fn parse_pragma_args(
    source: &str,
    node: &NodeRule,
    errors: &mut Vec<Diagnostic>,
) -> ParserResult<Vec<ConstExpr>> {
    let mut cursor = node.cursor();
    let _ = cursor.expect_token(TokenKind::OpenParen)?;
    let expr = cursor.expect_rule(Rule::Expr)?;
    let arg = parse_const_expr(source, expr, errors)?;
    let _ = cursor.expect_token(TokenKind::CloseParen)?;
    Ok(vec![arg])
}

fn parse_const_expr(
    source: &str,
    node: &NodeRule,
    errors: &mut Vec<Diagnostic>,
) -> ParserResult<ConstExpr> {
    let mut cursor = node.cursor();
    let s = cursor.expect_rule(Rule::String)?;
    let expr = parse_string(source, s, errors)?;
    Ok(ConstExpr::String(expr))
}

fn parse_string(
    source: &str,
    node: &NodeRule,
    errors: &mut Vec<Diagnostic>,
) -> ParserResult<StringLiteral> {
    let mut cursor = node.cursor();
    let _ = cursor.expect_token(TokenKind::DoubleQuote)?;
    let data = cursor.expect_token(TokenKind::RawString)?;
    let _ = cursor.expect_token(TokenKind::DoubleQuote)?;
    Ok(StringLiteral {
        span: node.span,
        text: data.span,
    })
}

fn parse_function(
    source: &str,
    node: &NodeRule,
    errors: &mut Vec<Diagnostic>,
) -> ParserResult<Function> {
    let mut cursor = node.cursor();
    let _ = cursor.expect_token(TokenKind::Ident);
    let name = cursor.expect_rule(Rule::Name)?;
    let params = cursor.expect_rule(Rule::FnParams)?;
    let params = parse_fn_params(source, params, errors)?;
    Ok(Function {
        name: name.span,
        params,
        span: node.span,
    })
}

fn parse_fn_params(
    source: &str,
    node: &NodeRule,
    errors: &mut Vec<Diagnostic>,
) -> ParserResult<FnParams> {
    // TODO
    Ok(FnParams {
        params: vec![],
        span: node.span,
    })
}
