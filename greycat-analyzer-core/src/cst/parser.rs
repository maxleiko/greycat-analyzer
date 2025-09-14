use std::{borrow::Cow, convert::Infallible};

use bumpalo::collections::{CollectIn as _, Vec};

use crate::{
    TokenKind,
    cst::{AddToNode, CstNode, ErrorKind, Node, NodeError, NodeKind, Tokens, combi::*},
};

macro_rules! new_node {
    ($arena:expr, $kind:expr, [$($child:expr),* $(,)?]) => {
        {
            // Create the new node
            let mut node = Node::with_capacity($kind, count!($($child),*), $arena);
            // Add each child to the node
            $(
                node.add($child);
            )*
            node
        }
    };
}

// Helper macro to count items at compile time
// Macro that counts the number of arguments by expanding each to 1usize
macro_rules! count {
    // Match any number of expressions separated by commas
    ($($args:expr),*) => {
        {
            // Replace each argument with 1usize and sum them with +
            0usize $(+ { let _ = $args; 1usize })*
        }
    };
}

macro_rules! expected {
    ($expected:expr, $token:expr) => {
        NodeError {
            kind: ErrorKind::Expected {
                expected: $expected.into(),
                got: $token.kind,
            },
            span: $token.span,
        }
    };
}

pub fn parse<'t, 'a>(mut ctx: ParserCtx<'t, 'a>) -> Node<'a> {
    let mut node = Node::with_capacity(NodeKind::Module, 128, ctx.arena);
    loop {
        let (next, peeked) = peek(ctx);
        if peeked.token.kind == TokenKind::Eof {
            node.add(peeked.leading);
            ctx = next; // 't' should be empty after that because 'Eof'
            break;
        } else {
            let trivia_len = peeked.leading.len();
            node.add(peeked.leading);
            ctx.tokens = &ctx.tokens[trivia_len..]; // consume trivia only
        }
        match either(module_stmt, SEMI).parse(ctx) {
            Ok((next, Either::Left(stmt))) => {
                node.add(stmt);
                ctx.tokens = next.tokens;
            }
            Ok((next, Either::Right(semi))) => {
                node.add(semi);
                ctx.tokens = next.tokens;
            }
            Err(_) => {
                node.add(expected!("a module statement", ctx.tokens[0]));
                ctx.tokens = &ctx.tokens[1..]; // advance
            }
        }
    }
    assert!(ctx.tokens.is_empty());
    node
}

fn module_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    one_of(&[&fn_decl, &type_decl, &enum_decl, &mod_var_decl, &mod_pragma]).parse(ctx)
}

fn fn_decl<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, header) = stmt_header(ctx).unwrap();
    let (ctx, modifiers) = opt(modifiers).parse(ctx).unwrap();
    let (ctx, kw) = KW_FN.parse(ctx)?;
    match ident_or_kw(ctx) {
        Ok((ctx, name)) => {
            let (ctx, generics) = opt(generic_params).parse(ctx).unwrap();
            match fn_params(ctx) {
                Ok((ctx, params)) => {
                    let (ctx, return_type) = opt(type_decorator).parse(ctx).unwrap();
                    match either(body, SEMI).parse(ctx) {
                        Ok((ctx, body_or_semi)) => Ok((
                            ctx,
                            new_node!(
                                &ctx.arena,
                                NodeKind::FnDecl,
                                [
                                    header,
                                    modifiers,
                                    kw,
                                    name,
                                    generics,
                                    params,
                                    return_type,
                                    body_or_semi,
                                ]
                            ),
                        )),
                        Err(_) => Ok((
                            ctx,
                            new_node!(
                                &ctx.arena,
                                NodeKind::FnDecl,
                                [
                                    header,
                                    modifiers,
                                    kw,
                                    name,
                                    generics,
                                    params,
                                    return_type,
                                    expected!("function body or semi", ctx.tokens[0])
                                ]
                            ),
                        )),
                    }
                }
                Err(_) => Ok((
                    ctx,
                    new_node!(
                        &ctx.arena,
                        NodeKind::FnDecl,
                        [
                            header,
                            modifiers,
                            kw,
                            name,
                            generics,
                            expected!("function params", ctx.tokens[0])
                        ]
                    ),
                )),
            }
        }
        Err(_) => Ok((
            ctx,
            new_node!(
                &ctx.arena,
                NodeKind::FnDecl,
                [
                    header,
                    modifiers,
                    kw,
                    expected!("function identifier", ctx.tokens[0])
                ]
            ),
        )),
    }
}

fn mod_var_decl<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, header) = stmt_header(ctx).unwrap();
    let (ctx, modifiers) = opt(modifiers).parse(ctx).unwrap();
    let (ctx, kw) = KW_VAR.parse(ctx)?;
    match ident_or_kw(ctx) {
        Ok((ctx, name)) => {
            let (ctx, ty) = opt(type_decorator).parse(ctx).unwrap();
            let (ctx, init) = opt(initializer).parse(ctx).unwrap();
            let (ctx, semi) = expect(SEMI).parse(ctx).unwrap();

            Ok((
                ctx,
                new_node!(
                    &ctx.arena,
                    NodeKind::ModVarDecl,
                    [header, modifiers, kw, name, ty, init, semi]
                ),
            ))
        }
        Err(_) => Ok((
            ctx,
            new_node!(
                &ctx.arena,
                NodeKind::ModVarDecl,
                [
                    header,
                    modifiers,
                    kw,
                    expected!("identifier", ctx.tokens[0])
                ]
            ),
        )),
    }
}

fn enum_decl<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, header) = stmt_header(ctx).unwrap();
    let (ctx, modifiers) = opt(modifiers).parse(ctx).unwrap();
    let (ctx, kw) = KW_ENUM.parse(ctx)?;
    match ident_or_kw(ctx) {
        Ok((ctx, name)) => match enum_body(ctx) {
            Ok((ctx, body)) => {
                let (ctx, semi) = opt(SEMI).parse(ctx).unwrap();

                Ok((
                    ctx,
                    new_node!(
                        &ctx.arena,
                        NodeKind::EnumDecl,
                        [header, modifiers, kw, name, body, semi,]
                    ),
                ))
            }
            Err(_) => Ok((
                ctx,
                new_node!(
                    &ctx.arena,
                    NodeKind::EnumDecl,
                    [
                        header,
                        modifiers,
                        kw,
                        name,
                        expected!("enum body", ctx.tokens[0])
                    ]
                ),
            )),
        },
        Err(_) => Ok((
            ctx,
            new_node!(
                &ctx.arena,
                NodeKind::EnumDecl,
                [
                    header,
                    modifiers,
                    kw,
                    expected!("enum identifier", ctx.tokens[0])
                ]
            ),
        )),
    }
}

