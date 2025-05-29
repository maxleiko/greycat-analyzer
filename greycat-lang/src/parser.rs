pub mod combi;
pub mod error;

use std::{borrow::Cow, iter::Peekable};

use combi::span_from_nodes;
use error::ParseError;

use crate::{
    cst::{Node, NodeKind},
    lexer::{Lexer, Token, TokenKind, tokenize},
};

type ParserResult<T> = std::result::Result<T, ParseError>;

pub struct Parser2 {
    tokens: Vec<Token>,
    curr: usize,
    lookahead: Option<TokenExt>,
    errors: Vec<ParseError>,
}

impl Parser2 {
    pub fn new(source: &str) -> Self {
        let mut parser = Self {
            tokens: tokenize(source),
            curr: 0,
            lookahead: None,
            errors: Vec::new(),
        };
        parser.lookahead = parser.bump();
        parser
    }

    pub fn parse<'cst>(&mut self, source: &str) -> ParserResult<Node<'cst>> {
        let mut children = Vec::new();

        while self.has_token() {
            let next = self.peek();
            println!(
                "{:?}",
                next.map(|t| (
                    t.kind(),
                    &source[t.token.span.as_range(source)],
                    t.token.span
                ))
            );
            let function = self.parse_function(source)?;
            children.push(function);
        }

        let span = span_from_nodes(&children);
        Ok(Node {
            kind: NodeKind::Rule(std::borrow::Cow::Borrowed("Module")),
            children,
            token: None,
            span,
        })
    }

    fn parse_function<'cst>(&mut self, source: &str) -> ParserResult<Node<'cst>> {
        let modifiers = self.parse_fn_modifiers(source)?;
        let kw = self
            .expect_ident(source, "fn")?
            .ok_or(ParseError::NoMatch)?;
        let name = self.expect(TokenKind::Ident)?;
        let generic_params = self.parse_fn_generic_params(source)?;
        let params = self.parse_fn_params(source)?;
        let return_type = self.parse_fn_return_type(source)?;
        let body = self.parse_fn_body(source)?;

        let mut children = Vec::new();
        if let Some(modifiers) = modifiers {
            children.push(modifiers);
        }
        kw.merge_into(&mut children);
        name.merge_into(&mut children);
        if let Some(generic_params) = generic_params {
            children.push(generic_params);
        }
        children.push(params);
        if let Some(return_type) = return_type {
            children.push(return_type);
        }
        if let Some(body) = body {
            children.push(body);
        }
        let span = span_from_nodes(&children);
        Ok(Node {
            kind: NodeKind::Rule(Cow::Borrowed("function")),
            children,
            token: None,
            span,
        })
    }

    fn parse_fn_modifiers<'cst>(&mut self, source: &str) -> ParserResult<Option<Node<'cst>>> {
        let mut children = Vec::new();
        while let Some(modifier) = self.parse_fn_modifier(source)? {
            modifier.merge_into(&mut children);
        }
        if children.is_empty() {
            return Ok(None);
        }
        let span = span_from_nodes(&children);
        Ok(Some(Node {
            kind: NodeKind::Rule(Cow::Borrowed("fn_modifiers")),
            children,
            token: None,
            span,
        }))
    }

    fn parse_fn_modifier(&mut self, source: &str) -> ParserResult<Option<TokenExt>> {
        self.expect_ident_n(source, &["native"])
    }

