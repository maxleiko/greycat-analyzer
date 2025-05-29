use std::borrow::Cow;

use crate::{
    cst::{Node, NodeKind},
    lexer::{Token, TokenKind},
    span::Span,
};

use super::{Parser, Parser2, TokenExt, error::ParseError};

impl Parser2 {
    // pub fn has_token(&self) -> bool {
    //     self.curr < self.tokens.len()
    // }

    pub(super) fn has_token(&self) -> bool {
        self.lookahead.is_some()
    }

    fn peek_token(&mut self) -> Option<&Token> {
        self.tokens.get(self.curr)
    }

    fn next_token(&mut self) -> Option<Token> {
        self.tokens.get(self.curr).and_then(|tok| {
            self.curr += 1;
            Some(*tok)
        })
    }

    pub(super) fn peek(&mut self) -> Option<&TokenExt> {
        self.lookahead.as_ref()
    }

    pub(super) fn next(&mut self) -> Option<TokenExt> {
        let next = self.lookahead.take();
        self.lookahead = self.bump();
        next
    }

    pub(super) fn bump(&mut self) -> Option<TokenExt> {
        let mut leading = Vec::new();

        // Collect leading trivia
        while let Some(tok) = self.next_token() {
            if tok.kind.is_trivia() {
                leading.push(tok);
            } else {
                // Found main token
                let token = tok;

                // Collect trailing trivia
                let mut trailing = Vec::new();
                while let Some(next) = self.peek_token() {
                    if next.kind.is_trivia() {
                        trailing.push(self.next_token().unwrap());
                    } else {
                        break;
                    }
                }

                return Some(TokenExt {
                    leading,
                    token,
                    trailing,
                });
            }
        }

        None
    }