fn enum_body<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    many_sep_bound(
        NodeKind::EnumBody,
        OPEN_CURLY,
        enum_field,
        alt(SEMI, COMMA),
        CLOSE_CURLY,
    )
    .parse(ctx)
}

fn enum_field<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, header) = stmt_header(ctx).unwrap();
    let (ctx, name) = ident_or_kw_or_strlit(ctx)?;
    let (ctx, args) = opt(paren_expr).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::EnumField, [header, name, args]),
    ))
}

fn type_decl<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, header) = stmt_header(ctx).unwrap();
    let (ctx, modifiers) = opt(modifiers).parse(ctx).unwrap();
    let (ctx, kw) = KW_TYPE.parse(ctx)?;

    match ident_or_kw(ctx) {
        Ok((ctx, name)) => {
            let (ctx, params) = opt(generic_params).parse(ctx).unwrap();
            let (ctx, extend) = opt(type_extends).parse(ctx).unwrap();
            match type_body(ctx) {
                Ok((ctx, body)) => {
                    let (ctx, semi) = opt(SEMI).parse(ctx).unwrap();
                    Ok((
                        ctx,
                        new_node!(
                            &ctx.arena,
                            NodeKind::TypeDecl,
                            [header, modifiers, kw, name, params, extend, body, semi]
                        ),
                    ))
                }
                Err(_) => Ok((
                    ctx,
                    new_node!(
                        &ctx.arena,
                        NodeKind::TypeDecl,
                        [
                            header,
                            modifiers,
                            kw,
                            name,
                            params,
                            extend,
                            expected!("type body", ctx.tokens[0]),
                        ]
                    ),
                )),
            }
        }
        Err(_) => Ok((
            ctx,
            new_node!(
                &ctx.arena,
                NodeKind::TypeDecl,
                [
                    header,
                    modifiers,
                    kw,
                    expected!("type identifier", ctx.tokens[0])
                ]
            ),
        )),
    }
}

fn type_extends<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_EXTENDS.parse(ctx)?;
    let (ctx, name) = type_ident(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::TypeExtends, [kw, name]),
    ))
}

fn type_body<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let item = either(alt(TypeAttr, TypeMethod), SEMI);
    // let named_item = named_expect("a type field or method", item);
    // TODO need to find a way to put any token in error as long as the closing token is found
    many_bound(NodeKind::TypeBody, OPEN_CURLY, item, CLOSE_CURLY).parse(ctx)
}

struct TypeAttr;

impl<'t, 'a> Parser<'t, 'a, Node<'a>> for TypeAttr {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("a type attribute")
    }

    fn parse(&self, ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>, ParseError> {
        let (ctx, header) = stmt_header_allow_semi(ctx)?;
        let (ctx, modifiers) = opt(modifiers).parse(ctx).unwrap();
        let (ctx, name) = ident_or_kw_or_strlit(ctx)?;
        let (ctx, colon) = COLON.parse(ctx)?;
        let (ctx, ty) = type_ident(ctx)?;
        let (ctx, init) = opt(initializer).parse(ctx).unwrap();
        let (ctx, semi) = expect(SEMI).parse(ctx).unwrap();

        Ok((
            ctx,
            new_node!(
                &ctx.arena,
                NodeKind::TypeAttr,
                [header, modifiers, name, colon, ty, init, semi]
            ),
        ))
    }
}

fn mod_pragma<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, doc) = opt(doc).parse(ctx).unwrap();
    let (ctx, at) = matches(TokenKind::AtSign).parse(ctx)?;
    match ident_or_kw(ctx) {
        Ok((ctx, name)) => {
            let (ctx, args) = opt(call_args).parse(ctx).unwrap();
            let (ctx, semi) = expect(SEMI).parse(ctx).unwrap();

            Ok((
                ctx,
                new_node!(&ctx.arena, NodeKind::ModPragma, [doc, at, name, args, semi]),
            ))
        }
        Err(_) => Ok((
            ctx,
            new_node!(
                &ctx.arena,
                NodeKind::ModPragma,
                [doc, at, expected!("pragma identifier", ctx.tokens[0])]
            ),
        )),
    }
}

fn ident<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, id) = matches(TokenKind::Ident).parse(ctx)?;
    Ok((ctx, new_node!(&ctx.arena, NodeKind::Ident, [id])))
}

fn ident_or_kw<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, id) = IDENT_OR_KW.parse(ctx)?;
    Ok((ctx, new_node!(&ctx.arena, NodeKind::Ident, [id])))
}

fn ident_or_kw_or_strlit<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    alt(
        map(IDENT_OR_KW, |tokens, ctx| {
            new_node!(ctx.arena, NodeKind::Ident, [tokens])
        }),
        map(str_expr, |n, ctx| {
            new_node!(ctx.arena, NodeKind::Ident, [n])
        }),
    )
    .parse(ctx)
}

fn paren_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, open) = OPEN_PAREN.parse(ctx)?;
    let (ctx, expr) = expect(expr).parse(ctx).unwrap();
    let (ctx, close) = CLOSE_PAREN.parse(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::ParenExpr, [open, expr, close]),
    ))
}

fn str_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, enter_tpl) = DOUBLE_QUOTE.parse(ctx)?;
    let (ctx, opt_raw_string) = opt(RAW_STRING).parse(ctx).unwrap();
    let (ctx, exit_tpl) = DOUBLE_QUOTE.parse(ctx)?;

    Ok((
        ctx,
        new_node!(
            &ctx.arena,
            NodeKind::StringExpr,
            [enter_tpl, opt_raw_string, exit_tpl]
        ),
    ))
}

struct TypeMethod;

impl<'t, 'a> Parser<'t, 'a, Node<'a>> for TypeMethod {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("a type method")
    }

    fn parse(&self, ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>, ParseError> {
        let (ctx, header) = stmt_header_allow_semi(ctx).unwrap();
        let (ctx, modifiers) = opt(modifiers).parse(ctx).unwrap();
        let (ctx, kw) = KW_FN.parse(ctx)?;
        let (ctx, name) = IDENT_OR_KW.parse(ctx)?;
        let (ctx, generics) = opt(generic_params).parse(ctx).unwrap();
        let (ctx, params) = fn_params(ctx)?;
        let (ctx, return_type) = opt(type_decorator).parse(ctx).unwrap();
        let (ctx, body) = opt(body).parse(ctx).unwrap();

        Ok((
            ctx,
            new_node!(
                &ctx.arena,
                NodeKind::TypeMethod,
                [
                    header,
                    modifiers,
                    kw,
                    name,
                    generics,
                    params,
                    return_type,
                    body
                ]
            ),
        ))
    }
}

fn initializer<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, eq) = EQ.parse(ctx)?;
    let (ctx, expr) = expr(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::Initializer, [eq, expr]),
    ))
}

