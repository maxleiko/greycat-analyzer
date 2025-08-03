use std::convert::Infallible;

use crate::{
    Token, TokenKind,
    cst::{AddToNode, CstNode, ErrorKind, Node, NodeError, NodeKind, Tokens, combi::*},
};

macro_rules! new_node {
    ($kind:expr, [$($child:expr),* $(,)?]) => {
        {
            // Create the new node
            let mut node = Node::with_capacity($kind, count!($($child),*));
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

pub fn parse(mut t: &[Token]) -> Node {
    let mut node = Node::with_capacity(NodeKind::Module, 128);
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
        match either(module_stmt, SEMI).parse(t) {
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
                    kind: ErrorKind::Expected {
                        expected: "a module statement",
                        got: t[0].kind,
                    },
                    span: t[0].span,
                });
                t = &t[1..]; // advance
            }
        }
    }
    assert!(t.is_empty());
    node
}

fn module_stmt(t: &[Token]) -> Res<Node> {
    one_of(&[&fn_decl, &type_decl, &enum_decl, &mod_var_decl, &mod_pragma]).parse(t)
}

fn fn_decl(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t).unwrap();
    let (t, modifiers) = opt(modifiers).parse(t).unwrap();
    let (t, kw) = KW_FN.parse(t)?;
    let (t, name) = ident_or_kw(t)?;
    let (t, generics) = opt(generic_params).parse(t).unwrap();
    let (t, params) = fn_params(t)?;
    let (t, return_type) = opt(type_decorator).parse(t).unwrap();
    let (t, body_or_semi) = either(body, SEMI).parse(t)?;

    Ok((
        t,
        new_node!(
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
    ))
}

fn mod_var_decl(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t).unwrap();
    let (t, modifiers) = opt(modifiers).parse(t).unwrap();
    let (t, kw) = KW_VAR.parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, ty) = opt(type_decorator).parse(t).unwrap();
    let (t, init) = opt(initializer).parse(t).unwrap();
    let (t, semi) = opt(SEMI).parse(t).unwrap();

    Ok((
        t,
        new_node!(
            NodeKind::ModVarDecl,
            [header, modifiers, kw, name, ty, init, semi,]
        ),
    ))
}

fn enum_decl(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t).unwrap();
    let (t, modifiers) = opt(modifiers).parse(t).unwrap();
    let (t, kw) = KW_ENUM.parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, body) = enum_body(t)?;
    let (t, semi) = opt(SEMI).parse(t).unwrap();

    Ok((
        t,
        new_node!(
            NodeKind::EnumDecl,
            [header, modifiers, kw, name, body, semi,]
        ),
    ))
}

fn enum_body(t: &[Token]) -> Res<Node> {
    many_sep_bound(
        NodeKind::EnumBody,
        OPEN_CURLY,
        enum_field,
        alt(SEMI, COMMA),
        CLOSE_CURLY,
    )
    .parse(t)
}

fn enum_field(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t).unwrap();
    let (t, name) = ident_or_kw_or_strlit(t)?;
    let (t, args) = opt(paren_expr).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::EnumField, [header, name, args])))
}

fn type_decl(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t).unwrap();
    let (t, modifiers) = opt(modifiers).parse(t).unwrap();
    let (t, kw) = KW_TYPE.parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, params) = opt(generic_params).parse(t).unwrap();
    let (t, extend) = opt(type_extends).parse(t).unwrap();
    let (t, body) = type_body(t)?;
    let (t, semi) = opt(SEMI).parse(t).unwrap();

    Ok((
        t,
        new_node!(
            NodeKind::TypeDecl,
            [header, modifiers, kw, name, params, extend, body, semi]
        ),
    ))
}

fn type_extends(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_EXTENDS.parse(t)?;
    let (t, name) = type_ident(t)?;

    Ok((t, new_node!(NodeKind::TypeExtends, [kw, name])))
}

fn type_body(t: &[Token]) -> Res<Node> {
    many_bound(
        NodeKind::TypeBody,
        OPEN_CURLY,
        either(alt(type_attr, type_method), SEMI),
        CLOSE_CURLY,
    )
    .parse(t)
}

fn type_attr(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header_allow_semi(t)?;
    let (t, modifiers) = opt(modifiers).parse(t).unwrap();
    let (t, name) = ident_or_kw_or_strlit(t)?;
    let (t, colon) = COLON.parse(t)?;
    let (t, ty) = type_ident(t)?;
    let (t, init) = opt(initializer).parse(t).unwrap();
    let (t, semi) = expect(SEMI).parse(t).unwrap();

    Ok((
        t,
        new_node!(
            NodeKind::TypeAttr,
            [header, modifiers, name, colon, ty, init, semi]
        ),
    ))
}

