use std::convert::Infallible;

use crate::{
    Token, TokenKind,
    cst::combi::*,
    cst::{CstNode, ErrorKind, Node, NodeError, NodeKind, Tokens},
};

pub fn parse(mut t: &[Token]) -> Node {
    let mut node = Node::new(NodeKind::Module);
    loop {
        let (next, peeked) = peek(t);
        if peeked.token.kind == TokenKind::Eof {
            node.add_tokens(peeked.leading);
            t = next; // 't' should be empty after that because 'Eof'
            break;
        } else {
            let trivia_len = peeked.leading.len();
            node.add_tokens(peeked.leading);
            t = &t[trivia_len..]; // consume trivia only
        }
        match either(&module_stmt, &SEMI).parse(t) {
            Ok((next, Either::Left(stmt))) => {
                node.add_node(stmt);
                t = next;
            }
            Ok((next, Either::Right(semi))) => {
                node.add_tokens2(semi);
                t = next;
            }
            Err(_) => {
                node.add_error(NodeError {
                    kind: ErrorKind::UnexpectedToken,
                    token: t[0],
                });
                t = &t[1..]; // advance
            }
        }
    }
    assert!(t.is_empty());
    node
}

fn module_stmt(t: &[Token]) -> Res<Node> {
    one_of(&[fn_decl]).parse(t)
}

fn fn_decl(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t).unwrap();
    let (t, modifiers) = modifiers(t).unwrap();
    let (t, kw) = KW_FN.parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, params) = fn_params(t)?;
    let (t, body_or_semi) = either(&body, &SEMI).parse(t)?;

    let mut node = Node::new(NodeKind::Fn);
    node.add_opt_node(header);
    node.add_opt_node(modifiers);
    node.add_tokens2(kw);
    node.add_tokens2(name);
    node.add_node(params);
    match body_or_semi {
        Either::Left(body) => node.add_node(body),
        Either::Right(semi) => node.add_tokens2(semi),
    }
    Ok((t, node))
}

fn modifiers(t: &[Token]) -> Res<Option<Node>, Infallible> {
    let (t, mods) = many(MODIFIER).parse(t).unwrap();
    if let Some(mods) = mods {
        let mut node = Node::new(NodeKind::FnModifiers);
        for modifier in mods {
            let Tokens { leading, token } = modifier;
            node.add_tokens(leading);
            node.add_node(Node {
                kind: NodeKind::FnModifier,
                children: vec![CstNode::Token(token)],
            });
        }
        Ok((t, Some(node)))
    } else {
        Ok((t, None))
    }
}

fn body(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_CURLY.parse(t)?;
    // TODO body stmts
    let (t, close) = CLOSE_CURLY.parse(t)?;
    let mut node = Node::new(NodeKind::Body);
    node.add_tokens2(open);
    node.add_tokens2(close);
    Ok((t, node))
}

fn stmt_header(t: &[Token]) -> Res<Option<Node>, Infallible> {
    let (t, items) = many(doc_or_pragma).parse(t).unwrap();
    match items {
        Some(items) => {
            let node = Node {
                kind: NodeKind::StmtHeader,
                children: items.into_iter().map(CstNode::Node).collect(),
            };
            Ok((t, Some(node)))
        }
        None => Ok((t, None)),
    }
}

fn doc_or_pragma(t: &[Token]) -> Res<Node> {
    alt(doc, pragma).parse(t)
}

fn doc(t: &[Token]) -> Res<Node> {
    let (t, items) = many1(doc_comment).parse(t)?;
    let mut node = Node::new(NodeKind::Doc);
    node.add_many_tokens(items);
    Ok((t, node))
}

fn doc_comment(t: &[Token]) -> Res<Tokens> {
    matches(TokenKind::DocComment).parse(t)
}

fn pragma(t: &[Token]) -> Res<Node> {
    let mut node = Node::new(NodeKind::Pragma);
    let (t, at) = matches(TokenKind::AtSign).parse(t)?;
    node.add_tokens2(at);
    let (t, name) = IDENT_OR_KW.parse(t)?;
    node.add_tokens2(name);
    // TODO add call_args on pragma
    // let (t, args) = call_args(t)?;
    // node.add_node(CstNode::Node(args));
    Ok((t, node))
}

fn call_args(t: &[Token]) -> Res<Node> {
    todo!()
}