fn expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (mut ctx, mut acc) = postfix_expr(ctx)?;

    loop {
        let Ok((next, op_tok)) = binary_op(ctx) else {
            break;
        };
        ctx = next; // advance

        let new_prec = op_tok.token.kind.precedence();

        let op = Node {
            kind: NodeKind::BinaryOperator,
            field_name: Some("op"),
            children: op_tok
                .leading
                .into_iter()
                .map(CstNode::Token)
                .chain(std::iter::once(CstNode::Token(op_tok.token)))
                .collect_in(ctx.arena),
        };

        match postfix_expr(ctx) {
            Ok((next, mut rhs)) => {
                ctx = next; // advance

                match acc.kind {
                    NodeKind::BinaryExpr => {
                        let acc_op = acc
                    .get_node_by_field("op")
                    .expect(
                        "BinaryExpr is always composed of 3 named fields: 'lhs', 'op' and 'rhs'",
                    ).first_non_trivia_token()
                    .expect(
                        "BinaryOperator is always composed of a non-trivia binary operator token",
                    );
                        let mut node = Node::with_capacity(NodeKind::BinaryExpr, 3, ctx.arena);
                        if new_prec <= acc_op.kind.precedence() {
                            acc.field("lhs");
                            node.add(acc);
                            node.add(op);
                            rhs.field("rhs");
                            node.add(rhs);
                        } else {
                            let mut acc_nodes = acc.into_nodes(ctx.arena).into_iter();
                            let acc_lhs = acc_nodes.next().expect("BinaryExpr always have a 'lhs'");
                            let acc_op = acc_nodes.next().expect("BinaryExpr always have a 'op'");
                            let mut acc_rhs =
                                acc_nodes.next().expect("BinaryExpr always have a 'rhs'");
                            // we create a new rhs composed of: (acc.rhs, op, rhs)
                            let mut new_rhs =
                                Node::with_capacity(NodeKind::BinaryExpr, 3, ctx.arena);
                            acc_rhs.field("lhs");
                            new_rhs.add(acc_rhs);
                            new_rhs.add(op);
                            rhs.field("rhs");
                            new_rhs.add(rhs);
                            // and we create the new acc value with: (acc.lhs, acc.op, new_rhs)
                            node.add(acc_lhs);
                            node.add(acc_op);
                            new_rhs.field("rhs");
                            node.add(new_rhs);
                        }
                        acc = node;
                    }
                    _ => {
                        let mut node = Node::with_capacity(NodeKind::BinaryExpr, 3, ctx.arena);
                        acc.field("lhs");
                        node.add(acc);
                        node.add(op);
                        rhs.field("rhs");
                        node.add(rhs);
                        acc = node;
                    }
                }
            }
            Err(_) => {
                // unable to parse rhs: recover
                let mut node = Node::with_capacity(NodeKind::BinaryExpr, 3, ctx.arena);
                acc.field("lhs");
                node.add(acc);
                node.add(op);
                node.add(expected!("an expression", ctx.tokens[0]));
                acc = node;
            }
        }
    }

    Ok((ctx, acc))
}

// fn dump_node(node: &Node) {
//     let mut children = Vec::new();
//     for child in &node.children {
//         children.push(child.to_string());
//     }
//     println!("{:?}={children:?}", node.kind);
// }

fn postfix_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, expr) = prefix_expr(ctx)?;
    match postfix_op(ctx) {
        Ok((ctx, op)) => Ok((
            ctx,
            new_node!(&ctx.arena, NodeKind::PostfixExpr, [expr, op]),
        )),
        Err(_) => Ok((ctx, expr)),
    }
}

fn postfix_op<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Tokens<'a>> {
    alt(PLUS_PLUS, MINUS_MINUS).parse(ctx)
}

fn prefix_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    match prefix_op(ctx) {
        Ok((ctx, op)) => {
            let (ctx, expr) = call_expr(ctx)?;
            Ok((ctx, new_node!(&ctx.arena, NodeKind::PrefixExpr, [op, expr])))
        }
        Err(_) => call_expr(ctx),
    }
}

fn prefix_op<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Tokens<'a>> {
    one_of(&[&BANG, &PLUS, &MINUS, &STAR, &PLUS_PLUS, &MINUS_MINUS]).parse(ctx)
}

fn call_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (mut ctx, mut recv) = primary_expr(ctx)?;

    loop {
        match either(&call_args, &call_expr_spec).parse(ctx) {
            Ok((next, Either::Left(args))) => {
                ctx = next; // advance
                recv = new_node!(&ctx.arena, NodeKind::CallExpr, [recv, args]);
            }
            Ok((next, Either::Right(spec))) => {
                ctx = next; // advance

                let mut node = Node::with_capacity(spec.kind, spec.children.len() + 1, ctx.arena);
                node.add(recv);
                node.children.extend(spec.children);
                recv = node;
            }
            Err(_) => break,
        }
    }

    Ok((ctx, recv))
}

fn call_expr_spec<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    one_of(&[
        &call_args,
        &static_member_expr,
        &member_expr,
        &offset_expr,
        &as_spec,
        &is_spec,
        &nonnull_expr,
        &optional_expr,
    ])
    .parse(ctx)
}

fn static_member_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, sep) = COLON_COLON.parse(ctx)?;
    let (ctx, prop) = ident_or_kw_or_strlit(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::StaticMemberExpr, [sep, prop]),
    ))
}

fn member_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, sep) = either(DOT, ARROW).parse(ctx)?;
    let (ctx, prop) = ident_or_kw_or_strlit(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::MemberExpr, [sep, prop]),
    ))
}

fn offset_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, open) = OPEN_SQUARE.parse(ctx)?;
    let (ctx, expr) = expr(ctx)?;
    let (ctx, close) = CLOSE_SQUARE.parse(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::OffsetExpr, [open, expr, close]),
    ))
}

fn as_spec<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_AS.parse(ctx)?;
    let (ctx, ty) = type_ident(ctx)?;

    Ok((ctx, new_node!(&ctx.arena, NodeKind::AsSpec, [kw, ty])))
}

fn is_spec<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_IS.parse(ctx)?;
    let (ctx, ty) = type_ident(ctx)?;

    Ok((ctx, new_node!(&ctx.arena, NodeKind::IsSpec, [kw, ty])))
}

fn nonnull_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, op) = BANG_BANG.parse(ctx)?;

    Ok((ctx, new_node!(&ctx.arena, NodeKind::NonNullExpr, [op])))
}

fn optional_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, op) = QMARK.parse(ctx)?;

    Ok((ctx, new_node!(&ctx.arena, NodeKind::OptionalExpr, [op])))
}

fn primary_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    one_of(&[
        &literal,
        &lambda_expr,
        &tuple_expr,
        &paren_expr,
        &object_expr,
        &template_expr,
        &array_inline_expr,
        &ident_or_kw_or_strlit,
    ])
    .parse(ctx)
}

