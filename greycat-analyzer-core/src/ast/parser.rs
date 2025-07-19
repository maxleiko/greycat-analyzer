// #![allow(clippy::ptr_arg)] // TODO remove (this is for 'errors: &mut Vec<Diagnostic>')

// use lsp_types::{Diagnostic, DiagnosticSeverity, Range};

// use crate::{TokenKind, ast::*, cst::*, cursor::Either, error::ParseError};

// type ParserResult<T> = std::result::Result<T, ParseError>;

// pub fn parse(name: &str, source: impl AsRef<str>, errors: &mut Vec<Diagnostic>) -> ParserResult<Module> {
//     let source = source.as_ref();
//     let mut parser = CstParser::new(source);
//     let root = parser.parse_module(source)?;

//     let span = root.span();
//     let mut functions = Vec::new();
//     let mut pragmas = Vec::new();

//     for child in &root.children {
//         match child {
//             CstNode::Node(node) => match node.kind {
//                 NodeKind::Fn => functions.push(parse_function(source, node, errors)?),
//                 NodeKind::PragmaStmt => pragmas.push(parse_pragma_stmt(source, node, errors)?),
//                 _ => errors.push(Diagnostic {
//                     range: node.span().to_range(),
//                     severity: Some(DiagnosticSeverity::ERROR),
//                     message: format!("unexpected rule '{:?}'", node.kind),
//                     ..Default::default()
//                 }),
//             },
//             CstNode::Token(token) => errors.push(Diagnostic {
//                 range: token.span.to_range(),
//                 severity: Some(DiagnosticSeverity::ERROR),
//                 message: format!("unexpected token '{}'", token.kind),
//                 ..Default::default()
//             }),
//             CstNode::Error(err) => errors.push(Diagnostic::from(err)),
//         }
//     }

//     Ok(Module {
//         name: name.to_string(),
//         pragmas,
//         functions,
//         span,
//     })
// }

// fn parse_pragma_stmt(
//     source: &str,
//     node: &Node,
//     errors: &mut Vec<Diagnostic>,
// ) -> ParserResult<Pragma> {
//     let mut cursor = node.cursor();
//     let _ = cursor.expect_token(TokenKind::At)?;
//     let name = cursor.expect_rule(NodeKind::Name)?;
//     match cursor.peek_node() {
//         Some(CstNode::Node(rule)) => {
//             let args = parse_pragma_args(source, rule, errors)?;
//             Ok(Pragma {
//                 name: name.span(),
//                 args: Some(args),
//                 span: node.span(),
//             })
//         }
//         Some(CstNode::Token(token)) if token.kind == TokenKind::Semi => Ok(Pragma {
//             name: name.span(),
//             args: None,
//             span: node.span(),
//         }),
//         Some(CstNode::Token(other)) => {
//             errors.push(Diagnostic {
//                 range: other.span.to_range(),
//                 severity: Some(DiagnosticSeverity::ERROR),
//                 message: format!("Module pragma expects '{}' got '{}'", TokenKind::Semi, other.kind),
//                 ..Default::default()
//             });
//             Ok(Pragma {
//                 name: name.span(),
//                 args: None,
//                 span: node.span(),
//             })
//         }
//         Some(CstNode::Error(err)) => {
//             // TODO this will just discard the rest of the potential errors in this node's children....
//             errors.push(err.into());
//             Ok(Pragma {
//                 name: name.span(),
//                 args: None,
//                 span: node.span(),
//             })
//         }
//         None => {
//             errors.push(Diagnostic {
//                 range: name.span().to_range(),
//                 severity: Some(DiagnosticSeverity::ERROR),
//                 message: format!("Module pragma expects a '{}' at the end", TokenKind::Semi),
//                 ..Default::default()
//             });
//             Ok(Pragma {
//                 name: name.span(),
//                 args: None,
//                 span: node.span(),
//             })
//         }
//     }
// }

// fn parse_pragma_args(
//     source: &str,
//     node: &Node,
//     errors: &mut Vec<Diagnostic>,
// ) -> ParserResult<Vec<ConstExpr>> {
//     let mut cursor = node.cursor();
//     let _ = cursor.expect_token(TokenKind::OpenParen)?;
//     let expr = cursor.expect_rule(NodeKind::Expr)?;
//     let arg = parse_const_expr(source, expr, errors)?;
//     let _ = cursor.expect_token(TokenKind::CloseParen)?;
//     Ok(vec![arg])
// }

// fn parse_const_expr(
//     source: &str,
//     node: &Node,
//     errors: &mut Vec<Diagnostic>,
// ) -> ParserResult<ConstExpr> {
//     let mut cursor = node.cursor();
//     let s = cursor.expect_rule(NodeKind::String)?;
//     let expr = parse_string(source, s, errors)?;
//     Ok(ConstExpr::String(expr))
// }

// fn parse_string(
//     source: &str,
//     node: &Node,
//     errors: &mut Vec<Diagnostic>,
// ) -> ParserResult<StringLiteral> {
//     let mut cursor = node.cursor();
//     let _ = cursor.expect_token(TokenKind::DoubleQuote)?;
//     let data = cursor.expect_token(TokenKind::RawString)?;
//     let _ = cursor.expect_token(TokenKind::DoubleQuote)?;
//     Ok(StringLiteral {
//         span: node.span(),
//         text: data.span,
//     })
// }

// fn parse_function(
//     source: &str,
//     node: &Node,
//     errors: &mut Vec<Diagnostic>,
// ) -> ParserResult<Function> {
//     let mut cursor = node.cursor();
//     let _ = cursor.expect_token(TokenKind::Ident);
//     let name = cursor.expect_rule(NodeKind::Name)?;
//     let params = cursor.expect_rule(NodeKind::FnParams)?;
//     let params = parse_fn_params(source, params, errors)?;
//     Ok(Function {
//         name: name.span(),
//         params,
//         span: node.span(),
//     })
// }

// fn parse_fn_params(
//     source: &str,
//     node: &Node,
//     errors: &mut Vec<Diagnostic>,
// ) -> ParserResult<FnParams> {
//     // TODO
//     Ok(FnParams {
//         params: vec![],
//         span: node.span(),
//     })
// }