fn fn_params(t: &[Token]) -> Res<Node> {
    many_sep_bound(NodeKind::FnParams, OPEN_PAREN, fn_param, COMMA, CLOSE_PAREN).parse(t)
}

fn fn_param(t: &[Token]) -> Res<Node> {
    let mut node = Node::new(NodeKind::FnParam);
    let (t, name) = IDENT.parse(t)?;
    node.add_tokens2(name); // TODO don't we want 'ident' token to be its own 'node'?
    let (t, ty) = type_decorator(t)?;
    node.add_node(ty);
    Ok((t, node))
}

fn type_decorator(t: &[Token]) -> Res<Node> {
    let mut node = Node::new(NodeKind::TypeDecorator);
    let (t, c) = COLON.parse(t)?;
    node.add_tokens2(c);
    let (t, ty) = TYPE_IDENT.parse(t)?;
    node.add_node(ty);
    Ok((t, node))
}

#[derive(Clone, Copy)]
struct TypeIdent;

impl<'t> Parser<'t, Node> for TypeIdent {
    fn parse(&self, t: &'t [Token]) -> Res<'t, Node, ParseError> {
        let (t, kw_typeof) = opt(KW_TYPEOF).parse(t).unwrap();
        let (t, parts) = many(seq2(IDENT_OR_KW, COLON_COLON)).parse(t).unwrap();
        let (t, name) = IDENT_OR_KW.parse(t)?;
        let (t, params) = opt(TYPE_PARAMS).parse(t).unwrap();
        let (t, qmark) = opt(QMARK).parse(t).unwrap();

        let mut node = Node::new(NodeKind::TypeIdent);
        node.add_opt_tokens2(kw_typeof);
        if let Some(parts) = parts {
            for (id, c) in parts {
                node.add_tokens2(id);
                node.add_tokens2(c);
            }
        }
        node.add_tokens2(name);
        node.add_opt_node(params);
        node.add_opt_tokens2(qmark);
        Ok((t, node))
    }
}

static IDENT: Matches = matches(TokenKind::Ident);
static SEMI: Matches = matches(TokenKind::Semi);
static COLON: Matches = matches(TokenKind::Colon);
static OPEN_PAREN: Matches = matches(TokenKind::OpenParen);
static CLOSE_PAREN: Matches = matches(TokenKind::CloseParen);
static OPEN_CURLY: Matches = matches(TokenKind::OpenCurly);
static CLOSE_CURLY: Matches = matches(TokenKind::CloseCurly);
static COMMA: Matches = matches(TokenKind::Comma);
static COLON_COLON: Matches = matches(TokenKind::ColonColon);
static QMARK: Matches = matches(TokenKind::Question);
static LT: Matches = matches(TokenKind::Lt);
static GT: Matches = matches(TokenKind::Gt);

static KW_FN: Matches = matches(TokenKind::Fn);
static KW_NATIVE: Matches = matches(TokenKind::Native);
static KW_PRIVATE: Matches = matches(TokenKind::Private);
static KW_STATIC: Matches = matches(TokenKind::Static);
static KW_ABSTRACT: Matches = matches(TokenKind::Abstract);
static KW_TYPEOF: Matches = matches(TokenKind::TypeOf);

static KW: MatchesOne<38> = matches_one(
    [
        TokenKind::Abstract,
        TokenKind::As,
        TokenKind::At,
        TokenKind::Break,
        TokenKind::Breakpoint,
        TokenKind::Catch,
        TokenKind::Continue,
        TokenKind::Do,
        TokenKind::Else,
        TokenKind::Enum,
        TokenKind::Extends,
        TokenKind::False,
        TokenKind::For,
        TokenKind::Fn,
        TokenKind::If,
        TokenKind::In,
        TokenKind::Is,
        TokenKind::Limit,
        TokenKind::Native,
        TokenKind::Null,
        TokenKind::NaN,
        TokenKind::Infinity,
        TokenKind::Private,
        TokenKind::Return,
        TokenKind::Sampling,
        TokenKind::Skip,
        TokenKind::Static,
        TokenKind::Task,
        TokenKind::This,
        TokenKind::Throw,
        TokenKind::Try,
        TokenKind::Type,
        TokenKind::True,
        TokenKind::TypeOf,
        TokenKind::Use,
        TokenKind::Var,
        TokenKind::While,
        TokenKind::Without,
    ],
    "a keyword",
);

static MODIFIER: OneOf<'static, Matches> = one_of(&[KW_NATIVE, KW_PRIVATE, KW_STATIC, KW_ABSTRACT]);
static IDENT_OR_KW: MatchesOne<39> = matches_one(
    [
        // Identifier
        TokenKind::Ident,
        // Keywords
        TokenKind::Abstract,
        TokenKind::As,
        TokenKind::At,
        TokenKind::Break,
        TokenKind::Breakpoint,
        TokenKind::Catch,
        TokenKind::Continue,
        TokenKind::Do,
        TokenKind::Else,
        TokenKind::Enum,
        TokenKind::Extends,
        TokenKind::False,
        TokenKind::For,
        TokenKind::Fn,
        TokenKind::If,
        TokenKind::In,
        TokenKind::Is,
        TokenKind::Limit,
        TokenKind::Native,
        TokenKind::Null,
        TokenKind::NaN,
        TokenKind::Infinity,
        TokenKind::Private,
        TokenKind::Return,
        TokenKind::Sampling,
        TokenKind::Skip,
        TokenKind::Static,
        TokenKind::Task,
        TokenKind::This,
        TokenKind::Throw,
        TokenKind::Try,
        TokenKind::Type,
        TokenKind::True,
        TokenKind::TypeOf,
        TokenKind::Use,
        TokenKind::Var,
        TokenKind::While,
        TokenKind::Without,
    ],
    "an identifier",
);
static TYPE_IDENT: TypeIdent = TypeIdent;
static TYPE_PARAMS: ManySepBound<Matches, TypeIdent, Matches, Matches> =
    many_sep_bound(NodeKind::TypeParams, LT, TYPE_IDENT, COMMA, GT);

pub fn acc_trivia<'t>(acc: &mut Vec<CstNode>, t: &'t [Token]) -> &'t [Token] {
    let (next, tok) = peek(t);
    let skip = tok.leading.len();
    acc.extend(tok.leading.into_iter().map(CstNode::Token));
    &t[skip..]
}