fn lambda_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_FN.parse(ctx)?;
    let (ctx, params) = fn_params(ctx)?;
    let (ctx, body) = body(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::LambdaExpr, [kw, params, body]),
    ))
}

fn tuple_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, open) = OPEN_PAREN.parse(ctx)?;
    let (ctx, a) = expr(ctx)?;
    let (ctx, sep) = COMMA.parse(ctx)?;
    let (ctx, b) = expr(ctx)?;
    let (ctx, close) = CLOSE_PAREN.parse(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::TupleExpr, [open, a, sep, b, close]),
    ))
}

fn object_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, ty) = expect(type_ident).parse(ctx).unwrap();
    let (ctx, fields) = object_fields(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::ObjectExpr, [ty, fields]),
    ))
}

fn template_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, enter) = DOUBLE_QUOTE.parse(ctx)?;
    let (ctx, children) = many(either(RAW_STRING, interpolation)).parse(ctx).unwrap();
    let (ctx, exit) = DOUBLE_QUOTE.parse(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::TemplateExpr, [enter, children, exit]),
    ))
}

fn interpolation<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, enter) = ENTER_INTERPOLATION.parse(ctx)?;
    let (ctx, expr) = expr(ctx)?;
    let (ctx, exit) = EXIT_INTERPOLATION.parse(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::Interpolation, [enter, expr, exit]),
    ))
}

fn array_inline_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    many_sep_bound(
        NodeKind::ArrayInlineExpr,
        OPEN_SQUARE,
        expr,
        COMMA,
        CLOSE_SQUARE,
    )
    .parse(ctx)
}

fn literal<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    one_of(&[
        &num_expr, &bool_expr, &char_expr, &str_expr, &nan_expr, &inf_expr, &null_expr, &this_expr,
    ])
    .parse(ctx)
}

fn num_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, num) = NUMBER.parse(ctx)?;
    Ok((ctx, new_node!(&ctx.arena, NodeKind::NumExpr, [num])))
}

fn bool_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, tokens) = alt(KW_TRUE, KW_FALSE).parse(ctx)?;
    Ok((ctx, new_node!(&ctx.arena, NodeKind::BoolExpr, [tokens])))
}

fn char_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, tokens) = alt(CHAR_T, CHAR_U).parse(ctx)?;
    Ok((ctx, new_node!(&ctx.arena, NodeKind::CharExpr, [tokens])))
}

fn nan_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, tokens) = KW_NAN.parse(ctx)?;
    Ok((ctx, new_node!(&ctx.arena, NodeKind::NaNExpr, [tokens])))
}

fn inf_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, tokens) = KW_INFINITY.parse(ctx)?;
    Ok((ctx, new_node!(&ctx.arena, NodeKind::InfExpr, [tokens])))
}

fn null_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, tokens) = KW_NULL.parse(ctx)?;
    Ok((ctx, new_node!(&ctx.arena, NodeKind::NullExpr, [tokens])))
}

fn this_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, tokens) = KW_THIS.parse(ctx)?;
    Ok((ctx, new_node!(&ctx.arena, NodeKind::ThisExpr, [tokens])))
}

fn object_fields<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    many_sep_bound(
        NodeKind::ObjectFields,
        OPEN_CURLY,
        alt(object_field_entry, object_field_expr),
        COMMA,
        CLOSE_CURLY,
    )
    .parse(ctx)
}

fn object_field_entry<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, name) = ident_or_kw_or_strlit(ctx)?;
    let (ctx, sep) = COLON.parse(ctx)?;
    let (ctx, expr) = expr(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::ObjectFieldEntry, [name, sep, expr]),
    ))
}

fn object_field_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, expr) = expr(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::ObjectFieldExpr, [expr]),
    ))
}

fn generic_params<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    many_sep_bound(NodeKind::GenericParams, LT, ident, COMMA, GT).parse(ctx)
}

fn type_params<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    many_sep_bound(NodeKind::TypeParams, LT, type_ident, COMMA, GT).parse(ctx)
}

fn modifiers<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, mods) = many1(modifier).parse(ctx)?;
    let mut node = Node {
        kind: NodeKind::FnModifiers,
        children: Vec::with_capacity_in(
            mods.iter().map(|modifier| modifier.leading.len() + 1).sum(),
            ctx.arena,
        ),
        field_name: Some("modifiers"),
    };
    for modifier in mods {
        let Tokens { leading, token } = modifier;
        node.add(leading);
        node.add(Node {
            kind: NodeKind::FnModifier,
            children: bumpalo::vec![in ctx.arena; token.into()],
            field_name: Some("modifier"),
        });
    }
    Ok((ctx, node))
}

fn modifier<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Tokens<'a>> {
    one_of(&[&KW_NATIVE, &KW_PRIVATE, &KW_STATIC, &KW_ABSTRACT]).parse(ctx)
}

fn body<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    many_bound(NodeKind::Body, OPEN_CURLY, body_stmt, CLOSE_CURLY).parse(ctx)
}

fn body_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Either<Node<'a>, Tokens<'a>>> {
    either(
        one_of(&[
            &var_decl,
            &try_stmt,
            &if_stmt,
            &while_stmt,
            &do_while_stmt,
            &for_in_stmt,
            &for_stmt,
            &at_stmt,
            &throw_stmt,
            &assign_stmt,
            &breakpoint_stmt,
            &break_stmt,
            &continue_stmt,
            &return_stmt,
            &expr_stmt,
        ]),
        SEMI,
    )
    .parse(ctx)
}

fn var_decl<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, header) = stmt_header(ctx)?;
    let (ctx, kw) = KW_VAR.parse(ctx)?;
    let (ctx, name) = IDENT.parse(ctx)?;
    let (ctx, ty) = opt(type_decorator).parse(ctx).unwrap();
    let (ctx, init) = opt(initializer).parse(ctx).unwrap();
    let (ctx, semi) = expect(SEMI).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(
            &ctx.arena,
            NodeKind::VarDecl,
            [header, kw, name, ty, init, semi]
        ),
    ))
}

fn try_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_TRY.parse(ctx)?;
    let (ctx, body) = body(ctx)?;
    let (ctx, catch) = opt(catch_branch).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::TryStmt, [kw, body, catch]),
    ))
}

fn catch_branch<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_CATCH.parse(ctx)?;
    let (ctx, param) = opt(catch_param).parse(ctx).unwrap();
    let (ctx, body) = body(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::CatchBranch, [kw, param, body]),
    ))
}

fn catch_param<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, open) = OPEN_PAREN.parse(ctx)?;
    let (ctx, name) = expect(IDENT).parse(ctx).unwrap();
    let (ctx, close) = CLOSE_PAREN.parse(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::CatchParam, [open, name, close]),
    ))
}