fn mod_pragma(t: &[Token]) -> Res<Node> {
    let (t, doc) = opt(doc).parse(t).unwrap();
    let (t, at) = matches(TokenKind::AtSign).parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, args) = opt(call_args).parse(t).unwrap();
    let (t, semi) = SEMI.parse(t)?;

    Ok((
        t,
        new_node!(NodeKind::ModPragma, [doc, at, name, args, semi]),
    ))
}

fn ident(t: &[Token]) -> Res<Node> {
    let (t, id) = matches(TokenKind::Ident).parse(t)?;
    Ok((t, new_node!(NodeKind::Ident, [id])))
}

fn ident_or_kw(t: &[Token]) -> Res<Node> {
    let (t, id) = IDENT_OR_KW.parse(t)?;
    Ok((t, new_node!(NodeKind::Ident, [id])))
}

fn ident_or_kw_or_strlit(t: &[Token]) -> Res<Node> {
    alt(
        map(IDENT_OR_KW, |tokens| new_node!(NodeKind::Ident, [tokens])),
        map(str_expr, |n| new_node!(NodeKind::Ident, [n])),
    )
    .parse(t)
}

fn paren_expr(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_PAREN.parse(t)?;
    let (t, expr) = expect(expr).parse(t).unwrap();
    let (t, close) = CLOSE_PAREN.parse(t)?;

    Ok((t, new_node!(NodeKind::ParenExpr, [open, expr, close])))
}

fn str_expr(t: &[Token]) -> Res<Node> {
    let (t, enter_tpl) = DOUBLE_QUOTE.parse(t)?;
    let (t, opt_raw_string) = opt(RAW_STRING).parse(t).unwrap();
    let (t, exit_tpl) = DOUBLE_QUOTE.parse(t)?;

    Ok((
        t,
        new_node!(NodeKind::StringExpr, [enter_tpl, opt_raw_string, exit_tpl]),
    ))
}

