use std::{cell::RefCell, convert::Infallible};

use crate::{
    Token, TokenKind,
    cst::{AddToNode, CstNode, ErrorKind, Node, NodeError, NodeKind, Tokens, combi::*},
};

pub fn parse(mut t: &[Token]) -> Node {
    let mut node = Node::new(NodeKind::Module);
    loop {
        let (next, peeked) = peek(t);
        if peeked.token.kind == TokenKind::Eof {
            node.add(peeked.leading);
            t = next; // 't' should be empty after that because 'Eof'
            break;
        } else {
            let trivia_len = peeked.leading.len();
            node.add(peeked.leading);
            t = &t[trivia_len..]; // consume trivia only
        }
        match either(&module_stmt, &SEMI).parse(t) {
            Ok((next, Either::Left(stmt))) => {
                node.add(stmt);
                t = next;
            }
            Ok((next, Either::Right(semi))) => {
                node.add(semi);
                t = next;
            }
            Err(_) => {
                node.add(NodeError {
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
    one_of(&[&fn_decl, &type_decl, &mod_var_decl, &mod_pragma]).parse(t)
}

fn fn_decl(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t).unwrap();
    let (t, modifiers) = modifiers(t).unwrap();
    let (t, kw) = KW_FN.parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, generics) = opt(generic_params).parse(t).unwrap();
    let (t, params) = fn_params(t)?;
    let (t, return_type) = opt(type_decorator).parse(t).unwrap();
    let (t, body_or_semi) = either(&body, &SEMI).parse(t)?;

    let mut node = Node::new(NodeKind::Fn);
    node.add(header);
    node.add(modifiers);
    node.add(kw);
    node.add(name);
    node.add(generics);
    node.add(params);
    node.add(return_type);
    node.add(body_or_semi);
    Ok((t, node))
}

fn mod_var_decl(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t).unwrap();
    let (t, modifiers) = modifiers(t).unwrap();
    let (t, kw) = KW_VAR.parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, ty) = opt(type_decorator).parse(t).unwrap();
    let (t, init) = opt(initializer).parse(t).unwrap();
    let (t, semi) = opt(SEMI).parse(t).unwrap();

    let mut node = Node::new(NodeKind::ModVarDecl);
    node.add(header);
    node.add(modifiers);
    node.add(kw);
    node.add(name);
    node.add(ty);
    node.add(init);
    node.add(semi);
    Ok((t, node))
}

fn type_decl(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t).unwrap();
    let (t, modifiers) = modifiers(t).unwrap();
    let (t, kw) = KW_TYPE.parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, params) = opt(generic_params).parse(t).unwrap();
    let (t, extend) = opt(type_extends).parse(t).unwrap();
    let (t, body) = type_body(t)?;
    let (t, semi) = opt(SEMI).parse(t).unwrap();

    let mut node = Node::new(NodeKind::TypeDecl);
    node.add(header);
    node.add(modifiers);
    node.add(kw);
    node.add(name);
    node.add(params);
    node.add(extend);
    node.add(body);
    node.add(semi);
    Ok((t, node))
}

fn type_extends(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_EXTENDS.parse(t)?;
    let (t, name) = TYPE_IDENT.parse(t)?;

    let mut node = Node::new(NodeKind::TypeExtends);
    node.add(kw);
    node.add(name);
    Ok((t, node))
}

fn type_body(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_CURLY.parse(t)?;
    let (t, fields) = many(one_of(&[&type_attr, &type_method])).parse(t).unwrap();
    let (t, close) = CLOSE_CURLY.parse(t)?;

    let mut node = Node::new(NodeKind::TypeBody);
    node.add(open);
    node.add(fields);
    node.add(close);
    Ok((t, node))
}

