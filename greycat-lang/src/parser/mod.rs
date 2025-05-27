pub mod error;

use std::borrow::Cow;

use error::ParseError;

use crate::{
    cst::{Node, NodeKind},
    lexer::{Lexer, Token, TokenKind},
    span::{Pos, Span},
};

pub struct Parser<'src> {
    lexer: Lexer<'src>,
    lookahead: Option<Token>,
    errors: Vec<ParseError>,
}

pub struct TokenExt {
    /// Trivia tokens that appear *before* the main token (eg. spaces, newlines, comments)
    leading: Vec<Token>,
    /// The main token itself
    token: Token,
    /// Trivia tokens that appear *after* the main token (eg. spaces, newlines, comments)
    trailing: Vec<Token>,
}

impl<'src> Parser<'src> {
    pub fn new(source: &'src str) -> Self {
        let mut lexer = Lexer::new(source);
        let lookahead = lexer.next();
        Self {
            lexer,
            lookahead,
            errors: Vec::default(),
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.lookahead.as_ref()
    }

    // fn next(&mut self) -> Option<Token> {
    //     let curr = self.lookahead.take();
    //     self.lookahead = self.lexer.next();
    //     curr
    // }

    fn next(&mut self) -> Option<TokenExt> {
        let mut leading = Vec::new();

        // collect all trivia tokens as leading
        while let Some(tok) = self.lookahead.as_ref() {
            if tok.kind.is_trivia() {
                // consume trivia token
                leading.push(self.lookahead.take().unwrap());
                self.lookahead = self.lexer.next();
            } else {
                break;
            }
        }

        // now take the main token (non-trivia)
        let token = self.lookahead.take()?;
        self.lookahead = self.lexer.next();

        let mut trailing = Vec::new();

        // collect all trivia tokens as trailing
        while let Some(tok) = self.lookahead.as_ref() {
            if tok.kind.is_trivia() {
                // consume trivia token
                trailing.push(self.lookahead.take().unwrap());
                self.lookahead = self.lexer.next();
            } else {
                break;
            }
        }

        Some(TokenExt {
            leading,
            token,
            trailing,
        })
    }

    fn expect(&mut self, kind: TokenKind) -> Result<Token, ParseError> {
        match self.peek() {
            Some(tok) if tok.kind == kind => Ok(self.next().unwrap()),
            Some(tok) => Err(ParseError::UnexpectedToken(kind, tok.clone())),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    fn expect_ident(&mut self, source: &'src str, ident: &str) -> Result<Token, ParseError> {
        match self.peek() {
            Some(tok) if tok_text(source, tok) == ident => Ok(self.next().unwrap()),
            Some(tok) => Err(ParseError::ExpectedIdent(ident.to_string(), tok.clone())),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    // fn many_sep<'cst, F, G>(
    //     &mut self,
    //     mut parse_item: F,
    //     mut parse_sep: G,
    // ) -> Result<Vec<Node<'cst>>, ParseError>
    // where
    //     F: FnMut(&mut Self, &'src str) -> Result<Node<'cst>, ParseError>,
    //     G: FnMut(&mut Self) -> Result<Node<'cst>, ParseError>,
    // {
    //     let mut items = Vec::new();

    //     match parse_item(self, source) {
    //         Ok(first) => {
    //             items.push(first);
    //         },
    //         Err(_) => todo!(),
    //     }

    //     todo!()
    // }

    fn error(&mut self, err: ParseError) {
        self.errors.push(err)
    }

    pub fn parse<'cst>(&mut self, source: &'src str) -> Result<Node<'cst>, ParseError> {
        let mut children = Vec::new();

        // TODO parse module statements

        // Build root node with all parsed items
        Ok(Node {
            kind: NodeKind::Rule(std::borrow::Cow::Borrowed("Root")),
            children,
            token: None,
            span: Span {
                start: Pos::new(0, 0),
                end: Pos::new(u32::MAX, u32::MAX),
            },
        })
    }

    fn check_next_is_fn(&mut self, source: &'src str) -> bool {
        let mut iter = self.lexer.clone();

        // expect "fn" or "native"
        match iter.next() {
            Some(tok)
                if tok.kind == TokenKind::Ident
                    && (tok_text(source, &tok) == "fn" || tok_text(source, &tok) == "native") =>
            {
                true
            }
            _ => false,
        }
    }

    fn parse_function<'cst>(&mut self, source: &'src str) -> Result<Node<'cst>, ParseError> {
        let modifiers = self.parse_fn_modifiers(source)?;
        let kw = self.expect_ident(source, "fn")?;
        let name = self.expect(TokenKind::Ident)?;
        let params = self.parse_fn_params(source)?;
        let body = self.parse_fn_body(source)?;

        let mut children = Vec::new();
        let start: Pos;
        let end: Pos;
        if let Some(modifiers) = modifiers {
            start = modifiers.span.start;
            children.push(modifiers);
        } else {
            start = kw.span.start;
        }
        children.push(Node::from(kw));
        children.push(Node::from(name));
        children.push(params);
        Ok(Node {
            kind: NodeKind::Rule(std::borrow::Cow::Owned(format!(
                "Function: {}",
                tok_text(source, &name)
            ))),
            children,
            token: None,
            span: Span { start, end },
        })
    }

    fn parse_fn_modifiers<'cst>(
        &mut self,
        source: &'src str,
    ) -> Result<Option<Node<'cst>>, ParseError> {
        match self.expect(TokenKind::Ident) {
            Ok(tok) => match tok_text(source, &tok) {
                "native" => Ok(Some(tok.into())),
                _ => Ok(None),
            },
            Err(_) => Ok(None),
        }
    }

    fn parse_fn_params<'cst>(&mut self, source: &'src str) -> Result<Node<'cst>, ParseError> {
        let mut children = Vec::new();

        let oparen = self.expect(TokenKind::OpenParen)?;
        let start = oparen.span.start;
        children.push(Node::from(oparen));

        enum State {
            First,
            AfterItem,
            AfterSep,
        }

        let mut state = State::First;
        while let Some(tok) = self.peek() {
            match state {
                State::First => {
                    if tok.kind == TokenKind::CloseParen {
                        let cparen = self.next().unwrap();
                        let end = cparen.span.end;
                        children.push(Node::from(cparen));
                        let node = Node {
                            kind: NodeKind::Rule(Cow::Borrowed("fn_params")),
                            children,
                            token: None,
                            span: Span { start, end },
                        };
                        return Ok(node);
                    } else if tok.kind == TokenKind::Comma {
                        let comma = self.next().unwrap();
                        children.push(Node::from(comma));
                    } else {
                        let fn_param = self.parse_fn_param()?;
                        children.push(fn_param);
                    }
                }
                State::AfterItem => {
                    if tok.kind == TokenKind::CloseParen {
                        let cparen = self.next().unwrap();
                        let end = cparen.span.end;
                        children.push(Node::from(cparen));
                        let node = Node {
                            kind: NodeKind::Rule(Cow::Borrowed("fn_params")),
                            children,
                            token: None,
                            span: Span { start, end },
                        };
                        return Ok(node);
                    } else if tok.kind == TokenKind::Comma {
                        let comma = self.next().unwrap();
                        children.push(Node::from(comma));
                    } else {
                    }
                }
                State::AfterSep => {
                    if tok.kind == TokenKind::CloseParen {
                        let cparen = self.next().unwrap();
                        let end = cparen.span.end;
                        children.push(Node::from(cparen));
                        let node = Node {
                            kind: NodeKind::Rule(Cow::Borrowed("fn_params")),
                            children,
                            token: None,
                            span: Span { start, end },
                        };
                        return Ok(node);
                    } else if tok.kind == TokenKind::Comma {
                        let comma = self.next().unwrap();
                        children.push(Node::from(comma));
                    } else {
                        let fn_param = self.parse_fn_param()?;
                        children.push(fn_param);
                    }
                }
            }
        }

        Err(ParseError::UnexpectedEof)
    }

    fn parse_fn_param<'cst>(&mut self) -> Result<Node<'cst>, ParseError> {
        let name = self.expect(TokenKind::Ident)?;
        let start = name.span.start;
        let colon = self.expect(TokenKind::Colon)?;
        let type_ident = self.parse_type_ident()?;
        let end = type_ident.span.end;
        Ok(Node {
            kind: NodeKind::Rule(Cow::Borrowed("fn_param")),
            children: vec![Node::from(name), Node::from(colon), type_ident],
            token: None,
            span: Span { start, end },
        })
    }

    fn parse_type_ident<'cst>(&mut self) -> Result<Node<'cst>, ParseError> {
        // TODO complete type ident grammar
        let name = self.expect(TokenKind::Ident)?;
        Ok(Node::from(name))
    }

    fn parse_fn_body<'cst>(&mut self, source: &'src str) -> Result<Option<Node<'cst>>, ParseError> {
        todo!()
    }
}

/// Helper to get string slice from token's span in source text
fn tok_text<'src>(source: &'src str, token: &'src Token) -> &'src str {
    &source[token.span.as_range(source)]
}