fn type_method(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header_allow_semi(t).unwrap();
    let (t, modifiers) = opt(modifiers).parse(t).unwrap();
    let (t, kw) = KW_FN.parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, generics) = opt(generic_params).parse(t).unwrap();
    let (t, params) = fn_params(t)?;
    let (t, return_type) = opt(type_decorator).parse(t).unwrap();
    let (t, body) = opt(body).parse(t).unwrap();

    Ok((
        t,
        new_node!(
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

fn initializer(t: &[Token]) -> Res<Node> {
    let (t, eq) = EQ.parse(t)?;
    let (t, expr) = expr(t)?;

    Ok((t, new_node!(NodeKind::Initializer, [eq, expr])))
}

fn expr(t: &[Token]) -> Res<Node> {
    let (mut t, mut acc) = postfix_expr(t)?;

    loop {
        let Ok((next_t, op_tok)) = binary_op(t) else {
            break;
        };
        t = next_t; // advance

        let new_prec = op_tok.token.kind.precedence();

        let op = Node {
            kind: NodeKind::BinaryOperator,
            field_name: Some("op"),
            children: op_tok
                .leading
                .into_iter()
                .map(CstNode::Token)
                .chain(std::iter::once(CstNode::Token(op_tok.token)))
                .collect(),
        };

        match postfix_expr(t) {
            Ok((next_t, mut rhs)) => {
                t = next_t; // advance

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
                        let mut node = Node::with_capacity(NodeKind::BinaryExpr, 3);
                        if new_prec <= acc_op.kind.precedence() {
                            acc.field("lhs");
                            node.add(acc);
                            node.add(op);
                            rhs.field("rhs");
                            node.add(rhs);
                        } else {
                            let mut acc_nodes = acc.into_nodes().into_iter();
                            let acc_lhs = acc_nodes.next().expect("BinaryExpr always have a 'lhs'");
                            let acc_op = acc_nodes.next().expect("BinaryExpr always have a 'op'");
                            let mut acc_rhs =
                                acc_nodes.next().expect("BinaryExpr always have a 'rhs'");
                            // we create a new rhs composed of: (acc.rhs, op, rhs)
                            let mut new_rhs = Node::with_capacity(NodeKind::BinaryExpr, 3);
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
                        let mut node = Node::with_capacity(NodeKind::BinaryExpr, 3);
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
                let mut node = Node::with_capacity(NodeKind::BinaryExpr, 3);
                acc.field("lhs");
                node.add(acc);
                node.add(op);
                node.add(NodeError {
                    kind: ErrorKind::Expected {
                        expected: "an expression",
                        got: t[0].kind,
                    },
                    span: t[0].span,
                });
                acc = node;
            }
        }
    }

    Ok((t, acc))
}

// fn dump_node(node: &Node) {
//     let mut children = Vec::new();
//     for child in &node.children {
//         children.push(child.to_string());
//     }
//     println!("{:?}={children:?}", node.kind);
// }

fn postfix_expr(t: &[Token]) -> Res<Node> {
    let (t, expr) = prefix_expr(t)?;
    match postfix_op(t) {
        Ok((t, op)) => Ok((t, new_node!(NodeKind::PostfixExpr, [expr, op]))),
        Err(_) => Ok((t, expr)),
    }
}

fn postfix_op(t: &[Token]) -> Res<Tokens> {
    alt(PLUS_PLUS, MINUS_MINUS).parse(t)
}

fn prefix_expr(t: &[Token]) -> Res<Node> {
    match prefix_op(t) {
        Ok((t, op)) => {
            let (t, expr) = call_expr(t)?;
            Ok((t, new_node!(NodeKind::PrefixExpr, [op, expr])))
        }
        Err(_) => call_expr(t),
    }
}

fn prefix_op(t: &[Token]) -> Res<Tokens> {
    one_of(&[&BANG, &PLUS, &MINUS, &STAR, &PLUS_PLUS, &MINUS_MINUS]).parse(t)
}

fn call_expr(t: &[Token]) -> Res<Node> {
    let (mut t, mut recv) = primary_expr(t)?;

    loop {
        match either(&call_args, &call_expr_spec).parse(t) {
            Ok((next_t, Either::Left(args))) => {
                t = next_t; // advance
                recv = new_node!(NodeKind::CallExpr, [recv, args]);
            }
            Ok((next_t, Either::Right(spec))) => {
                t = next_t; // advance

                let mut node = Node::with_capacity(spec.kind, spec.children.len() + 1);
                node.add(recv);
                node.children.extend(spec.children);
                recv = node;
            }
            Err(_) => break,
        }
    }

    Ok((t, recv))
}

fn call_expr_spec(t: &[Token]) -> Res<Node> {
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
    .parse(t)
}

fn static_member_expr(t: &[Token]) -> Res<Node> {
    let (t, sep) = COLON_COLON.parse(t)?;
    let (t, prop) = ident_or_kw_or_strlit(t)?;

    Ok((t, new_node!(NodeKind::StaticMemberExpr, [sep, prop])))
}

fn member_expr(t: &[Token]) -> Res<Node> {
    let (t, sep) = either(DOT, ARROW).parse(t)?;
    let (t, prop) = ident_or_kw_or_strlit(t)?;

    Ok((t, new_node!(NodeKind::MemberExpr, [sep, prop])))
}

fn offset_expr(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_SQUARE.parse(t)?;
    let (t, expr) = expr(t)?;
    let (t, close) = CLOSE_SQUARE.parse(t)?;

    Ok((t, new_node!(NodeKind::OffsetExpr, [open, expr, close])))
}

fn as_spec(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_AS.parse(t)?;
    let (t, ty) = type_ident(t)?;

    Ok((t, new_node!(NodeKind::AsSpec, [kw, ty])))
}

fn is_spec(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_IS.parse(t)?;
    let (t, ty) = type_ident(t)?;

    Ok((t, new_node!(NodeKind::IsSpec, [kw, ty])))
}

fn nonnull_expr(t: &[Token]) -> Res<Node> {
    let (t, op) = BANG_BANG.parse(t)?;

    Ok((t, new_node!(NodeKind::NonNullExpr, [op])))
}

fn optional_expr(t: &[Token]) -> Res<Node> {
    let (t, op) = QMARK.parse(t)?;

    Ok((t, new_node!(NodeKind::OptionalExpr, [op])))
}

fn primary_expr(t: &[Token]) -> Res<Node> {
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
    .parse(t)
}

fn lambda_expr(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_FN.parse(t)?;
    let (t, params) = fn_params(t)?;
    let (t, body) = body(t)?;

    Ok((t, new_node!(NodeKind::LambdaExpr, [kw, params, body])))
}

fn tuple_expr(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_PAREN.parse(t)?;
    let (t, a) = expr(t)?;
    let (t, sep) = COMMA.parse(t)?;
    let (t, b) = expr(t)?;
    let (t, close) = CLOSE_PAREN.parse(t)?;

    Ok((t, new_node!(NodeKind::TupleExpr, [open, a, sep, b, close])))
}

fn object_expr(t: &[Token]) -> Res<Node> {
    let (t, ty) = expect(type_ident).parse(t).unwrap();
    let (t, fields) = object_fields(t)?;

    Ok((t, new_node!(NodeKind::ObjectExpr, [ty, fields])))
}

fn template_expr(t: &[Token]) -> Res<Node> {
    let (t, enter) = DOUBLE_QUOTE.parse(t)?;
    let (t, children) = many(either(RAW_STRING, interpolation)).parse(t).unwrap();
    let (t, exit) = DOUBLE_QUOTE.parse(t)?;

    Ok((
        t,
        new_node!(NodeKind::TemplateExpr, [enter, children, exit]),
    ))
}

fn interpolation(t: &[Token]) -> Res<Node> {
    let (t, enter) = ENTER_INTERPOLATION.parse(t)?;
    let (t, expr) = expr(t)?;
    let (t, exit) = EXIT_INTERPOLATION.parse(t)?;

    Ok((t, new_node!(NodeKind::Interpolation, [enter, expr, exit])))
}

fn array_inline_expr(t: &[Token]) -> Res<Node> {
    many_sep_bound(
        NodeKind::ArrayInlineExpr,
        OPEN_SQUARE,
        expr,
        COMMA,
        CLOSE_SQUARE,
    )
    .parse(t)
}

fn literal(t: &[Token]) -> Res<Node> {
    one_of(&[
        &num_expr, &bool_expr, &char_expr, &str_expr, &nan_expr, &inf_expr, &null_expr, &this_expr,
    ])
    .parse(t)
}

fn num_expr(t: &[Token]) -> Res<Node> {
    let (t, num) = NUMBER.parse(t)?;
    Ok((t, new_node!(NodeKind::NumExpr, [num])))
}

fn bool_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = alt(KW_TRUE, KW_FALSE).parse(t)?;
    Ok((t, new_node!(NodeKind::BoolExpr, [tokens])))
}

fn char_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = alt(CHAR_T, CHAR_U).parse(t)?;
    Ok((t, new_node!(NodeKind::CharExpr, [tokens])))
}

fn nan_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = KW_NAN.parse(t)?;
    Ok((t, new_node!(NodeKind::NaNExpr, [tokens])))
}

fn inf_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = KW_INFINITY.parse(t)?;
    Ok((t, new_node!(NodeKind::InfExpr, [tokens])))
}