fn if_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_IF.parse(ctx)?;
    let (ctx, condition) = condition(ctx)?;
    let (ctx, then_branch) = body(ctx)?;
    let (ctx, else_if_branches) = many(else_if_branch).parse(ctx).unwrap();
    let (ctx, else_branch) = opt(else_branch).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(
            &ctx.arena,
            NodeKind::IfStmt,
            [kw, condition, then_branch, else_if_branches, else_branch]
        ),
    ))
}

fn condition<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, open) = OPEN_PAREN.parse(ctx)?;
    let (ctx, name) = expect(expr).parse(ctx).unwrap();
    let (ctx, close) = CLOSE_PAREN.parse(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::Condition, [open, name, close]),
    ))
}

fn else_if_branch<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw_else) = KW_ELSE.parse(ctx)?;
    let (ctx, kw_if) = KW_IF.parse(ctx)?;
    let (ctx, condition) = condition(ctx)?;
    let (ctx, branch) = body(ctx)?;

    Ok((
        ctx,
        new_node!(
            &ctx.arena,
            NodeKind::ElseIfBranch,
            [kw_else, kw_if, condition, branch]
        ),
    ))
}

fn else_branch<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_ELSE.parse(ctx)?;
    let (ctx, branch) = body(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::ElseBranch, [kw, branch]),
    ))
}

fn while_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_WHILE.parse(ctx)?;
    let (ctx, condition) = expect(condition).parse(ctx).unwrap();
    let (ctx, body) = expect(body).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::WhileStmt, [kw, condition, body]),
    ))
}

fn do_while_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw_do) = KW_DO.parse(ctx)?;
    let (ctx, body) = expect(body).parse(ctx).unwrap();
    let (ctx, kw_while) = expect(KW_WHILE).parse(ctx).unwrap();
    let (ctx, condition) = expect(condition).parse(ctx).unwrap();
    let (ctx, semi) = expect(SEMI).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(
            &ctx.arena,
            NodeKind::DoWhileStmt,
            [kw_do, body, kw_while, condition, semi]
        ),
    ))
}

fn for_in_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_FOR.parse(ctx)?;
    let (ctx, condition) = for_in_condition(ctx)?;
    let (ctx, body) = expect(body).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::ForInStmt, [kw, condition, body]),
    ))
}

fn for_in_condition<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, open) = OPEN_PAREN.parse(ctx)?;
    let (ctx, key) = for_in_param(ctx)?;
    let (ctx, comma) = COMMA.parse(ctx)?;
    let (ctx, value) = for_in_param(ctx)?;
    let (ctx, kw_in) = expect(KW_IN).parse(ctx).unwrap();
    let (ctx, iter) = expect(expr).parse(ctx).unwrap();
    let (ctx, range) = opt(range).parse(ctx).unwrap();
    let (ctx, filters) = many(for_in_filter).parse(ctx).unwrap();
    let (ctx, close) = CLOSE_PAREN.parse(ctx)?;

    Ok((
        ctx,
        new_node!(
            &ctx.arena,
            NodeKind::ForInCondition,
            [open, key, comma, value, kw_in, iter, range, filters, close]
        ),
    ))
}

fn for_in_param<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, name) = IDENT.parse(ctx)?;
    let (ctx, ty) = opt(type_decorator).parse(ctx).unwrap();

    Ok((ctx, new_node!(&ctx.arena, NodeKind::ForInParam, [name, ty])))
}

fn range<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, open) = RANGE_BOUND.parse(ctx)?;
    let (ctx, lower) = opt(expr).parse(ctx).unwrap();
    let (ctx, dot_dot) = DOT_DOT.parse(ctx)?;
    let (ctx, upper) = opt(expr).parse(ctx).unwrap();
    let (ctx, close) = RANGE_BOUND.parse(ctx)?;

    Ok((
        ctx,
        new_node!(
            &ctx.arena,
            NodeKind::Range,
            [open, lower, dot_dot, upper, close]
        ),
    ))
}

fn for_in_filter<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = FOR_IN_FILTER.parse(ctx)?;
    let (ctx, expr) = expect(expr).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::ForInFilter, [kw, expr]),
    ))
}

fn for_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_FOR.parse(ctx)?;
    let (ctx, condition) = for_condition(ctx)?;
    let (ctx, body) = expect(body).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::ForStmt, [kw, condition, body]),
    ))
}

fn for_condition<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, open) = OPEN_PAREN.parse(ctx)?;
    let (ctx, init) = expect(var_decl).parse(ctx).unwrap();
    let (ctx, condition) = expect(expr_stmt).parse(ctx).unwrap();
    let (ctx, update) = expect(for_expr).parse(ctx).unwrap();
    let (ctx, close) = CLOSE_PAREN.parse(ctx)?;

    Ok((
        ctx,
        new_node!(
            ctx.arena,
            NodeKind::ForCondition,
            [open, init, condition, update, close]
        ),
    ))
}

fn for_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, expr) = alt(assign_expr, expr).parse(ctx)?;

    Ok((ctx, new_node!(&ctx.arena, NodeKind::ForExpr, [expr])))
}

fn at_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_AT.parse(ctx)?;
    let (ctx, expr) = paren_expr(ctx)?;
    let (ctx, body) = expect(body).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::AtStmt, [kw, expr, body]),
    ))
}

fn throw_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_THROW.parse(ctx)?;
    let (ctx, expr) = expect(expr).parse(ctx).unwrap();
    let (ctx, semi) = expect(SEMI).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::ThrowStmt, [kw, expr, semi]),
    ))
}

fn assign_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, expr) = assign_expr(ctx)?;
    let (ctx, semi) = expect(SEMI).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::AssignStmt, [expr, semi]),
    ))
}

fn assign_expr<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, lhs) = expr(ctx)?;
    let (ctx, op) = ASSIGN_OP.parse(ctx)?;
    let (ctx, rhs) = expr(ctx)?;

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::AssignExpr, [lhs, op, rhs]),
    ))
}

fn breakpoint_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_BREAKPOINT.parse(ctx)?;
    let (ctx, semi) = expect(SEMI).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::BreakpointStmt, [kw, semi]),
    ))
}

fn break_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_BREAK.parse(ctx)?;
    let (ctx, semi) = expect(SEMI).parse(ctx).unwrap();

    Ok((ctx, new_node!(&ctx.arena, NodeKind::BreakStmt, [kw, semi])))
}

fn continue_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_CONTINUE.parse(ctx)?;
    let (ctx, semi) = expect(SEMI).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::ContinueStmt, [kw, semi]),
    ))
}

fn return_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw) = KW_RETURN.parse(ctx)?;
    let (ctx, expr) = opt(expr).parse(ctx).unwrap();
    let (ctx, semi) = expect(SEMI).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::ReturnStmt, [kw, expr, semi]),
    ))
}