#[derive(Clone, Copy)]
pub struct ManySepBound<O, I, S, C> {
    kind: NodeKind,
    open: O,
    item: I,
    sep: S,
    close: C,
}

impl<'t, O, I, S, C> Parser<'t, Node> for ManySepBound<O, I, S, C>
where
    O: Parser<'t, Tokens>,
    I: Parser<'t, Node>,
    S: Parser<'t, Tokens>,
    C: Parser<'t, Tokens>,
{
    fn parse(&self, t: &'t [Token]) -> Res<'t, Node, ParseError> {
        let (t, o) = self.open.parse(t)?;
        let mut node = Node::new(self.kind);
        node.add_tokens2(o);

        let mut state = ManySepBoundState::ExpectItem;
        let mut tokens = t;

        let item_or_sep = either(&self.item, &self.sep);
        loop {
            // consume any trivia "in-between"
            tokens = acc_trivia(&mut node.children, tokens);
            if tokens.len() == 1 {
                // EOF reached
                let err = self.close.parse(tokens).err().unwrap();
                node.add_error(NodeError {
                    kind: ErrorKind::UnexpectedToken,
                    token: tokens[0],
                });
                return Err(err);
            }
            // check for closing bound
            if let Ok((t, c)) = self.close.parse(tokens) {
                node.add_tokens2(c);
                return Ok((t, node));
            }
            match state {
                ManySepBoundState::ExpectSep => match self.sep.parse(tokens) {
                    Ok((t, s)) => {
                        node.add_tokens2(s);
                        tokens = t;
                        state = ManySepBoundState::ExpectItem;
                    }
                    Err(_) => {
                        // we actually expected a separator, record the error
                        node.add_error(NodeError {
                            kind: ErrorKind::MissingSeparator,
                            token: tokens[0], // TODO might prefer to give node.last_token() here might be more accurate
                        });
                        let (t, i) = self.item.parse(tokens)?;
                        node.add_node(i);
                        tokens = t;
                        state = ManySepBoundState::ExpectSep;
                    }
                },
                ManySepBoundState::ExpectItem => {
                    match self.item.parse(tokens) {
                        Ok((t, i)) => {
                            node.add_node(i);
                            tokens = t;
                            state = ManySepBoundState::ExpectSep;
                        }
                        Err(_) => match either(&self.sep, &self.close).parse(tokens) {
                            Ok((t, Either::Left(s))) => {
                                let Tokens { leading, token } = s;
                                node.add_tokens(leading);
                                node.add_error(NodeError {
                                    kind: ErrorKind::UnexpectedToken,
                                    token,
                                });
                                tokens = t;
                                state = ManySepBoundState::ExpectItem;
                            }
                            Ok((t, Either::Right(c))) => {
                                let Tokens { leading, token } = c;
                                node.add_tokens(leading);
                                node.add_error(NodeError {
                                    kind: ErrorKind::UnexpectedToken,
                                    token,
                                });
                                return Ok((t, node));
                            }
                            Err(err) => return Err(err),
                        },
                    }
                    // match item_or_sep.parse(tokens) {
                    //     Ok((t, Either::Left(n))) => {
                    //         node.add_node(n);
                    //         tokens = t;
                    //         state = ManySepBoundState::ExpectSep;
                    //     }
                    //     Ok((t, Either::Right(n))) => {
                    //         // extra leading separator, record as error
                    //         let Tokens { leading, token } = n;
                    //         node.add_tokens(leading);
                    //         node.add_error(NodeError {
                    //             kind: ErrorKind::UnexpectedToken,
                    //             token,
                    //         });
                    //         tokens = t;
                    //         state = ManySepBoundState::ExpectItem;
                    //     }
                    //     Err(_) => {
                    //         node.add_error(NodeError {
                    //             kind: ErrorKind::UnexpectedToken,
                    //             token: tokens[0],
                    //         });
                    //         tokens = &tokens[1..]; // skip one
                    //     }
                    // }
                }
            }
        }
    }
}

pub enum ManySepBoundState {
    ExpectSep,
    ExpectItem,
}
pub const fn many_sep_bound<'t, O, I, S, C>(
    kind: NodeKind,
    open: O,
    item: I,
    sep: S,
    close: C,
) -> ManySepBound<O, I, S, C>
where
    O: Parser<'t, Tokens>,
    I: Parser<'t, Node>,
    S: Parser<'t, Tokens>,
    C: Parser<'t, Tokens>,
{
    ManySepBound {
        kind,
        open,
        item,
        sep,
        close,
    }
}