fn null_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = KW_NULL.parse(t)?;
    Ok((t, new_node!(NodeKind::NullExpr, [tokens])))
}

fn this_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = KW_THIS.parse(t)?;
    Ok((t, new_node!(NodeKind::ThisExpr, [tokens])))
}

fn object_fields(t: &[Token]) -> Res<Node> {
    many_sep_bound(
        NodeKind::ObjectFields,
        OPEN_CURLY,
        alt(object_field_entry, object_field_expr),
        COMMA,
        CLOSE_CURLY,
    )
    .parse(t)
}

fn object_field_entry(t: &[Token]) -> Res<Node> {
    let (t, name) = ident_or_kw_or_strlit(t)?;
    let (t, sep) = COLON.parse(t)?;
    let (t, expr) = expr(t)?;

    Ok((t, new_node!(NodeKind::ObjectFieldEntry, [name, sep, expr])))
}

fn object_field_expr(t: &[Token]) -> Res<Node> {
    let (t, expr) = expr(t)?;

    Ok((t, new_node!(NodeKind::ObjectFieldExpr, [expr])))
}

fn generic_params(t: &[Token]) -> Res<Node> {
    many_sep_bound(NodeKind::GenericParams, LT, ident, COMMA, GT).parse(t)
}

fn type_params(t: &[Token]) -> Res<Node> {
    many_sep_bound(NodeKind::TypeParams, LT, type_ident, COMMA, GT).parse(t)
}