fn expr_stmt<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, expr) = expr(ctx)?;
    let (ctx, semi) = expect(SEMI).parse(ctx).unwrap();

    Ok((ctx, new_node!(&ctx.arena, NodeKind::ExprStmt, [expr, semi])))
}

fn stmt_header<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Option<Node<'a>>> {
    let (ctx, items) = many(doc_or_pragma).parse(ctx).unwrap();
    match items {
        Some(items) => {
            let node = Node {
                kind: NodeKind::StmtHeader,
                children: items.into_iter().map(CstNode::Node).collect_in(ctx.arena),
                field_name: None,
            };
            Ok((ctx, Some(node)))
        }
        None => Ok((ctx, None)),
    }
}

fn stmt_header_allow_semi<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Option<Node<'a>>> {
    let (ctx, items) = many(doc_or_pragma_allow_semi).parse(ctx).unwrap();
    match items {
        Some(items) => {
            let node = Node {
                kind: NodeKind::StmtHeader,
                children: items.into_iter().map(CstNode::Node).collect_in(ctx.arena),
                field_name: None,
            };
            Ok((ctx, Some(node)))
        }
        None => Ok((ctx, None)),
    }
}

fn doc_or_pragma<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    alt(doc, pragma).parse(ctx)
}

fn doc_or_pragma_allow_semi<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    alt(doc, pragma_allow_semi).parse(ctx)
}

fn doc<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, items) = many1(DOC_COMMENT).parse(ctx)?;

    Ok((ctx, new_node!(&ctx.arena, NodeKind::Doc, [items])))
}

fn pragma<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, at) = matches(TokenKind::AtSign).parse(ctx)?;
    let (ctx, name) = IDENT_OR_KW.parse(ctx)?;
    let (ctx, args) = opt(call_args).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(&ctx.arena, NodeKind::Pragma, [at, name, args]),
    ))
}

fn pragma_allow_semi<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, mut pragma) = pragma(ctx)?;
    let (ctx, semi) = opt(SEMI).parse(ctx).unwrap();
    pragma.add(semi);
    Ok((ctx, pragma))
}

fn call_args<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    many_sep_bound(NodeKind::CallArgs, OPEN_PAREN, expr, COMMA, CLOSE_PAREN).parse(ctx)
}

fn fn_params<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    many_sep_bound(NodeKind::FnParams, OPEN_PAREN, fn_param, COMMA, CLOSE_PAREN).parse(ctx)
}

fn fn_param<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, name) = IDENT.parse(ctx)?;
    let (ctx, ty) = type_decorator(ctx)?;

    Ok((ctx, new_node!(&ctx.arena, NodeKind::FnParam, [name, ty])))
}

fn type_decorator<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, c) = COLON.parse(ctx)?;
    let (ctx, ty) = expect(type_ident).parse(ctx).unwrap();

    Ok((ctx, new_node!(&ctx.arena, NodeKind::TypeDecorator, [c, ty])))
}

fn type_ident<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Node<'a>> {
    let (ctx, kw_typeof) = opt(KW_TYPEOF).parse(ctx).unwrap();
    let (ctx, parts) = many(seq2(IDENT_OR_KW, COLON_COLON)).parse(ctx).unwrap();
    let (ctx, name) = IDENT_OR_KW.parse(ctx)?;
    let (ctx, params) = opt(type_params).parse(ctx).unwrap();
    let (ctx, qmark) = opt(QMARK).parse(ctx).unwrap();

    Ok((
        ctx,
        new_node!(
            &ctx.arena,
            NodeKind::TypeIdent,
            [kw_typeof, parts, name, params, qmark]
        ),
    ))
}

// fn binary_op(ctx: ParserCtx) -> Res<Tokens> {
//     one_of(&[&OR_OP, &AND_OP, &EQ_OP, &REL_OP, &ADD_OP, &MUL_OP, &POW_OP]).parse(ctx)
// }

/// Try operators in precedence order (highest to lowest)
///
/// This way we fail fast on the most common cases
fn binary_op<'t, 'a>(ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Tokens<'a>> {
    // Precedence 13: Exponentiation
    if let Ok(result) = POW_OP.parse(ctx) {
        return Ok(result);
    }

    // Precedence 12: Multiplication, Division, Modulo
    if let Ok(result) = MUL_OP.parse(ctx) {
        return Ok(result);
    }

    // Precedence 11: Addition, Subtraction
    if let Ok(result) = ADD_OP.parse(ctx) {
        return Ok(result);
    }

    // Precedence 9: Relational operators
    if let Ok(result) = REL_OP.parse(ctx) {
        return Ok(result);
    }

    // Precedence 8: Equality operators
    if let Ok(result) = EQ_OP.parse(ctx) {
        return Ok(result);
    }

    // Precedence 4: Logical AND
    if let Ok(result) = AND_OP.parse(ctx) {
        return Ok(result);
    }

    // Precedence 3: Logical OR
    if let Ok(result) = OR_OP.parse(ctx) {
        return Ok(result);
    }

    // If none match, construct a comprehensive error
    let (_, tok) = peek(ctx);
    Err(CustomParseError {
        text: "binary operator",
        got: tok.token,
    }
    .into())
}

struct NamedParser<P> {
    name: Cow<'static, str>,
    parser: P,
}

impl<'t, 'a, T, P: Parser<'t, 'a, T>> Parser<'t, 'a, T> for NamedParser<P> {
    fn name(&self) -> Cow<'static, str> {
        Cow::clone(&self.name)
    }

    fn parse(&self, ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, T, ParseError> {
        self.parser.parse(ctx)
    }
}

pub fn expect<'t, 'a, P, T>(parser: P) -> impl Parser<'t, 'a, Either<T, NodeError>, Infallible>
where
    P: Parser<'t, 'a, T>,
{
    move |ctx| match parser.parse(ctx) {
        Ok((ctx, res)) => Ok((ctx, Either::Left(res))),
        Err(_) => Ok((ctx, Either::Right(expected!(parser.name(), ctx.tokens[0])))),
    }
}

#[inline(always)]
pub fn named_expect<'t, 'a, P, T>(
    expected: &'static str,
    parser: P,
) -> impl Parser<'t, 'a, Either<T, NodeError>, Infallible>
where
    P: Parser<'t, 'a, T>,
{
    expect(NamedParser {
        name: Cow::Borrowed(expected),
        parser,
    })
}

pub fn acc_trivia<'t, 'a>(acc: &mut Vec<'a, CstNode>, ctx: ParserCtx<'t, 'a>) -> ParserCtx<'t, 'a> {
    let ParserCtx { arena, tokens } = ctx;
    let (_next, tok) = peek(ctx);
    let skip = tok.leading.len();
    acc.extend(tok.leading.into_iter().map(CstNode::Token));
    ParserCtx {
        arena,
        tokens: &tokens[skip..],
    }
}