fn type_attr(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header_allow_semi(t)?;
    let (t, modifiers) = modifiers(t)?;
    let (t, name) = ident_or_kw_or_strlit(t)?;
    let (t, colon) = COLON.parse(t)?;
    let (t, ty) = TYPE_IDENT.parse(t)?;
    let (t, init) = opt(initializer).parse(t).unwrap();
    let (t, semi) = opt(SEMI).parse(t).unwrap();

    let mut node = Node::new(NodeKind::TypeAttr);
    node.add(header);
    node.add(modifiers);
    node.add(name);
    node.add(colon);
    node.add(ty);
    node.add(init);
    node.add(semi);
    Ok((t, node))
}

fn mod_pragma(t: &[Token]) -> Res<Node> {
    let (t, doc) = opt(doc).parse(t).unwrap();
    let (t, at) = matches(TokenKind::AtSign).parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, args) = opt(call_args).parse(t).unwrap();
    let (t, semi) = SEMI.parse(t)?;

    let mut node = Node::new(NodeKind::ModPragma);
    node.add(doc);
    node.add(at);
    node.add(name);
    node.add(args);
    node.add(semi);
    Ok((t, node))
}

fn ident_or_kw_or_strlit(t: &[Token]) -> Res<Either<Tokens, Node>> {
    either(&IDENT_OR_KW, &str_expr).parse(t)
}

fn str_expr(t: &[Token]) -> Res<Node> {
    let (t, enter_tpl) = DOUBLE_QUOTE.parse(t)?;
    let (t, opt_raw_string) = opt(RAW_STRING).parse(t).unwrap();
    let (t, exit_tpl) = DOUBLE_QUOTE.parse(t)?;

    let mut node = Node::new(NodeKind::StringExpr);
    node.add(enter_tpl);
    node.add(opt_raw_string);
    node.add(exit_tpl);
    Ok((t, node))
}

fn type_method(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header_allow_semi(t).unwrap();
    let (t, modifiers) = modifiers(t).unwrap();
    let (t, kw) = KW_FN.parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, generics) = opt(generic_params).parse(t).unwrap();
    let (t, params) = fn_params(t)?;
    let (t, return_type) = opt(type_decorator).parse(t).unwrap();
    let (t, body_or_semi) = either(&body, &SEMI).parse(t)?;

    let mut node = Node::new(NodeKind::TypeMethod);
    node.add(header);
    node.add(modifiers);
    node.add(kw);
    node.add(name);
    node.add(generics);
    node.add(params);
    node.add(return_type);
    node.add(body_or_semi);
    Ok((t, node))
}

fn initializer(t: &[Token]) -> Res<Node> {
    let (t, eq) = EQ.parse(t)?;
    let (t, e) = expr(t)?;

    let mut node = Node::new(NodeKind::Initializer);
    node.add(eq);
    node.add(e);
    Ok((t, node))
}

fn expr(t: &[Token]) -> Res<Node> {
    // TODO
    let (t, e) = literal(t)?;
    Ok((t, e))
}

fn literal(t: &[Token]) -> Res<Node> {
    one_of(&[&str_expr]).parse(t)
}

fn name(t: &[Token]) -> Res<Node> {
    let (t, id) = matches(TokenKind::Ident).parse(t)?;
    let mut node = Node::new(NodeKind::Name);
    node.add(id);
    Ok((t, node))
}

fn generic_params(t: &[Token]) -> Res<Node> {
    many_sep_bound(NodeKind::GenericParams, LT, name, COMMA, GT).parse(t)
}

fn type_params(t: &[Token]) -> Res<Node> {
    many_sep_bound(NodeKind::TypeParams, LT, TYPE_IDENT, COMMA, GT).parse(t)
}

fn modifiers(t: &[Token]) -> Res<Option<Node>> {
    let (t, mods) = many(modifier).parse(t).unwrap();
    if let Some(mods) = mods {
        let mut node = Node::new(NodeKind::FnModifiers);
        for modifier in mods {
            let Tokens { leading, token } = modifier;
            node.add(leading);
            node.add(Node {
                kind: NodeKind::FnModifier,
                children: vec![CstNode::Token(token)],
            });
        }
        Ok((t, Some(node)))
    } else {
        Ok((t, None))
    }
}