fn modifiers(t: &[Token]) -> Res<Node> {
    let (t, mods) = many_1(modifier).parse(t)?;
    let mut node = Node {
        kind: NodeKind::FnModifiers,
        children: Vec::with_capacity(mods.iter().map(|modifier| modifier.leading.len() + 1).sum()),
        field_name: Some("modifiers"),
    };
    for modifier in mods {
        let Tokens { leading, token } = modifier;
        node.add(leading);
        node.add(Node {
            kind: NodeKind::FnModifier,
            children: vec![token.into()],
            field_name: Some("modifier"),
        });
    }
    Ok((t, node))
}

fn modifier(t: &[Token]) -> Res<Tokens> {
    one_of(&[&KW_NATIVE, &KW_PRIVATE, &KW_STATIC, &KW_ABSTRACT]).parse(t)
}

fn body(t: &[Token]) -> Res<Node> {
    many_bound(NodeKind::Body, OPEN_CURLY, body_stmt, CLOSE_CURLY).parse(t)
}

fn body_stmt(t: &[Token]) -> Res<Either<Node, Tokens>> {
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
    .parse(t)
}

fn var_decl(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t)?;
    let (t, kw) = KW_VAR.parse(t)?;
    let (t, name) = IDENT.parse(t)?;
    let (t, ty) = opt(type_decorator).parse(t).unwrap();
    let (t, init) = opt(initializer).parse(t).unwrap();
    let (t, semi) = expect(SEMI).parse(t).unwrap();

    Ok((
        t,
        new_node!(NodeKind::VarDecl, [header, kw, name, ty, init, semi]),
    ))
}

fn try_stmt(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_TRY.parse(t)?;
    let (t, body) = body(t)?;
    let (t, catch) = opt(catch_branch).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::TryStmt, [kw, body, catch])))
}

fn catch_branch(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_CATCH.parse(t)?;
    let (t, param) = opt(catch_param).parse(t).unwrap();
    let (t, body) = body(t)?;

    Ok((t, new_node!(NodeKind::CatchBranch, [kw, param, body])))
}

fn catch_param(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_PAREN.parse(t)?;
    let (t, name) = expect(IDENT).parse(t).unwrap();
    let (t, close) = CLOSE_PAREN.parse(t)?;

    Ok((t, new_node!(NodeKind::CatchParam, [open, name, close])))
}

fn if_stmt(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_IF.parse(t)?;
    let (t, condition) = condition(t)?;
    let (t, then_branch) = body(t)?;
    let (t, else_if_branches) = many(else_if_branch).parse(t).unwrap();
    let (t, else_branch) = opt(else_branch).parse(t).unwrap();

    Ok((
        t,
        new_node!(
            NodeKind::IfStmt,
            [kw, condition, then_branch, else_if_branches, else_branch]
        ),
    ))
}

fn condition(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_PAREN.parse(t)?;
    let (t, name) = expect(expr).parse(t).unwrap();
    let (t, close) = CLOSE_PAREN.parse(t)?;

    Ok((t, new_node!(NodeKind::Condition, [open, name, close])))
}

fn else_if_branch(t: &[Token]) -> Res<Node> {
    let (t, kw_else) = KW_ELSE.parse(t)?;
    let (t, kw_if) = KW_IF.parse(t)?;
    let (t, condition) = condition(t)?;
    let (t, branch) = body(t)?;

    Ok((
        t,
        new_node!(NodeKind::ElseIfBranch, [kw_else, kw_if, condition, branch]),
    ))
}

fn else_branch(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_ELSE.parse(t)?;
    let (t, branch) = body(t)?;

    Ok((t, new_node!(NodeKind::ElseBranch, [kw, branch])))
}

fn while_stmt(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_WHILE.parse(t)?;
    let (t, condition) = expect(condition).parse(t).unwrap();
    let (t, body) = expect(body).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::WhileStmt, [kw, condition, body])))
}

fn do_while_stmt(t: &[Token]) -> Res<Node> {
    let (t, kw_do) = KW_DO.parse(t)?;
    let (t, body) = expect(body).parse(t).unwrap();
    let (t, kw_while) = expect(KW_WHILE).parse(t).unwrap();
    let (t, condition) = expect(condition).parse(t).unwrap();
    let (t, semi) = expect(SEMI).parse(t).unwrap();

    Ok((
        t,
        new_node!(
            NodeKind::DoWhileStmt,
            [kw_do, body, kw_while, condition, semi]
        ),
    ))
}

fn for_in_stmt(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_FOR.parse(t)?;
    let (t, condition) = for_in_condition(t)?;
    let (t, body) = expect(body).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::ForInStmt, [kw, condition, body])))
}

