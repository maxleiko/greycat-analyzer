use crate::{
    cst::{Node, NodeError, NodeKind, NodeRule},
    lexer::{Token, TokenKind},
    span::Span,
};

use super::{error::ParseError, parser::CstParser, token_ext::TokenExt};

impl<'src> CstParser<'src> {
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
    ) -> Result<TokenExt, ParseError> {
        match self.peek() {
            Some(tok)
                if tok.kind() == TokenKind::Ident && tok_text(source, &tok.token) == ident =>
            {
                Ok(self.next().unwrap())
            }
            Some(tok) => Err(ParseError::ExpectedIdent(ident.to_string(), tok.token)),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub(super) fn expect_ident_n(
        &mut self,
        source: &'src str,
        idents: &[&str],
    ) -> Result<TokenExt, ParseError> {
        match self.peek() {
            Some(tok) if tok.kind() == TokenKind::Ident => {
                let current = &source[tok.token.span.as_range(source)];
                for ident in idents {
                    if current == *ident {
                        return Ok(self.next().unwrap());
                    }
                }
                Err(ParseError::ExpectedIdents(
                    idents.iter().map(|s| s.to_string()).collect(),
                    tok.token,
                ))
            }
            Some(tok) => Err(ParseError::ExpectedIdents(
                idents.iter().map(|s| s.to_string()).collect(),
                tok.token,
            )),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub(super) fn many_sep<'cst>(
        &mut self,
        source: &'src str,
        open: TokenKind,
        sep: TokenKind,
        close: TokenKind,
        item_parser: impl Fn(&mut Self, &'src str) -> Result<Node, ParseError>,
        rule: NodeRule,
    ) -> Result<Node, ParseError> {
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
                        return Ok(Node::Rule {
                            rule,
                            children,
                            span,
                        });
                    } else if tok.kind() == sep {
                        let comma = self.next().unwrap();
                        comma.merge_into_as(&mut children, |token| Node::Error {
                            kind: NodeError::UnexpectedSeparator,
                            token,
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
                        return Ok(Node::Rule {
                            rule,
                            children,
                            span,
                        });
                    } else if tok.kind() == sep {
                        let sep_tok = self.next().unwrap();
                        sep_tok.merge_into(&mut children);
                        state = State::AfterSep;
                    } else {
                        // TODO review the following line as it might have dropped some tokens along the way
                        //      might need to use merge_into_as (not sure)
                        children.push(Node::Error {
                            kind: NodeError::MissingSeparator,
                            token: tok.token,
                        });
                        state = State::AfterSep;
                    }
                }
                State::AfterSep => {
                    if tok.kind() == close {
                        let close_tok = self.next().unwrap();
                        close_tok.merge_into(&mut children);
                        let span = span_from_nodes(&children);
                        return Ok(Node::Rule {
                            rule,
                            children,
                            span,
                        });
                    } else if tok.kind() == sep {
                        let extra = self.next().unwrap();
                        extra.merge_into_as(&mut children, |token| Node::Error {
                            kind: NodeError::UnexpectedSeparator,
                            token,
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
}

/// Helper to get string slice from token's span in source text
fn tok_text<'src>(source: &'src str, token: &'src Token) -> &'src str {
    &source[token.span.as_range(source)]
}

pub(super) fn span_from_nodes(nodes: &[Node]) -> Span {
    match (nodes.first(), nodes.last()) {
        (None, None) => Span::default(),
        (Some(first), Some(last)) => Span {
            start: first.start(),
            end: last.end(),
        },
        _ => unreachable!(),
    }
}