#[cfg(test)]
mod test {
    use crate::tokenize;

    use super::*;
    use pretty_assertions::assert_eq;

    fn assert_token_kind(node: &CstNode, kind: TokenKind) {
        match node {
            CstNode::Token(token) => assert_eq!(token.kind, kind),
            other => panic!("Expected CstNode::Token with kind {kind:?}, got: {other:?}"),
        }
    }

    fn assert_node_kind(node: &CstNode, kind: NodeKind) {
        match node {
            CstNode::Node(node) => assert_eq!(node.kind, kind),
            other => panic!("Expected CstNode::Node with kind {kind:?}, got: {other:?}"),
        }
    }

    fn assert_error_kind(node: &CstNode, kind: ErrorKind) {
        match node {
            CstNode::Error(err) => assert_eq!(err.kind, kind),
            other => panic!("Expected CstNode::Err with kind {kind:?}, got: {other:?}"),
        }
    }

    #[test]
    fn many_sep_bound_missing_paren() {
        let tokens = tokenize("(a: A, b: B c: C)");
        let (t, res) = many_sep_bound(NodeKind::FnParams, OPEN_PAREN, fn_param, COMMA, CLOSE_PAREN)
            .parse(&tokens)
            .unwrap();
        assert_eq!(res.kind, NodeKind::FnParams);
        assert_token_kind(&res.children[0], TokenKind::OpenParen);
        assert_node_kind(&res.children[1], NodeKind::FnParam);
        assert_token_kind(&res.children[2], TokenKind::Comma);
        assert_token_kind(&res.children[3], TokenKind::Space(1));
        assert_node_kind(&res.children[4], NodeKind::FnParam);
        assert_token_kind(&res.children[5], TokenKind::Space(1));
        assert_error_kind(&res.children[6], ErrorKind::MissingSeparator);
        assert_node_kind(&res.children[7], NodeKind::FnParam);
        assert_token_kind(&res.children[8], TokenKind::CloseParen);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, TokenKind::Eof);
    }

    #[test]
    fn unfinished() {
        let tokens = tokenize("f");
        let module = parse(&tokens);
        println!("{module:#?}");
    }

    #[test]
    fn unfinished_type_ident() {
        let tokens = tokenize("a: Array<");
        let (_, param) = fn_param(&tokens).unwrap();
        println!(
            "{}",
            CstNode::Node(param).to_display_node("a: Array<", true)
        );
    }
}