fn for_in_condition(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_PAREN.parse(t)?;
    let (t, key) = for_in_param(t)?;
    let (t, comma) = COMMA.parse(t)?;
    let (t, value) = for_in_param(t)?;
    let (t, kw_in) = expect(KW_IN).parse(t).unwrap();
    let (t, iter) = expect(expr).parse(t).unwrap();
    let (t, range) = opt(range).parse(t).unwrap();
    let (t, filters) = many(for_in_filter).parse(t).unwrap();
    let (t, close) = CLOSE_PAREN.parse(t)?;

    Ok((
        t,
        new_node!(
            NodeKind::ForInCondition,
            [open, key, comma, value, kw_in, iter, range, filters, close]
        ),
    ))
}

fn for_in_param(t: &[Token]) -> Res<Node> {
    let (t, name) = IDENT.parse(t)?;
    let (t, ty) = opt(type_decorator).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::ForInParam, [name, ty])))
}

fn range(t: &[Token]) -> Res<Node> {
    let (t, open) = RANGE_BOUND.parse(t)?;
    let (t, lower) = opt(expr).parse(t).unwrap();
    let (t, dot_dot) = DOT_DOT.parse(t)?;
    let (t, upper) = opt(expr).parse(t).unwrap();
    let (t, close) = RANGE_BOUND.parse(t)?;

    Ok((
        t,
        new_node!(NodeKind::Range, [open, lower, dot_dot, upper, close]),
    ))
}

fn for_in_filter(t: &[Token]) -> Res<Node> {
    let (t, kw) = FOR_IN_FILTER.parse(t)?;
    let (t, expr) = expect(expr).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::ForInFilter, [kw, expr])))
}

fn for_stmt(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_FOR.parse(t)?;
    let (t, condition) = for_condition(t)?;
    let (t, body) = expect(body).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::ForStmt, [kw, condition, body])))
}

fn for_condition(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_PAREN.parse(t)?;
    let (t, init) = expect(var_decl).parse(t).unwrap();
    let (t, condition) = expect(expr_stmt).parse(t).unwrap();
    let (t, update) = expect(for_expr).parse(t).unwrap();
    let (t, close) = CLOSE_PAREN.parse(t)?;

    Ok((
        t,
        new_node!(
            NodeKind::ForCondition,
            [open, init, condition, update, close]
        ),
    ))
}

fn for_expr(t: &[Token]) -> Res<Node> {
    let (t, expr) = alt(assign_expr, expr).parse(t)?;

    Ok((t, new_node!(NodeKind::ForExpr, [expr])))
}

fn at_stmt(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_AT.parse(t)?;
    let (t, expr) = paren_expr(t)?;
    let (t, body) = expect(body).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::AtStmt, [kw, expr, body])))
}

fn throw_stmt(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_THROW.parse(t)?;
    let (t, expr) = expect(expr).parse(t).unwrap();
    let (t, semi) = expect(SEMI).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::ThrowStmt, [kw, expr, semi])))
}

fn assign_stmt(t: &[Token]) -> Res<Node> {
    let (t, expr) = assign_expr(t)?;
    let (t, semi) = expect(SEMI).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::AssignStmt, [expr, semi])))
}

fn assign_expr(t: &[Token]) -> Res<Node> {
    let (t, lhs) = expr(t)?;
    let (t, op) = ASSIGN_OP.parse(t)?;
    let (t, rhs) = expr(t)?;

    Ok((t, new_node!(NodeKind::AssignExpr, [lhs, op, rhs])))
}

fn breakpoint_stmt(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_BREAKPOINT.parse(t)?;
    let (t, semi) = expect(SEMI).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::BreakpointStmt, [kw, semi])))
}

fn break_stmt(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_BREAK.parse(t)?;
    let (t, semi) = expect(SEMI).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::BreakStmt, [kw, semi])))
}

fn continue_stmt(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_CONTINUE.parse(t)?;
    let (t, semi) = expect(SEMI).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::ContinueStmt, [kw, semi])))
}

fn return_stmt(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_RETURN.parse(t)?;
    let (t, expr) = opt(expr).parse(t).unwrap();
    let (t, semi) = expect(SEMI).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::ReturnStmt, [kw, expr, semi])))
}

fn expr_stmt(t: &[Token]) -> Res<Node> {
    let (t, expr) = expr(t)?;
    let (t, semi) = expect(SEMI).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::ExprStmt, [expr, semi])))
}

