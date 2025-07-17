use crate::{
    Node, NodeKind, Token, TokenKind,
    cst2::combi::{Parser as _, Res, Tokens, matches},
};

pub fn ident(t: &[Token]) -> Res<Tokens> {
    matches(TokenKind::Ident).parse(t)
}
