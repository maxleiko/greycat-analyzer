use lsp_types::{Diagnostic, DiagnosticSeverity, Range};

use crate::{ast::*, cst::*, error::ParseError};

type ParseResult<T> = std::result::Result<T, ParseError>;

pub fn parse(name: &str, source: &str, errors: &mut Vec<Diagnostic>) -> ParseResult<Module> {
    let mut parser = CstParser::new(source);
    let root = parser.parse_module(source)?;

    let span = root.span();
    let mut functions = Vec::new();
    let mut pragmas = Vec::new();

    match root {
        Node::Rule {
            rule,
            children,
            span,
        } => {
            assert_eq!(rule, NodeRule::Module);
        }
        Node::Token(token) => errors.push(Diagnostic {
            range: token.span.to_range(),
            severity: Some(DiagnosticSeverity::ERROR),
            message: format!("unexpected token '{:?}'", token.kind),
            ..Default::default()
        }),
        Node::Error { kind, token } => errors.push(Diagnostic {
            range: token.span.to_range(),
            severity: Some(DiagnosticSeverity::ERROR),
            message: kind.to_string(),
            ..Default::default()
        }),
    }
    // for child in root.children {
    //     match child.kind {
    //         NodeKind::Rule(NodeRule::PragmaStmt) => {
    //             pragmas.push(parse_pragma_stmt(source, child, errors)?);
    //         }
    //         NodeKind::Rule(NodeRule::Function) => {
    //             functions.push(parse_function(source, child, errors)?);
    //         }
    //         NodeKind::Rule(rule) => todo!(),
    //         NodeKind::Token(kind) => todo!(),
    //         NodeKind::Error(err) => errors.push(Diagnostic {
    //             range: child.span.to_range(),
    //             severity: Some(DiagnosticSeverity::ERROR),
    //             message: err.to_string(),
    //             ..Default::default()
    //         }),
    //     }
    // }

    Ok(Module {
        name: name.to_string(),
        pragmas,
        functions,
        span,
    })
}

fn parse_pragma_stmt(
    source: &str,
    node: Node,
    errors: &mut Vec<Diagnostic>,
) -> ParseResult<Pragma> {
    Ok(Pragma { name: node.span() })
}

fn parse_function(source: &str, node: Node, errors: &mut Vec<Diagnostic>) -> ParseResult<Function> {
    let mut name = Span::default();

    // for child in node.children {
    //     match child.kind {
    //         NodeKind::Rule(NodeRule::Name) => {
    //             name = child.span;
    //         }
    //         NodeKind::Rule(rule) => todo!(),
    //         NodeKind::Token(kind) => todo!(),
    //         NodeKind::Error(err) => errors.push(Diagnostic {
    //             range: child.span.to_range(),
    //             severity: Some(DiagnosticSeverity::ERROR),
    //             message: err.to_string(),
    //             ..Default::default()
    //         }),
    //     }
    // }

    Ok(Function { name })
}