    fn parse_fn_generic_params<'cst>(
        &mut self,
        source: &str,
    ) -> ParserResult<Option<Node<'cst>>> {
        if let Some(tok) = self.peek() {
            if tok.kind() != TokenKind::Lt {
                return Ok(None);
            }
        }
        match self.many_sep(
            source,
            TokenKind::Lt,
            TokenKind::Comma,
            TokenKind::Gt,
            Parser2::parse_generic_param,
            "generic_params",
        ) {
            Ok(node) => Ok(Some(node)),
            Err(ParseError::NoMatch) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn parse_generic_param<'cst>(&mut self, _source: &str) -> ParserResult<Node<'cst>> {
        let ident = self.expect(TokenKind::Ident)?;
        let mut children = Vec::new();
        ident.merge_into(&mut children);
        let span = span_from_nodes(&children);
        Ok(Node {
            kind: NodeKind::Rule(Cow::Borrowed("generic_param")),
            children,
            span,
            token: None,
        })
    }

    fn parse_fn_params<'cst>(&mut self, source: &str) -> ParserResult<Node<'cst>> {
        self.many_sep(
            source,
            TokenKind::OpenParen,
            TokenKind::Comma,
            TokenKind::CloseParen,
            Parser2::parse_fn_param,
            "fn_params",
        )
    }

    fn parse_fn_param<'cst>(&mut self, source: &str) -> ParserResult<Node<'cst>> {
        let name = self.expect(TokenKind::Ident)?;
        let colon = self.expect(TokenKind::Colon)?;
        let type_ident = self.parse_type_ident(source)?;
        let mut children = Vec::new();
        name.merge_into(&mut children);
        colon.merge_into(&mut children);
        children.push(type_ident);
        let span = span_from_nodes(&children);
        Ok(Node {
            kind: NodeKind::Rule(Cow::Borrowed("fn_param")),
            children,
            token: None,
            span,
        })
    }

    fn parse_type_ident<'cst>(&mut self, _source: &str) -> ParserResult<Node<'cst>> {
        // TODO complete type ident grammar
        let name = self.expect(TokenKind::Ident)?;
        let mut children = Vec::new();
        name.merge_into(&mut children);
        let span = span_from_nodes(&children);
        Ok(Node {
            kind: NodeKind::Rule(Cow::Borrowed("type_ident")),
            children,
            token: None,
            span,
        })
    }

    fn parse_fn_return_type<'cst>(
        &mut self,
        source: &str,
    ) -> ParserResult<Option<Node<'cst>>> {
        match self.expect_opt(TokenKind::Colon)? {
            Some(colon) => {
                let type_ident = self.parse_type_ident(source)?;
                let mut children = Vec::new();
                colon.merge_into(&mut children);
                children.push(type_ident);
                let span = span_from_nodes(&children);
                let node = Node {
                    kind: NodeKind::Rule(Cow::Borrowed("return_type")),
                    children,
                    span,
                    token: None,
                };
                Ok(Some(node))
            }
            None => Ok(None),
        }
    }

    fn parse_fn_body<'cst>(&mut self, source: &str) -> ParserResult<Option<Node<'cst>>> {
        match self.expect_opt(TokenKind::OpenCurly)? {
            Some(ocurly) => {
                let mut stmts = Vec::new();
                while let Some(tok) = self.peek() {
                    match tok.kind() {
                        TokenKind::CloseCurly => {
                            let ccurly = self.next().unwrap();
                            let mut children = Vec::new();
                            ocurly.merge_into(&mut children);
                            children.extend(stmts);
                            ccurly.merge_into(&mut children);
                            let span = span_from_nodes(&children);
                            let node = Node {
                                kind: NodeKind::Rule(Cow::Borrowed("body")),
                                children,
                                token: None,
                                span,
                            };
                            return Ok(Some(node));
                        }
                        _ => {
                            let stmt = self.parse_body_stmt(source)?;
                            stmts.push(stmt);
                        }
                    }
                }
                Err(ParseError::UnexpectedEof)
            }
            None => Ok(None),
        }
    }

    fn parse_body_stmt<'cst>(&mut self, _source: &str) -> ParserResult<Node<'cst>> {
        // TODO actual body stmt parsing, right now we just eat everything until ';'
        let mut children = Vec::new();
        while let Some(tok) = self.peek() {
            match tok.kind() {
                TokenKind::Semi => {
                    let semi = self.next().unwrap();
                    semi.merge_into(&mut children);
                    let span = span_from_nodes(&children);
                    return Ok(Node {
                        kind: NodeKind::Rule(Cow::Borrowed("body_stmt")),
                        children,
                        token: None,
                        span,
                    });
                }
                _ => {
                    let tok = self.next().unwrap();
                    tok.merge_into(&mut children);
                }
            }
        }
        Err(ParseError::UnexpectedEof)
    }
}

pub struct Parser<'src> {
    lexer: Peekable<Lexer<'src>>,
    lookahead: Option<TokenExt>,
    errors: Vec<ParseError>,
}

impl<'src> Parser<'src> {
    pub fn new(source: &'src str) -> Self {
        let mut parser = Self {
            lexer: Lexer::new(source).peekable(),
            lookahead: None,
            errors: Vec::default(),
        };

        parser.lookahead = parser.bump();
        parser
    }

