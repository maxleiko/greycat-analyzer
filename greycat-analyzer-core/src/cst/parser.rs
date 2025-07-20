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
        match either(module_stmt, SEMI).parse(t) {
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
    let (t, body_or_semi) = either(body, SEMI).parse(t)?;

    let mut node = Node::new(NodeKind::Fn);
    node.add_opt_node(header);
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
    many_sep_bound(
        NodeKind::FnParams,
        OPEN_PAREN,
        map(fn_param, CstNode::Node),
        COMMA,
        CLOSE_PAREN,
    )
    .parse(t)
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
    let (t, ty) = type_ident(t)?;
    node.add_node(ty);
    Ok((t, node))
}

fn type_ident(t: &[Token]) -> Res<Node> {
    let mut node = Node::new(NodeKind::TypeIdent);
    // TODO make it actually parse a complete type_ident
    let (t, name) = IDENT_OR_KW.parse(t)?;
    node.add_tokens2(name);
    Ok((t, node))
}

static IDENT: Matches = matches(TokenKind::Ident);
static SEMI: Matches = matches(TokenKind::Semi);
static COLON: Matches = matches(TokenKind::Colon);
static OPEN_PAREN: Matches = matches(TokenKind::OpenParen);
static CLOSE_PAREN: Matches = matches(TokenKind::CloseParen);
static OPEN_CURLY: Matches = matches(TokenKind::OpenCurly);
static CLOSE_CURLY: Matches = matches(TokenKind::CloseCurly);
static COMMA: Matches = matches(TokenKind::Comma);

static KW_FN: Matches = matches(TokenKind::Fn);
static KW_NATIVE: Matches = matches(TokenKind::Native);
static KW_PRIVATE: Matches = matches(TokenKind::Private);
static KW_STATIC: Matches = matches(TokenKind::Static);
static KW_ABSTRACT: Matches = matches(TokenKind::Abstract);

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

pub fn acc_trivia<'t>(acc: &mut Vec<CstNode>, t: &'t [Token]) -> &'t [Token] {
    let (next, tok) = peek(t);
    let skip = tok.leading.len();
    acc.extend(tok.leading.into_iter().map(CstNode::Token));
    &t[skip..]
}

pub enum ManySepBoundState {
    First,
    AfterItem,
    AfterSep,
}
pub fn many_sep_bound<'t, O, I, S, C>(
    kind: NodeKind,
    open: O,
    item: I,
    sep: S,
    close: C,
) -> impl Parser<'t, Node>
where
    O: Parser<'t, Tokens>,
    I: Parser<'t, CstNode>,
    S: Parser<'t, Tokens>,
    C: Parser<'t, Tokens>,
{
    move |t| {
        let (t, o) = open.parse(t)?;
        let mut node = Node::new(kind);
        node.add_tokens2(o);

        let mut state = ManySepBoundState::First;
        let mut tokens = t;
        loop {
            // consume any trivia "in-between"
            tokens = acc_trivia(&mut node.children, tokens);
            // check for closing bound
            if let Ok((t, c)) = close.parse(tokens) {
                node.add_tokens2(c);
                return Ok((t, node));
            }
            match state {
                ManySepBoundState::First => match sep.parse(tokens) {
                    Ok((t, s)) => {
                        // extra leading separator, record as error
                        let Tokens { leading, token } = s;
                        node.add_tokens(leading);
                        node.add_error(NodeError {
                            kind: ErrorKind::UnexpectedToken,
                            token,
                        });
                        tokens = t;
                        state = ManySepBoundState::AfterSep;
                    }
                    Err(_) => {
                        let (t, i) = item.parse(tokens)?;
                        node.children.push(i);
                        tokens = t;
                        state = ManySepBoundState::AfterItem;
                    }
                },
                ManySepBoundState::AfterItem => match sep.parse(tokens) {
                    Ok((t, s)) => {
                        node.add_tokens2(s);
                        tokens = t;
                        state = ManySepBoundState::AfterSep;
                    }
                    Err(_) => {
                        // we actually expected a separator
                        node.add_error(NodeError {
                            kind: ErrorKind::MissingSeparator,
                            token: t[0], // TODO might prefer to give node.last_token() here might be more accurate
                        });
                        let (t, i) = item.parse(tokens)?;
                        node.children.push(i);
                        tokens = t;
                        state = ManySepBoundState::AfterItem;
                    }
                },
                ManySepBoundState::AfterSep => match item.parse(tokens) {
                    Ok((t, i)) => {
                        node.children.push(i);
                        tokens = t;
                        state = ManySepBoundState::AfterItem;
                    }
                    Err(_) => {
                        node.add_error(NodeError {
                            kind: ErrorKind::UnexpectedToken,
                            token: t[0],
                        });
                        let (t, s) = sep.parse(tokens)?;
                        tokens = t;
                        state = ManySepBoundState::AfterSep;
                    }
                },
            }
        }
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
        let (t, res) = many_sep_bound(
            NodeKind::FnParams,
            OPEN_PAREN,
            map(fn_param, CstNode::Node),
            COMMA,
            CLOSE_PAREN,
        )
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
}