    pub(super) fn expect(&mut self, kind: TokenKind) -> Result<TokenExt, ParseError> {
        match self.peek() {
            Some(tok) if tok.kind() == kind => Ok(self.next().unwrap()),
            Some(tok) => Err(ParseError::UnexpectedToken(kind, tok.token)),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub(super) fn expect_opt(&mut self, kind: TokenKind) -> Result<Option<TokenExt>, ParseError> {
        match self.peek() {
            Some(tok) if tok.kind() == kind => Ok(self.next()),
            Some(_) => Ok(None),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub(super) fn expect_ident(
        &mut self,
        source: &str,
        ident: &str,
    ) -> Result<Option<TokenExt>, ParseError> {
        match self.peek() {
            Some(tok)
                if tok.kind() == TokenKind::Ident && tok_text(source, &tok.token) == ident =>
            {
                Ok(self.next())
            }
            Some(_) => Ok(None),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub(super) fn expect_ident_n(
        &mut self,
        source: &str,
        idents: &[&str],
    ) -> Result<Option<TokenExt>, ParseError> {
        match self.peek() {
            Some(tok) if tok.kind() == TokenKind::Ident => {
                let current = &source[tok.token.span.as_range(source)];
                for ident in idents {
                    if current == *ident {
                        return Ok(self.next());
                    }
                }
                Ok(None)
            }
            Some(_) => Ok(None),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub(super) fn many_sep<'cst>(
        &mut self,
        source: &str,
        open: TokenKind,
        sep: TokenKind,
        close: TokenKind,
        item_parser: impl Fn(&mut Self, &str) -> Result<Node<'cst>, ParseError>,
        rule_name: &'static str,
    ) -> Result<Node<'cst>, ParseError> {
        let mut children = Vec::new();

        // Parse and collect the opening token
        let open_tok = self.expect(open)?;
        open_tok.merge_into(&mut children);

        enum State {
            First,
            AfterItem,
            AfterSep,
        }

        let mut state = State::First;

        while let Some(tok) = self.peek() {
            match state {
                State::First => {
                    if tok.kind() == close {
                        let close_tok = self.next().unwrap();
                        close_tok.merge_into(&mut children);
                        let span = span_from_nodes(&children);
                        return Ok(Node {
                            kind: NodeKind::Rule(Cow::Borrowed(rule_name)),
                            children,
                            token: None,
                            span,
                        });
                    } else if tok.kind() == sep {
                        let comma = self.next().unwrap();
                        comma.merge_into_as(&mut children, |token| Node {
                            kind: NodeKind::Error(Cow::Borrowed("unexpected separator")),
                            children: Vec::new(),
                            span: token.span,
                            token: Some(token),
                        });
                        state = State::AfterSep;
                    } else {
                        let item = item_parser(self, source)?;
                        children.push(item);
                        state = State::AfterItem;
                    }
                }
                State::AfterItem => {
                    if tok.kind() == close {
                        let close_tok = self.next().unwrap();
                        close_tok.merge_into(&mut children);
                        let span = span_from_nodes(&children);
                        return Ok(Node {
                            kind: NodeKind::Rule(Cow::Borrowed(rule_name)),
                            children,
                            token: None,
                            span,
                        });
                    } else if tok.kind() == sep {
                        let sep_tok = self.next().unwrap();
                        sep_tok.merge_into(&mut children);
                        state = State::AfterSep;
                    } else {
                        children.push(Node {
                            kind: NodeKind::Error(Cow::Borrowed("missing separator")),
                            children: Vec::new(),
                            span: tok.token.span,
                            token: Some(tok.token),
                        });
                        state = State::AfterSep;
                    }
                }
                State::AfterSep => {
                    if tok.kind() == close {
                        let close_tok = self.next().unwrap();
                        close_tok.merge_into(&mut children);
                        let span = span_from_nodes(&children);
                        return Ok(Node {
                            kind: NodeKind::Rule(Cow::Borrowed(rule_name)),
                            children,
                            token: None,
                            span,
                        });
                    } else if tok.kind() == sep {
                        let extra = self.next().unwrap();
                        extra.merge_into_as(&mut children, |token| Node {
                            kind: NodeKind::Error(Cow::Borrowed("unexpected separator")),
                            children: Vec::new(),
                            span: token.span,
                            token: Some(token),
                        });
                    } else {
                        let item = item_parser(self, source)?;
                        children.push(item);
                        state = State::AfterItem;
                    }
                }
            }
        }

        Err(ParseError::UnexpectedEof)
    }

    pub(super) fn error(&mut self, err: ParseError) {
        self.errors.push(err)
    }
}

impl<'src> Parser<'src> {
    pub(super) fn has_token(&self) -> bool {
        self.lookahead.is_some()
    }

    pub(super) fn peek(&mut self) -> Option<&TokenExt> {
        self.lookahead.as_ref()
    }

    pub(super) fn next(&mut self) -> Option<TokenExt> {
        let next = self.lookahead.take();
        self.lookahead = self.bump();
        next
    }

    pub(super) fn bump(&mut self) -> Option<TokenExt> {
        let mut leading = Vec::new();

        // Collect leading trivia
        while let Some(tok) = self.lexer.next() {
            if tok.kind.is_trivia() {
                leading.push(tok);
            } else {
                // Found main token
                let token = tok;

                // Collect trailing trivia
                let mut trailing = Vec::new();
                while let Some(next) = self.lexer.peek() {
                    if next.kind.is_trivia() {
                        trailing.push(self.lexer.next().unwrap());
                    } else {
                        break;
                    }
                }

                return Some(TokenExt {
                    leading,
                    token,
                    trailing,
                });
            }
        }

        None
    }

    pub(super) fn expect(&mut self, kind: TokenKind) -> Result<TokenExt, ParseError> {
        match self.peek() {
            Some(tok) if tok.kind() == kind => Ok(self.next().unwrap()),
            Some(tok) => Err(ParseError::UnexpectedToken(kind, tok.token)),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub(super) fn expect_opt(&mut self, kind: TokenKind) -> Result<Option<TokenExt>, ParseError> {
        match self.peek() {
            Some(tok) if tok.kind() == kind => Ok(self.next()),
            Some(_) => Ok(None),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub(super) fn expect_ident(
        &mut self,
        source: &'src str,
        ident: &str,
    ) -> Result<Option<TokenExt>, ParseError> {
        match self.peek() {
            Some(tok)
                if tok.kind() == TokenKind::Ident && tok_text(source, &tok.token) == ident =>
            {
                Ok(self.next())
            }
            Some(_) => Ok(None),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub(super) fn expect_ident_n(
        &mut self,
        source: &'src str,
        idents: &[&str],
    ) -> Result<Option<TokenExt>, ParseError> {
        match self.peek() {
            Some(tok) if tok.kind() == TokenKind::Ident => {
                let current = &source[tok.token.span.as_range(source)];
                for ident in idents {
                    if current == *ident {
                        return Ok(self.next());
                    }
                }
                Ok(None)
            }
            Some(_) => Ok(None),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub(super) fn many_sep<'cst>(
        &mut self,
        source: &'src str,
        open: TokenKind,
        sep: TokenKind,
        close: TokenKind,
        item_parser: impl Fn(&mut Self, &'src str) -> Result<Node<'cst>, ParseError>,
        rule_name: &'static str,
    ) -> Result<Node<'cst>, ParseError> {
        let mut children = Vec::new();

        // Parse and collect the opening token
        let open_tok = self.expect(open)?;
        open_tok.merge_into(&mut children);

        enum State {
            First,
            AfterItem,
            AfterSep,
        }

        let mut state = State::First;

        while let Some(tok) = self.peek() {
            match state {
                State::First => {
                    if tok.kind() == close {
                        let close_tok = self.next().unwrap();
                        close_tok.merge_into(&mut children);
                        let span = span_from_nodes(&children);
                        return Ok(Node {
                            kind: NodeKind::Rule(Cow::Borrowed(rule_name)),
                            children,
                            token: None,
                            span,
                        });
                    } else if tok.kind() == sep {
                        let comma = self.next().unwrap();
                        comma.merge_into_as(&mut children, |token| Node {
                            kind: NodeKind::Error(Cow::Borrowed("unexpected separator")),
                            children: Vec::new(),
                            span: token.span,
                            token: Some(token),
                        });
                        state = State::AfterSep;
                    } else {
                        let item = item_parser(self, source)?;
                        children.push(item);
                        state = State::AfterItem;
                    }
                }
                State::AfterItem => {
                    if tok.kind() == close {
                        let close_tok = self.next().unwrap();
                        close_tok.merge_into(&mut children);
                        let span = span_from_nodes(&children);
                        return Ok(Node {
                            kind: NodeKind::Rule(Cow::Borrowed(rule_name)),
                            children,
                            token: None,
                            span,
                        });
                    } else if tok.kind() == sep {
                        let sep_tok = self.next().unwrap();
                        sep_tok.merge_into(&mut children);
                        state = State::AfterSep;
                    } else {
                        children.push(Node {
                            kind: NodeKind::Error(Cow::Borrowed("missing separator")),
                            children: Vec::new(),
                            span: tok.token.span,
                            token: Some(tok.token),
                        });
                        state = State::AfterSep;
                    }
                }
                State::AfterSep => {
                    if tok.kind() == close {
                        let close_tok = self.next().unwrap();
                        close_tok.merge_into(&mut children);
                        let span = span_from_nodes(&children);
                        return Ok(Node {
                            kind: NodeKind::Rule(Cow::Borrowed(rule_name)),
                            children,
                            token: None,
                            span,
                        });
                    } else if tok.kind() == sep {
                        let extra = self.next().unwrap();
                        extra.merge_into_as(&mut children, |token| Node {
                            kind: NodeKind::Error(Cow::Borrowed("unexpected separator")),
                            children: Vec::new(),
                            span: token.span,
                            token: Some(token),
                        });
                    } else {
                        let item = item_parser(self, source)?;
                        children.push(item);
                        state = State::AfterItem;
                    }
                }
            }
        }

        Err(ParseError::UnexpectedEof)
    }

    pub(super) fn error(&mut self, err: ParseError) {
        self.errors.push(err)
    }
}

/// Helper to get string slice from token's span in source text
fn tok_text<'src>(source: &'src str, token: &'src Token) -> &'src str {
    &source[token.span.as_range(source)]
}

pub(super) fn span_from_nodes(nodes: &[Node<'_>]) -> Span {
    match (nodes.first(), nodes.last()) {
        (None, None) => Span::default(),
        (Some(first), Some(last)) => Span {
            start: first.span.start,
            end: last.span.end,
        },
        _ => unreachable!(),
    }
}