    pub fn parse<'cst>(&mut self, source: &'src str) -> ParserResult<Node<'cst>> {
        let mut children = Vec::new();

        while self.has_token() {
            let next = self.peek();
            println!(
                "{:?}",
                next.map(|t| (
                    t.kind(),
                    &source[t.token.span.as_range(source)],
                    t.token.span
                ))
            );
            let function = self.parse_function(source)?;
            children.push(function);
        }

        let span = span_from_nodes(&children);
        Ok(Node {
            kind: NodeKind::Rule(std::borrow::Cow::Borrowed("Module")),
            children,
            token: None,
            span,
        })
    }

    fn parse_function<'cst>(&mut self, source: &'src str) -> ParserResult<Node<'cst>> {
        let modifiers = self.parse_fn_modifiers(source)?;
        let kw = self
            .expect_ident(source, "fn")?
            .ok_or(ParseError::NoMatch)?;
        let name = self.expect(TokenKind::Ident)?;
        let generic_params = self.parse_fn_generic_params(source)?;
        let params = self.parse_fn_params(source)?;
        let return_type = self.parse_fn_return_type(source)?;
        let body = self.parse_fn_body(source)?;

        let mut children = Vec::new();
        if let Some(modifiers) = modifiers {
            children.push(modifiers);
        }
        kw.merge_into(&mut children);
        name.merge_into(&mut children);
        if let Some(generic_params) = generic_params {
            children.push(generic_params);
        }
        children.push(params);
        if let Some(return_type) = return_type {
            children.push(return_type);
        }
        if let Some(body) = body {
            children.push(body);
        }
        let span = span_from_nodes(&children);
        Ok(Node {
            kind: NodeKind::Rule(Cow::Borrowed("function")),
            children,
            token: None,
            span,
        })
    }

    fn parse_fn_modifiers<'cst>(&mut self, source: &'src str) -> ParserResult<Option<Node<'cst>>> {
        let mut children = Vec::new();
        while let Some(modifier) = self.parse_fn_modifier(source)? {
            modifier.merge_into(&mut children);
        }
        if children.is_empty() {
            return Ok(None);
        }
        let span = span_from_nodes(&children);
        Ok(Some(Node {
            kind: NodeKind::Rule(Cow::Borrowed("fn_modifiers")),
            children,
            token: None,
            span,
        }))
    }

    fn parse_fn_modifier(&mut self, source: &'src str) -> ParserResult<Option<TokenExt>> {
        self.expect_ident_n(source, &["native"])
    }

    fn parse_fn_generic_params<'cst>(
        &mut self,
        source: &'src str,
    ) -> ParserResult<Option<Node<'cst>>> {
        if let Some(tok) = self.peek() {
            if tok.kind() != TokenKind::Lt {
                return Ok(None);
            }
        }
        match self.many_sep(
            source,
            TokenKind::Lt,
            TokenKind::Comma,
            TokenKind::Gt,
            Parser::parse_generic_param,
            "generic_params",
        ) {
            Ok(node) => Ok(Some(node)),
            Err(ParseError::NoMatch) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn parse_generic_param<'cst>(&mut self, _source: &'src str) -> ParserResult<Node<'cst>> {
        let ident = self.expect(TokenKind::Ident)?;
        let mut children = Vec::new();
        ident.merge_into(&mut children);
        let span = span_from_nodes(&children);
        Ok(Node {
            kind: NodeKind::Rule(Cow::Borrowed("generic_param")),
            children,
            span,
            token: None,
        })
    }

    fn parse_fn_params<'cst>(&mut self, source: &'src str) -> ParserResult<Node<'cst>> {
        self.many_sep(
            source,
            TokenKind::OpenParen,
            TokenKind::Comma,
            TokenKind::CloseParen,
            Parser::parse_fn_param,
            "fn_params",
        )
    }

    fn parse_fn_param<'cst>(&mut self, source: &'src str) -> ParserResult<Node<'cst>> {
        let name = self.expect(TokenKind::Ident)?;
        let colon = self.expect(TokenKind::Colon)?;
        let type_ident = self.parse_type_ident(source)?;
        let mut children = Vec::new();
        name.merge_into(&mut children);
        colon.merge_into(&mut children);
        children.push(type_ident);
        let span = span_from_nodes(&children);
        Ok(Node {
            kind: NodeKind::Rule(Cow::Borrowed("fn_param")),
            children,
            token: None,
            span,
        })
    }

    fn parse_type_ident<'cst>(&mut self, _source: &'src str) -> ParserResult<Node<'cst>> {
        // TODO complete type ident grammar
        let name = self.expect(TokenKind::Ident)?;
        let mut children = Vec::new();
        name.merge_into(&mut children);
        let span = span_from_nodes(&children);
        Ok(Node {
            kind: NodeKind::Rule(Cow::Borrowed("type_ident")),
            children,
            token: None,
            span,
        })
    }

    fn parse_fn_return_type<'cst>(
        &mut self,
        source: &'src str,
    ) -> ParserResult<Option<Node<'cst>>> {
        match self.expect_opt(TokenKind::Colon)? {
            Some(colon) => {
                let type_ident = self.parse_type_ident(source)?;
                let mut children = Vec::new();
                colon.merge_into(&mut children);
                children.push(type_ident);
                let span = span_from_nodes(&children);
                let node = Node {
                    kind: NodeKind::Rule(Cow::Borrowed("return_type")),
                    children,
                    span,
                    token: None,
                };
                Ok(Some(node))
            }
            None => Ok(None),
        }
    }

    fn parse_fn_body<'cst>(&mut self, source: &'src str) -> ParserResult<Option<Node<'cst>>> {
        match self.expect_opt(TokenKind::OpenCurly)? {
            Some(ocurly) => {
                let mut stmts = Vec::new();
                while let Some(tok) = self.peek() {
                    match tok.kind() {
                        TokenKind::CloseCurly => {
                            let ccurly = self.next().unwrap();
                            let mut children = Vec::new();
                            ocurly.merge_into(&mut children);
                            children.extend(stmts);
                            ccurly.merge_into(&mut children);
                            let span = span_from_nodes(&children);
                            let node = Node {
                                kind: NodeKind::Rule(Cow::Borrowed("body")),
                                children,
                                token: None,
                                span,
                            };
                            return Ok(Some(node));
                        }
                        _ => {
                            let stmt = self.parse_body_stmt(source)?;
                            stmts.push(stmt);
                        }
                    }
                }
                Err(ParseError::UnexpectedEof)
            }
            None => Ok(None),
        }
    }

    fn parse_body_stmt<'cst>(&mut self, _source: &'src str) -> ParserResult<Node<'cst>> {
        // TODO actual body stmt parsing, right now we just eat everything until ';'
        let mut children = Vec::new();
        while let Some(tok) = self.peek() {
            match tok.kind() {
                TokenKind::Semi => {
                    let semi = self.next().unwrap();
                    semi.merge_into(&mut children);
                    let span = span_from_nodes(&children);
                    return Ok(Node {
                        kind: NodeKind::Rule(Cow::Borrowed("body_stmt")),
                        children,
                        token: None,
                        span,
                    });
                }
                _ => {
                    let tok = self.next().unwrap();
                    tok.merge_into(&mut children);
                }
            }
        }
        Err(ParseError::UnexpectedEof)
    }
}