pub fn many_bound<'t, 'a, O, I, C, T, E>(
    kind: NodeKind,
    open: O,
    item: I,
    close: C,
) -> impl Parser<'t, 'a, Node<'a>>
where
    O: Parser<'t, 'a, Tokens<'a>>,
    I: Parser<'t, 'a, T, E>,
    C: Parser<'t, 'a, Tokens<'a>>,
    T: AddToNode<'a>,
{
    move |ctx| {
        let (ctx, o) = open.parse(ctx)?;
        let mut node = Node::with_capacity(kind, 4, ctx.arena);
        node.add(o);

        let mut c = ctx;

        loop {
            // consume any trivia "in-between"
            c = acc_trivia(&mut node.children, c);
            if c.tokens.len() == 1 {
                // EOF reached
                let err = close.parse(c).err().unwrap();
                node.add(expected!("a closing token", c.tokens[0]));
                return Err(err);
            }
            // check for closing bound
            if let Ok((next, close)) = close.parse(c) {
                node.add(close);
                return Ok((next, node));
            }
            match item.parse(c) {
                Ok((next, i)) => {
                    node.add(i);
                    c = next;
                }
                Err(_) => {
                    node.add(expected!(item.name(), c.tokens[0]));
                    c.tokens = &c.tokens[1..];
                }
            }
        }
    }
}

pub enum ManySepBoundState {
    ExpectSep,
    ExpectItem,
}

pub fn many_sep_bound<'t, 'a, O, I, S, C, T>(
    kind: NodeKind,
    open: O,
    item: I,
    sep: S,
    close: C,
) -> impl Parser<'t, 'a, Node<'a>>
where
    O: Parser<'t, 'a, Tokens<'a>> + Copy,
    I: Parser<'t, 'a, T> + Copy,
    S: Parser<'t, 'a, Tokens<'a>> + Copy,
    C: Parser<'t, 'a, Tokens<'a>> + Copy,
    T: AddToNode<'a>,
{
    move |ctx| {
        let (ctx, o) = open.parse(ctx)?;
        let mut node = Node::with_capacity(kind, 4, ctx.arena);
        node.add(o);

        let mut state = ManySepBoundState::ExpectItem;
        let mut c = ctx;

        loop {
            // consume any trivia "in-between"
            c = acc_trivia(&mut node.children, c);
            if c.tokens.len() == 1 {
                // EOF reached
                let err = close.parse(c).err().unwrap();
                node.add(expected!("a closing token", c.tokens[0]));
                return Err(err);
            }
            // check for closing bound
            if let Ok((next, close)) = close.parse(c) {
                node.add(close);
                return Ok((next, node));
            }
            match state {
                ManySepBoundState::ExpectSep => match sep.parse(c) {
                    Ok((next, sep)) => {
                        node.add(sep);
                        c = next;
                        state = ManySepBoundState::ExpectItem;
                    }
                    Err(_) => {
                        // we actually expected a separator, record the error
                        if let Some(last) = node.last_token_mut()
                            && let CstNode::Token(tok) = last
                        {
                            *last = CstNode::Error(expected!("a separator", tok));
                        }
                        let (next, item) = item.parse(c)?;
                        node.add(item);
                        c = next;
                        state = ManySepBoundState::ExpectSep;
                    }
                },
                ManySepBoundState::ExpectItem => match item.parse(c) {
                    Ok((next, item)) => {
                        node.add(item);
                        c = next;
                        state = ManySepBoundState::ExpectSep;
                    }
                    Err(_) => match either(sep, close).parse(c) {
                        Ok((next, Either::Left(sep))) => {
                            node.add(sep.leading);
                            node.add(expected!("a separator", sep.token));
                            c = next;
                            state = ManySepBoundState::ExpectItem;
                        }
                        Ok((next, Either::Right(close))) => {
                            node.add(close.leading);
                            node.add(expected!("a separator", close.token));
                            return Ok((next, node));
                        }
                        Err(err) => return Err(err),
                    },
                },
            }
        }
    }
}

