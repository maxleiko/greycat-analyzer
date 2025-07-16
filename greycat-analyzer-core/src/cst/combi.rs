use crate::{
    cst::{ErrorKind, CstNode, NodeKind},
    lexer::{Token, TokenKind},
    span::Span, Node,
};

use super::{parser::CstParser, error::ParseError, token_ext::TokenExt};

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
        for tok in self.lexer.by_ref() {
            if tok.kind.is_trivia() {
                leading.push(tok);
            } else {
                // Found main token
                let token = tok;

                // Collect trailing trivia
                // let mut trailing = Vec::new();
                // while let Some(next) = self.lexer.peek() {
                //     if next.kind.is_trivia() {
                //         trailing.push(self.lexer.next().unwrap());
                //     } else {
                //         break;
                //     }
                // }

                return Some(TokenExt {
                    leading,
                    token,
                    // trailing,
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
        item_parser: impl Fn(&mut Self, &'src str) -> Result<CstNode, ParseError>,
        rule: NodeKind,
    ) -> Result<CstNode, ParseError> {
        let mut node = Node::new(rule);

        // Parse and collect the opening token
        let open_tok = self.expect(open)?;
        node.add_token_ext(open_tok);

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
                        node.add_token_ext(close_tok);
                        return Ok(CstNode::Node(node));
                    } else if tok.kind() == sep {
                        let comma = self.next().unwrap();
                        node.add_token_ext_as_error(comma, ErrorKind::UnexpectedToken);
                        state = State::AfterSep;
                    } else {
                        let item = item_parser(self, source)?;
                        node.add_node(item);
                        state = State::AfterItem;
                    }
                }
                State::AfterItem => {
                    if tok.kind() == close {
                        let close_tok = self.next().unwrap();
                        node.add_token_ext(close_tok);
                        return Ok(CstNode::Node(node));
                    } else if tok.kind() == sep {
                        let sep_tok = self.next().unwrap();
                        node.add_token_ext(sep_tok);
                        state = State::AfterSep;
                    } else {
                        // TODO review the following line as it might have dropped some tokens along the way
                        //      might need to use merge_into_as (not sure)
                        panic!("problems");
                        node.add_token_ext_as_error(*tok, ErrorKind::MissingSeparator);
                        state = State::AfterSep;
                    }
                }
                State::AfterSep => {
                    if tok.kind() == close {
                        let close_tok = self.next().unwrap();
                        node.add_token_ext(close_tok);
                        return Ok(CstNode::Node(node));
                    } else if tok.kind() == sep {
                        let extra = self.next().unwrap();
                        node.add_token_ext_as_error(extra, ErrorKind::UnexpectedToken);
                    } else {
                        let item = item_parser(self, source)?;
                        node.add_node(item);
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

pub(super) fn span_from_nodes(nodes: &[CstNode]) -> Span {
    match (nodes.first(), nodes.last()) {
        (None, None) => Span::default(),
        (Some(first), Some(last)) => Span {
            start: first.start(),
            end: last.end(),
        },
        _ => unreachable!(),
    }
}