#[derive(Debug)]
pub struct TokenExt {
    /// Trivia tokens that appear *before* the main token (eg. spaces, newlines, comments)
    leading: Vec<Token>,
    /// The main token itself
    token: Token,
    /// Trivia tokens that appear *after* the main token (eg. spaces, newlines, comments)
    trailing: Vec<Token>,
}

impl TokenExt {
    fn kind(&self) -> TokenKind {
        self.token.kind
    }

    fn nb_tokens(&self) -> usize {
        self.leading.len() + 1 + self.trailing.len()
    }

    fn merge_into<'cst>(self, children: &mut Vec<Node<'cst>>) {
        children.reserve(self.nb_tokens());
        let TokenExt {
            leading,
            token,
            trailing,
        } = self;
        children.extend(leading.into_iter().map(Node::from));
        children.push(Node::from(token));
        children.extend(trailing.into_iter().map(Node::from));
    }

    fn merge_into_as<'cst, F>(self, children: &mut Vec<Node<'cst>>, as_node: F)
    where
        F: Fn(Token) -> Node<'cst>,
    {
        children.reserve(self.nb_tokens());
        let TokenExt {
            leading,
            token,
            trailing,
        } = self;
        children.extend(leading.into_iter().map(Node::from));
        children.push(as_node(token));
        children.extend(trailing.into_iter().map(Node::from));
    }
}