fn stmt_header(t: &[Token]) -> Res<Option<Node>> {
    let (t, items) = many(doc_or_pragma).parse(t).unwrap();
    match items {
        Some(items) => {
            let node = Node {
                kind: NodeKind::StmtHeader,
                children: items.into_iter().map(CstNode::Node).collect(),
                field_name: None,
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
                field_name: None,
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

    Ok((t, new_node!(NodeKind::Doc, [items])))
}

fn pragma(t: &[Token]) -> Res<Node> {
    let (t, at) = matches(TokenKind::AtSign).parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, args) = opt(call_args).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::Pragma, [at, name, args])))
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
    let (t, name) = IDENT.parse(t)?;
    let (t, ty) = type_decorator(t)?;

    Ok((t, new_node!(NodeKind::FnParam, [name, ty])))
}

fn type_decorator(t: &[Token]) -> Res<Node> {
    let (t, c) = COLON.parse(t)?;
    let (t, ty) = expect(type_ident).parse(t).unwrap();

    Ok((t, new_node!(NodeKind::TypeDecorator, [c, ty])))
}

fn type_ident(t: &[Token]) -> Res<Node> {
    let (t, kw_typeof) = opt(KW_TYPEOF).parse(t).unwrap();
    let (t, parts) = many(seq2(IDENT_OR_KW, COLON_COLON)).parse(t).unwrap();
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, params) = opt(type_params).parse(t).unwrap();
    let (t, qmark) = opt(QMARK).parse(t).unwrap();

    Ok((
        t,
        new_node!(NodeKind::TypeIdent, [kw_typeof, parts, name, params, qmark]),
    ))
}

// fn binary_op(t: &[Token]) -> Res<Tokens> {
//     one_of(&[&OR_OP, &AND_OP, &EQ_OP, &REL_OP, &ADD_OP, &MUL_OP, &POW_OP]).parse(t)
// }

/// Try operators in precedence order (highest to lowest)
///
/// This way we fail fast on the most common cases
fn binary_op(t: &[Token]) -> Res<Tokens> {
    // Precedence 13: Exponentiation
    if let Ok(result) = POW_OP.parse(t) {
        return Ok(result);
    }

    // Precedence 12: Multiplication, Division, Modulo
    if let Ok(result) = MUL_OP.parse(t) {
        return Ok(result);
    }

    // Precedence 11: Addition, Subtraction
    if let Ok(result) = ADD_OP.parse(t) {
        return Ok(result);
    }

    // Precedence 9: Relational operators
    if let Ok(result) = REL_OP.parse(t) {
        return Ok(result);
    }

    // Precedence 8: Equality operators
    if let Ok(result) = EQ_OP.parse(t) {
        return Ok(result);
    }

    // Precedence 4: Logical AND
    if let Ok(result) = AND_OP.parse(t) {
        return Ok(result);
    }

    // Precedence 3: Logical OR
    if let Ok(result) = OR_OP.parse(t) {
        return Ok(result);
    }

    // If none match, construct a comprehensive error
    let (_, tok) = peek(t);
    Err(CustomParseError {
        text: "binary operator",
        got: tok.token,
    }
    .into())
}

pub fn expect<'t, P, T>(parser: P) -> impl Parser<'t, Either<T, NodeError>, Infallible>
where
    P: Parser<'t, T>,
{
    move |t| match parser.parse(t) {
        Ok((t, res)) => Ok((t, Either::Left(res))),
        Err(_) => Ok((
            t,
            Either::Right(NodeError {
                kind: ErrorKind::Expected {
                    expected: std::any::type_name_of_val(&parser)
                        .rsplit("::")
                        .next()
                        .unwrap(),
                    got: t[0].kind,
                },
                span: t[0].span,
            }),
        )),
    }
}

pub fn acc_trivia<'t>(acc: &mut Vec<CstNode>, t: &'t [Token]) -> &'t [Token] {
    let (_next, tok) = peek(t);
    let skip = tok.leading.len();
    acc.extend(tok.leading.into_iter().map(CstNode::Token));
    &t[skip..]
}