fn modifier(t: &[Token]) -> Res<Tokens> {
    one_of(&[&KW_NATIVE, &KW_PRIVATE, &KW_STATIC, &KW_ABSTRACT]).parse(t)
}

fn body(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_CURLY.parse(t)?;
    // TODO body stmts
    let (t, close) = CLOSE_CURLY.parse(t)?;
    let mut node = Node::new(NodeKind::Body);
    node.add(open);
    node.add(close);
    Ok((t, node))
}

fn stmt_header(t: &[Token]) -> Res<Option<Node>> {
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

fn stmt_header_allow_semi(t: &[Token]) -> Res<Option<Node>> {
    let (t, items) = many(doc_or_pragma_allow_semi).parse(t).unwrap();
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

fn doc_or_pragma_allow_semi(t: &[Token]) -> Res<Node> {
    alt(doc, pragma_allow_semi).parse(t)
}

fn doc(t: &[Token]) -> Res<Node> {
    let (t, items) = many1(DOC_COMMENT).parse(t)?;
    let mut node = Node::new(NodeKind::Doc);
    node.add(items);
    Ok((t, node))
}

fn pragma(t: &[Token]) -> Res<Node> {
    let (t, at) = matches(TokenKind::AtSign).parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, args) = opt(call_args).parse(t).unwrap();

    let mut node = Node::new(NodeKind::Pragma);
    node.add(at);
    node.add(name);
    node.add(args);
    Ok((t, node))
}

fn pragma_allow_semi(t: &[Token]) -> Res<Node> {
    let (t, mut pragma) = pragma(t)?;
    let (t, semi) = opt(SEMI).parse(t).unwrap();
    pragma.add(semi);
    Ok((t, pragma))
}

fn call_args(t: &[Token]) -> Res<Node> {
    many_sep_bound(NodeKind::CallArgs, OPEN_PAREN, expr, COMMA, CLOSE_PAREN).parse(t)
}

fn fn_params(t: &[Token]) -> Res<Node> {
    many_sep_bound(NodeKind::FnParams, OPEN_PAREN, fn_param, COMMA, CLOSE_PAREN).parse(t)
}

fn fn_param(t: &[Token]) -> Res<Node> {
    let mut node = Node::new(NodeKind::FnParam);
    let (t, name) = IDENT.parse(t)?;
    node.add(name); // TODO don't we want 'ident' token to be its own 'node'?
    let (t, ty) = type_decorator(t)?;
    node.add(ty);
    Ok((t, node))
}

fn type_decorator(t: &[Token]) -> Res<Node> {
    let (t, c) = COLON.parse(t)?;
    let (t, ty) = TYPE_IDENT.parse(t)?;

    let mut node = Node::new(NodeKind::TypeDecorator);
    node.add(c);
    node.add(ty);
    Ok((t, node))
}

#[derive(Clone, Copy)]
struct TypeIdent;

impl<'t> Parser<'t, Node> for TypeIdent {
    fn parse(&self, t: &'t [Token]) -> Res<'t, Node, ParseError> {
        let (t, kw_typeof) = opt(KW_TYPEOF).parse(t).unwrap();
        let (t, parts) = many(seq2(IDENT_OR_KW, COLON_COLON)).parse(t).unwrap();
        let (t, name) = IDENT_OR_KW.parse(t)?;
        let (t, params) = opt(type_params).parse(t).unwrap();
        let (t, qmark) = opt(QMARK).parse(t).unwrap();

        let mut node = Node::new(NodeKind::TypeIdent);
        node.add(kw_typeof);
        node.add(parts);
        node.add(name);
        node.add(params);
        node.add(qmark);
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
static DOC_COMMENT: Matches = matches(TokenKind::DocComment);
static EQ: Matches = matches(TokenKind::Eq);
static DOUBLE_QUOTE: Matches = matches(TokenKind::DoubleQuote);
static RAW_STRING: Matches = matches(TokenKind::RawString);

static KW_FN: Matches = matches(TokenKind::Fn);
static KW_VAR: Matches = matches(TokenKind::Var);
static KW_TYPE: Matches = matches(TokenKind::Type);
static KW_EXTENDS: Matches = matches(TokenKind::Extends);
static KW_ENUM: Matches = matches(TokenKind::Enum);
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

pub enum ManySepBoundState {
    ExpectSep,
    ExpectItem,
}
pub fn many_sep_bound<'t, O, I, S, C, T>(
    kind: NodeKind,
    open: O,
    item: I,
    sep: S,
    close: C,
) -> impl Parser<'t, Node>
where
    O: Parser<'t, Tokens>,
    I: Parser<'t, T>,
    S: Parser<'t, Tokens>,
    C: Parser<'t, Tokens>,
    T: AddToNode,
{
    move |t| {
        let (t, o) = open.parse(t)?;
        let mut node = Node::new(kind);
        node.add(o);

        let mut state = ManySepBoundState::ExpectItem;
        let mut tokens = t;

        // let item_or_sep = either(&self.item, &self.sep);
        loop {
            // consume any trivia "in-between"
            tokens = acc_trivia(&mut node.children, tokens);
            if tokens.len() == 1 {
                // EOF reached
                let err = close.parse(tokens).err().unwrap();
                node.add(NodeError {
                    kind: ErrorKind::UnexpectedToken,
                    token: tokens[0],
                });
                return Err(err);
            }
            // check for closing bound
            if let Ok((t, c)) = close.parse(tokens) {
                node.add(c);
                return Ok((t, node));
            }
            match state {
                ManySepBoundState::ExpectSep => match sep.parse(tokens) {
                    Ok((t, s)) => {
                        node.add(s);
                        tokens = t;
                        state = ManySepBoundState::ExpectItem;
                    }
                    Err(_) => {
                        // we actually expected a separator, record the error
                        node.replace_last_token_error(ErrorKind::MissingToken);
                        let (t, i) = item.parse(tokens)?;
                        node.add(i);
                        tokens = t;
                        state = ManySepBoundState::ExpectSep;
                    }
                },
                ManySepBoundState::ExpectItem => {
                    match item.parse(tokens) {
                        Ok((t, i)) => {
                            node.add(i);
                            tokens = t;
                            state = ManySepBoundState::ExpectSep;
                        }
                        Err(_) => match either(&sep, &close).parse(tokens) {
                            Ok((t, Either::Left(s))) => {
                                node.add(s.leading);
                                node.add(NodeError {
                                    kind: ErrorKind::UnexpectedToken,
                                    token: s.token,
                                });
                                tokens = t;
                                state = ManySepBoundState::ExpectItem;
                            }
                            Ok((t, Either::Right(c))) => {
                                node.add(c.leading);
                                node.add(NodeError {
                                    kind: ErrorKind::UnexpectedToken,
                                    token: c.token,
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

fn seq_n<'t, T>(kind: NodeKind, parsers: &[&dyn Parser<'t, T>]) -> impl Parser<'t, Node>
where
    T: AddToNode,
{
    move |t| {
        let mut node = Node::new(kind);
        let mut tokens = t;
        for parser in parsers {
            let (t, res) = parser.parse(tokens)?;
            res.append_to(&mut node);
            tokens = t;
        }
        Ok((tokens, node))
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

    fn assert_error_kind(node: &CstNode, token_kind: TokenKind, error_kind: ErrorKind) {
        match node {
            CstNode::Error(err) => {
                assert_eq!(err.token.kind, token_kind);
                assert_eq!(err.kind, error_kind);
            }
            other => panic!("Expected CstNode::Err with kind {error_kind:?}, got: {other:?}"),
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
        assert_error_kind(
            &res.children[5],
            TokenKind::Space(1),
            ErrorKind::MissingToken,
        );
        assert_node_kind(&res.children[6], NodeKind::FnParam);
        assert_token_kind(&res.children[7], TokenKind::CloseParen);
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