#[allow(unused)]
fn field<'t, 'a, P, E>(name: &'static str, parser: P) -> impl Parser<'t, 'a, Node<'a>, E>
where
    P: Parser<'t, 'a, Node<'a>, E>,
{
    move |ctx| {
        let (ctx, mut node) = parser.parse(ctx)?;
        node.field(name);
        Ok((ctx, node))
    }
}

static IDENT: Matches = matches(TokenKind::Ident);
static SEMI: Matches = matches(TokenKind::Semi);
static COLON: Matches = matches(TokenKind::Colon);
static OPEN_PAREN: Matches = matches(TokenKind::OpenParen);
static CLOSE_PAREN: Matches = matches(TokenKind::CloseParen);
static OPEN_CURLY: Matches = matches(TokenKind::OpenCurly);
static CLOSE_CURLY: Matches = matches(TokenKind::CloseCurly);
static OPEN_SQUARE: Matches = matches(TokenKind::OpenSquare);
static CLOSE_SQUARE: Matches = matches(TokenKind::CloseSquare);
static COMMA: Matches = matches(TokenKind::Comma);
static COLON_COLON: Matches = matches(TokenKind::ColonColon);
static QMARK: Matches = matches(TokenKind::Question);
static LT: Matches = matches(TokenKind::Lt);
static GT: Matches = matches(TokenKind::Gt);
static DOC_COMMENT: Matches = matches(TokenKind::DocComment);
static EQ: Matches = matches(TokenKind::Eq);
static DOUBLE_QUOTE: Matches = matches(TokenKind::DoubleQuote);
static RAW_STRING: Matches = matches(TokenKind::RawString);
static DOT: Matches = matches(TokenKind::Dot);
static DOT_DOT: Matches = matches(TokenKind::DotDot);
static ARROW: Matches = matches(TokenKind::Arrow);
static ENTER_INTERPOLATION: Matches = matches(TokenKind::EnterInterpolation);
static EXIT_INTERPOLATION: Matches = matches(TokenKind::ExitInterpolation);

static RANGE_BOUND: MatchesOne<2> = matches_one(
    [TokenKind::OpenSquare, TokenKind::CloseSquare],
    "'[' or ']'",
);
static FOR_IN_FILTER: MatchesOne<3> = matches_one(
    [TokenKind::Sampling, TokenKind::Limit, TokenKind::Skip],
    "'sampling', 'skip' or 'limit'",
);

static BANG_BANG: Matches = matches(TokenKind::BangBang);
static BANG: Matches = matches(TokenKind::Bang);
static PLUS_PLUS: Matches = matches(TokenKind::PlusPlus);
static MINUS_MINUS: Matches = matches(TokenKind::MinusMinus);
static PLUS: Matches = matches(TokenKind::Plus);
static MINUS: Matches = matches(TokenKind::Minus);
static STAR: Matches = matches(TokenKind::Star);

static OR_OP: MatchesOne<2> = matches_one([TokenKind::OrOr, TokenKind::QuestionQuestion], "or op");
static AND_OP: Matches = matches(TokenKind::AndAnd);
static EQ_OP: MatchesOne<2> = matches_one([TokenKind::EqEq, TokenKind::BangEq], "eq op");
static REL_OP: MatchesOne<4> = matches_one(
    [
        TokenKind::Lt,
        TokenKind::Gt,
        TokenKind::LtEq,
        TokenKind::GtEq,
    ],
    "rel op",
);
static ADD_OP: MatchesOne<2> = matches_one([TokenKind::Plus, TokenKind::Minus], "add op");
static MUL_OP: MatchesOne<3> = matches_one(
    [TokenKind::Star, TokenKind::Slash, TokenKind::Percent],
    "mul op",
);
static POW_OP: Matches = matches(TokenKind::Caret);
static ASSIGN_OP: MatchesOne<2> = matches_one([TokenKind::Eq, TokenKind::QuestionEq], "assign op");

static NUMBER: Matches = matches(TokenKind::Number);
static CHAR_T: Matches = matches(TokenKind::Char { terminated: true });
static CHAR_U: Matches = matches(TokenKind::Char { terminated: false });

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
static KW_THIS: Matches = matches(TokenKind::This);
static KW_NULL: Matches = matches(TokenKind::Null);
static KW_INFINITY: Matches = matches(TokenKind::Infinity);
static KW_NAN: Matches = matches(TokenKind::NaN);
static KW_TRUE: Matches = matches(TokenKind::True);
static KW_FALSE: Matches = matches(TokenKind::False);
static KW_AS: Matches = matches(TokenKind::As);
static KW_IS: Matches = matches(TokenKind::Is);
static KW_TRY: Matches = matches(TokenKind::Try);
static KW_CATCH: Matches = matches(TokenKind::Catch);
static KW_IF: Matches = matches(TokenKind::If);
static KW_ELSE: Matches = matches(TokenKind::Else);
static KW_WHILE: Matches = matches(TokenKind::While);
static KW_DO: Matches = matches(TokenKind::Do);
static KW_FOR: Matches = matches(TokenKind::For);
static KW_IN: Matches = matches(TokenKind::In);
static KW_AT: Matches = matches(TokenKind::At);
static KW_THROW: Matches = matches(TokenKind::Throw);
static KW_BREAKPOINT: Matches = matches(TokenKind::Breakpoint);
static KW_BREAK: Matches = matches(TokenKind::Break);
static KW_CONTINUE: Matches = matches(TokenKind::Continue);
static KW_RETURN: Matches = matches(TokenKind::Return);

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

#[cfg(test)]
mod test {
    use crate::tokenize;

    use super::*;
    use bumpalo::Bump;
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
                assert_eq!(err.got(), token_kind);
                assert_eq!(err.kind, error_kind);
            }
            other => panic!("Expected CstNode::Err with kind {error_kind:?}, got: {other:?}"),
        }
    }

    #[test]
    fn many_sep_bound_missing_paren() {
        let arena = Bump::new();
        let ctx = ParserCtx {
            arena: &arena,
            tokens: &tokenize("(a: A, b: B c: C)"),
        };
        let (ctx, res) =
            many_sep_bound(NodeKind::FnParams, OPEN_PAREN, fn_param, COMMA, CLOSE_PAREN)
                .parse(ctx)
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
            ErrorKind::Expected {
                expected: "a separator".into(),
                got: TokenKind::Space(1),
            },
        );
        assert_node_kind(&res.children[6], NodeKind::FnParam);
        assert_token_kind(&res.children[7], TokenKind::CloseParen);
        assert_eq!(ctx.tokens.len(), 1);
        assert_eq!(ctx.tokens[0].kind, TokenKind::Eof);
    }

    #[test]
    fn unfinished() {
        let arena = Bump::new();
        let ctx = ParserCtx {
            arena: &arena,
            tokens: &tokenize("f"),
        };
        let module = parse(ctx);
        println!("{module:#?}");
    }

    #[test]
    fn unfinished_type_ident() {
        let arena = Bump::new();
        let ctx = ParserCtx {
            arena: &arena,
            tokens: &tokenize("a: Array<"),
        };
        let (_, param) = fn_param(ctx).unwrap();
        println!(
            "{}",
            CstNode::Node(param).to_display_node("a: Array<", true)
        );
    }

    #[test]
    fn empty_fn_decl() {
        let arena = Bump::new();
        let ctx = ParserCtx {
            arena: &arena,
            tokens: &tokenize("fn main() {}"),
        };
        let res = fn_decl(ctx);
        assert!(res.is_ok());
    }

    #[test]
    fn fn_body_expr_stmt() {
        let arena = Bump::new();
        let ctx = ParserCtx {
            arena: &arena,
            tokens: &tokenize("{ a; }"),
        };
        let res = body(ctx);
        assert!(res.is_ok());
    }

    #[test]
    fn expected_expr_in_paren() {
        let source = r#"fn main() {
    var x = ();
}"#;
        let arena = Bump::new();
        let ctx = ParserCtx {
            arena: &arena,
            tokens: &tokenize(source),
        };
        let res = fn_decl(ctx);
        assert!(res.is_ok());
    }

    #[test]
    fn bin_expr_precedence() {
        let source = "3 + 4 * 2 / (1 - 5) ^ 2 ^ 3";
        let arena = Bump::new();
        let ctx = ParserCtx {
            arena: &arena,
            tokens: &tokenize(source),
        };
        let (_, node) = expr(ctx).unwrap();
        let sexpr = node.to_display_node(source, false).to_string();
        assert_eq!(
            sexpr,
            r#"(BinaryExpr
  (NumExpr
    (Number)
  )
  (BinaryOperator
    (Plus)
  )
  (BinaryExpr
    (BinaryExpr
      (BinaryExpr
        (BinaryExpr
          (NumExpr
            (Number)
          )
          (BinaryOperator
            (Star)
          )
          (NumExpr
            (Number)
          )
        )
        (BinaryOperator
          (Slash)
        )
        (ParenExpr
          (OpenParen)
          (BinaryExpr
            (NumExpr
              (Number)
            )
            (BinaryOperator
              (Minus)
            )
            (NumExpr
              (Number)
            )
          )
          (CloseParen)
        )
      )
      (BinaryOperator
        (Caret)
      )
      (NumExpr
        (Number)
      )
    )
    (BinaryOperator
      (Caret)
    )
    (NumExpr
      (Number)
    )
  )
)
"#
        );
    }
}