pub fn many_bound<'t, O, I, C, T>(
    kind: NodeKind,
    open: O,
    item: I,
    close: C,
) -> impl Parser<'t, Node>
where
    O: Parser<'t, Tokens>,
    I: Parser<'t, T>,
    C: Parser<'t, Tokens>,
    T: AddToNode,
{
    move |t| {
        let (t, o) = open.parse(t)?;
        let mut node = Node::with_capacity(kind, 4);
        node.add(o);

        let mut tokens = t;

        loop {
            // consume any trivia "in-between"
            tokens = acc_trivia(&mut node.children, tokens);
            if tokens.len() == 1 {
                // EOF reached
                let err = close.parse(tokens).err().unwrap();
                node.add(NodeError {
                    kind: ErrorKind::Expected {
                        expected: "a closing token",
                        got: tokens[0].kind,
                    },
                    span: tokens[0].span,
                });
                return Err(err);
            }
            // check for closing bound
            if let Ok((t, c)) = close.parse(tokens) {
                node.add(c);
                return Ok((t, node));
            }
            match item.parse(tokens) {
                Ok((t, i)) => {
                    node.add(i);
                    tokens = t;
                }
                Err(_) => return Ok((t, node)),
            }
        }
    }
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
    O: Parser<'t, Tokens> + Copy,
    I: Parser<'t, T> + Copy,
    S: Parser<'t, Tokens> + Copy,
    C: Parser<'t, Tokens> + Copy,
    T: AddToNode,
{
    move |t| {
        let (t, o) = open.parse(t)?;
        let mut node = Node::with_capacity(kind, 4);
        node.add(o);

        let mut state = ManySepBoundState::ExpectItem;
        let mut tokens = t;

        loop {
            // consume any trivia "in-between"
            tokens = acc_trivia(&mut node.children, tokens);
            if tokens.len() == 1 {
                // EOF reached
                let err = close.parse(tokens).err().unwrap();
                node.add(NodeError {
                    kind: ErrorKind::Expected {
                        expected: "a closing token",
                        got: tokens[0].kind,
                    },
                    span: tokens[0].span,
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
                        if let Some(last) = node.last_token_mut() {
                            if let CstNode::Token(tok) = last {
                                *last = CstNode::Error(NodeError {
                                    kind: ErrorKind::Expected {
                                        expected: "a separator",
                                        got: tok.kind,
                                    },
                                    span: tok.span,
                                });
                            }
                        }
                        let (t, i) = item.parse(tokens)?;
                        node.add(i);
                        tokens = t;
                        state = ManySepBoundState::ExpectSep;
                    }
                },
                ManySepBoundState::ExpectItem => match item.parse(tokens) {
                    Ok((t, i)) => {
                        node.add(i);
                        tokens = t;
                        state = ManySepBoundState::ExpectSep;
                    }
                    Err(_) => match either(sep, close).parse(tokens) {
                        Ok((t, Either::Left(s))) => {
                            node.add(s.leading);
                            node.add(NodeError {
                                kind: ErrorKind::Expected {
                                    expected: "a separator",
                                    got: s.token.kind,
                                },
                                span: s.token.span,
                            });
                            tokens = t;
                            state = ManySepBoundState::ExpectItem;
                        }
                        Ok((t, Either::Right(c))) => {
                            node.add(c.leading);
                            node.add(NodeError {
                                kind: ErrorKind::Expected {
                                    expected: "a separator",
                                    got: c.token.kind,
                                },
                                span: c.token.span,
                            });
                            return Ok((t, node));
                        }
                        Err(err) => return Err(err),
                    },
                },
            }
        }
    }
}

#[allow(unused)]
fn field<'t, P, E>(name: &'static str, parser: P) -> impl Parser<'t, Node, E>
where
    P: Parser<'t, Node, E>,
{
    move |t| {
        let (t, mut node) = parser.parse(t)?;
        node.field(name);
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
            ErrorKind::Expected {
                expected: "a separator",
                got: TokenKind::Space(1),
            },
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

    #[test]
    fn empty_fn_decl() {
        let source = "fn main() {}";
        let tokens = tokenize(source);
        let res = fn_decl(&tokens);
        assert!(res.is_ok());
    }

    #[test]
    fn fn_body_expr_stmt() {
        let source = "{ a; }";
        let tokens = tokenize(source);
        let res = body(&tokens);
        assert!(res.is_ok());
    }

    #[test]
    fn expected_expr_in_paren() {
        let source = r#"fn main() {
    var x = ();
}"#;
        let tokens = tokenize(source);
        let res = fn_decl(&tokens);
        assert!(res.is_ok());
    }

    #[test]
    fn bin_expr_precedence() {
        let source = "3 + 4 * 2 / (1 - 5) ^ 2 ^ 3";
        let tokens = tokenize(source);
        let (_, node) = expr(&tokens).unwrap();
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
