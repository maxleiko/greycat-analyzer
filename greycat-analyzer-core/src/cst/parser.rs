use std::{cell::RefCell, collections::VecDeque, convert::Infallible};

use crate::{
    Token, TokenKind,
    cst::{AddToNode, CstNode, ErrorKind, Node, NodeError, NodeKind, Tokens, combi::*},
    tokenize,
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
    one_of(&[&fn_decl, &type_decl, &enum_decl, &mod_var_decl, &mod_pragma]).parse(t)
}

fn fn_decl(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t).unwrap();
    let (t, modifiers) = modifiers(t).unwrap();
    let (t, kw) = KW_FN.parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, generics) = opt(generic_params).parse(t).unwrap();
    let (t, params) = fn_params(t)?;
    let (t, return_type) = opt(type_decorator).parse(t).unwrap();
    let (t, body_or_semi) = either(&fn_body, &SEMI).parse(t)?;

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

fn enum_decl(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t).unwrap();
    let (t, modifiers) = modifiers(t).unwrap();
    let (t, kw) = KW_ENUM.parse(t)?;
    let (t, name) = IDENT_OR_KW.parse(t)?;
    let (t, body) = enum_body(t)?;
    let (t, semi) = opt(SEMI).parse(t).unwrap();

    let mut node = Node::new(NodeKind::EnumDecl);
    node.add(header);
    node.add(modifiers);
    node.add(kw);
    node.add(name);
    node.add(body);
    node.add(semi);
    Ok((t, node))
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

    let mut node = Node::new(NodeKind::EnumField);
    node.add(header);
    node.add(name);
    node.add(args);
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
    let (t, name) = type_ident(t)?;

    let mut node = Node::new(NodeKind::TypeExtends);
    node.add(kw);
    node.add(name);
    Ok((t, node))
}

fn type_body(t: &[Token]) -> Res<Node> {
    many_bound(
        NodeKind::TypeBody,
        OPEN_CURLY,
        either(&alt(type_attr, type_method), &SEMI),
        CLOSE_CURLY,
    )
    .parse(t)
}

fn type_attr(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header_allow_semi(t)?;
    let (t, modifiers) = modifiers(t)?;
    let (t, name) = ident_or_kw_or_strlit(t)?;
    let (t, colon) = COLON.parse(t)?;
    let (t, ty) = type_ident(t)?;
    let (t, init) = opt(initializer).parse(t).unwrap();
    let (t, semi) = expect(SEMI).parse(t).unwrap();

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

fn ident_or_kw_or_strlit(t: &[Token]) -> Res<Node> {
    alt(
        map(IDENT_OR_KW, |tokens| {
            let mut node = Node::new(NodeKind::Ident);
            node.add(tokens);
            node
        }),
        map(str_expr, |n| {
            let mut node = Node::new(NodeKind::Ident);
            node.add(n);
            node
        }),
    )
    .parse(t)
}

fn paren_expr(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_PAREN.parse(t)?;
    let (t, expr) = expr(t)?;
    let (t, close) = CLOSE_PAREN.parse(t)?;

    let mut node = Node::new(NodeKind::ParenExpr);
    node.add(open);
    node.add(expr);
    node.add(close);
    Ok((t, node))
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
    let (t, body) = opt(fn_body).parse(t).unwrap();

    let mut node = Node::new(NodeKind::TypeMethod);
    node.add(header);
    node.add(modifiers);
    node.add(kw);
    node.add(name);
    node.add(generics);
    node.add(params);
    node.add(return_type);
    node.add(body);
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
    // parse initial operand
    let (mut t, mut lhs) = postfix_expr(t)?;
    // TODO:
    // loop {
    //     // try parsing a binary operator
    //     let Ok((next, op_tok)) = binary_op(t) else {
    //         break;
    //     };

    //     // determine operator precedence
    //     let new_prec = precedence(&op_tok.token);

    //     // advance past operator
    //     t = next;

    //     // try parsing right-hand side operand
    //     let rhs_res = postfix_expr(t);

    //     match rhs_res {
    //         Ok((next_t, mut rhs)) => {
    //             t = next_t;

    //             if lhs.kind == NodeKind::BinExpr {
    //                 // re-associate if current binary expr has higher precedence
    //                 let curr_op = lhs
    //                     .get_field("op")
    //                     .expect("BinaryExpr must have an operator");
    //                 let curr_prec = precedence(curr_op.token().unwrap());

    //                 if new_prec <= curr_prec {
    //                     let mut node = Node::new(NodeKind::BinExpr);
    //                     lhs.set_field("lhs");
    //                     node.add(lhs);
    //                     node.add(op_tok.to_node(NodeKind::BinOp, "op"));
    //                     rhs.set_field("rhs");
    //                     node.add(rhs);
    //                     lhs = node;
    //                     continue;
    //                 }

    //                 // restructure to insert inside rhs
    //                 let curr_lhs = lhs.get_field("lhs").unwrap();
    //                 let curr_op = lhs.get_field("op").unwrap();
    //                 let curr_rhs = lhs.get_field("rhs").unwrap();

    //                 let mut rhs_expr = Node::new(NodeKind::BinExpr);
    //                 curr_rhs.clone().set_field("lhs");
    //                 rhs_expr.add(curr_rhs);
    //                 rhs_expr.add(op_tok.to_node(NodeKind::BinOp, "op"));
    //                 rhs.set_field("rhs");
    //                 rhs_expr.add(rhs);

    //                 let mut node = Node::new(NodeKind::BinExpr);
    //                 curr_lhs.clone().set_field("lhs");
    //                 node.add(curr_lhs);
    //                 node.add(curr_op);
    //                 rhs_expr.set_field("rhs");
    //                 node.add(rhs_expr);

    //                 lhs = node;
    //             } else {
    //                 // normal case
    //                 let mut node = Node::new(NodeKind::BinExpr);
    //                 lhs.set_field("lhs");
    //                 node.add(lhs);
    //                 node.add(op_tok.to_node(NodeKind::BinOp, "op"));
    //                 rhs.set_field("rhs");
    //                 node.add(rhs);
    //                 lhs = node;
    //             }
    //         }
    //         Err(_) => {
    //             // rhs failed to parse, still construct a binary expr node with an error
    //             let mut node = Node::new(NodeKind::BinExpr);
    //             lhs.set_field("lhs");
    //             node.add(lhs);
    //             node.add(op_tok.to_node(NodeKind::BinOp, "op"));
    //             node.add(NodeError {
    //                 kind: ErrorKind::Expected,
    //                 token: t[0],
    //             });
    //             lhs = node;
    //             break;
    //         }
    //     }
    // }

    Ok((t, lhs))
}

fn postfix_expr(t: &[Token]) -> Res<Node> {
    let (t, expr) = prefix_expr(t)?;
    match postfix_op(t) {
        Ok((t, op)) => {
            let mut node = Node::new(NodeKind::PostfixExpr);
            node.add(expr);
            node.add(op);
            Ok((t, node))
        }
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
            let mut node = Node::new(NodeKind::PrefixExpr);
            node.add(op);
            node.add(expr);
            Ok((t, node))
        }
        Err(_) => call_expr(t),
    }
}

fn prefix_op(t: &[Token]) -> Res<Tokens> {
    one_of(&[&BANG, &PLUS, &MINUS, &STAR, &PLUS_PLUS, &MINUS_MINUS]).parse(t)
}

fn call_expr(t: &[Token]) -> Res<Node> {
    let (mut t, mut recv) = primary_expr(t)?;

    // loop {
    //     let Ok((next_t, mut spec)) = call_expr_spec(t) else {
    //         // no follow-up spec, stop parsing
    //         break;
    //     };

    //     t = next_t;

    //     if spec.kind == NodeKind::CallArgs {
    //         // construct CallExpr node
    //         let mut expr = Node::new(NodeKind::CallExpr);
    //         // TODO: field_name?
    //         // recv.set_field("recv");
    //         expr.add(recv);

    //         let mut args = spec;
    //         // TODO: field_name?
    //         // args.set_field("args");
    //         expr.add(args);

    //         recv = expr;
    //     } else {
    //         spec.prepend(recv);
    //         recv = spec;
    //     }
    // }

    Ok((t, recv))
}

enum CallExprSpec {
    Wrap(Node),
    Append(Node),
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

    let mut node = Node::new(NodeKind::StaticMemberExpr);
    node.add(sep);
    node.add(prop);
    Ok((t, node))
}

fn member_expr(t: &[Token]) -> Res<Node> {
    let (t, sep) = either(&DOT, &ARROW).parse(t)?;
    let (t, prop) = ident_or_kw_or_strlit(t)?;

    let mut node = Node::new(NodeKind::MemberExpr);
    node.add(sep);
    node.add(prop);
    Ok((t, node))
}

fn offset_expr(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_SQUARE.parse(t)?;
    let (t, expr) = expr(t)?;
    let (t, close) = CLOSE_SQUARE.parse(t)?;

    let mut node = Node::new(NodeKind::OffsetExpr);
    node.add(open);
    node.add(expr);
    node.add(close);
    Ok((t, node))
}

fn as_spec(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_AS.parse(t)?;
    let (t, ty) = type_ident(t)?;

    let mut node = Node::new(NodeKind::AsSpec);
    node.add(kw);
    node.add(ty);
    Ok((t, node))
}

fn is_spec(t: &[Token]) -> Res<Node> {
    let (t, kw) = KW_IS.parse(t)?;
    let (t, ty) = type_ident(t)?;

    let mut node = Node::new(NodeKind::IsSpec);
    node.add(kw);
    node.add(ty);
    Ok((t, node))
}

fn nonnull_expr(t: &[Token]) -> Res<Node> {
    let (t, op) = BANG_BANG.parse(t)?;

    let mut node = Node::new(NodeKind::NonNullExpr);
    node.add(op);
    Ok((t, node))
}

fn optional_expr(t: &[Token]) -> Res<Node> {
    let (t, op) = QMARK.parse(t)?;

    let mut node = Node::new(NodeKind::OptionalExpr);
    node.add(op);
    Ok((t, node))
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
    let (t, body) = fn_body(t)?;

    let mut node = Node::new(NodeKind::LambdaExpr);
    node.add(kw);
    node.add(params);
    node.add(body);
    Ok((t, node))
}

fn tuple_expr(t: &[Token]) -> Res<Node> {
    let (t, open) = OPEN_PAREN.parse(t)?;
    let (t, a) = expr(t)?;
    let (t, sep) = COMMA.parse(t)?;
    let (t, b) = expr(t)?;
    let (t, close) = CLOSE_PAREN.parse(t)?;

    let mut node = Node::new(NodeKind::LambdaExpr);
    node.add(open);
    node.add(a);
    node.add(sep);
    node.add(b);
    node.add(close);
    Ok((t, node))
}

fn object_expr(t: &[Token]) -> Res<Node> {
    let (t, ty) = expect(type_ident).parse(t).unwrap();
    let (t, fields) = object_fields(t)?;

    let mut node = Node::new(NodeKind::ObjectExpr);
    node.add(ty);
    node.add(fields);
    Ok((t, node))
}

fn template_expr(t: &[Token]) -> Res<Node> {
    todo!()
}

fn array_inline_expr(t: &[Token]) -> Res<Node> {
    todo!()
}

fn literal(t: &[Token]) -> Res<Node> {
    one_of(&[
        &num_expr, &bool_expr, &char_expr, &str_expr, &nan_expr, &inf_expr, &null_expr, &this_expr,
    ])
    .parse(t)
}

fn num_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = one_of(&[&INT, &FLOAT_T, &FLOAT_U]).parse(t)?;
    let mut node = Node::new(NodeKind::NumExpr);
    node.add(tokens);
    Ok((t, node))
}

fn bool_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = alt(KW_TRUE, KW_FALSE).parse(t)?;
    let mut node = Node::new(NodeKind::BoolExpr);
    node.add(tokens);
    Ok((t, node))
}

fn char_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = alt(CHAR_T, CHAR_U).parse(t)?;
    let mut node = Node::new(NodeKind::CharExpr);
    node.add(tokens);
    Ok((t, node))
}

fn nan_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = KW_NAN.parse(t)?;
    let mut node = Node::new(NodeKind::NaNExpr);
    node.add(tokens);
    Ok((t, node))
}

fn inf_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = KW_INFINITY.parse(t)?;
    let mut node = Node::new(NodeKind::InfExpr);
    node.add(tokens);
    Ok((t, node))
}

fn null_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = KW_NULL.parse(t)?;
    let mut node = Node::new(NodeKind::NullExpr);
    node.add(tokens);
    Ok((t, node))
}

fn this_expr(t: &[Token]) -> Res<Node> {
    let (t, tokens) = KW_THIS.parse(t)?;
    let mut node = Node::new(NodeKind::ThisExpr);
    node.add(tokens);
    Ok((t, node))
}

fn object_fields(t: &[Token]) -> Res<Node> {
    todo!()
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
    many_sep_bound(NodeKind::TypeParams, LT, type_ident, COMMA, GT).parse(t)
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

fn fn_body(t: &[Token]) -> Res<Node> {
    many_bound(NodeKind::FnBody, OPEN_CURLY, body_stmt, CLOSE_CURLY).parse(t)
}

fn body_stmt(t: &[Token]) -> Res<Either<Node, Tokens>> {
    either(&one_of(&[&var_decl, &expr_stmt]), &SEMI).parse(t)
}

fn var_decl(t: &[Token]) -> Res<Node> {
    let (t, header) = stmt_header(t)?;
    let (t, kw) = KW_VAR.parse(t)?;
    let (t, name) = IDENT.parse(t)?;
    let (t, ty) = opt(type_decorator).parse(t).unwrap();
    let (t, init) = opt(initializer).parse(t).unwrap();
    let (t, semi) = expect(SEMI).parse(t).unwrap();

    let mut node = Node::new(NodeKind::VarDecl);
    node.add(header);
    node.add(kw);
    node.add(name);
    node.add(ty);
    node.add(init);
    node.add(semi);
    Ok((t, node))
}

fn expr_stmt(t: &[Token]) -> Res<Node> {
    let (t, expr) = expr(t)?;
    let (t, semi) = expect(SEMI).parse(t).unwrap();

    let mut node = Node::new(NodeKind::ExprStmt);
    node.add(expr);
    node.add(semi);
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
    let (t, name) = IDENT.parse(t)?;
    let (t, ty) = type_decorator(t)?;

    let mut node = Node::new(NodeKind::FnParam);
    node.add(name);
    node.add(ty);
    Ok((t, node))
}

fn type_decorator(t: &[Token]) -> Res<Node> {
    let (t, c) = COLON.parse(t)?;
    let (t, ty) = expect(type_ident).parse(t).unwrap();

    let mut node = Node::new(NodeKind::TypeDecorator);
    node.add(c);
    node.add(ty);
    Ok((t, node))
}

fn type_ident(t: &[Token]) -> Res<Node> {
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

pub fn expect<'t, P, T>(parser: P) -> impl Parser<'t, Either<T, NodeError>, Infallible>
where
    P: Parser<'t, T>,
{
    move |t| match parser.parse(t) {
        Ok((t, res)) => Ok((t, Either::Left(res))),
        Err(_) => Ok((
            t,
            Either::Right(NodeError {
                kind: ErrorKind::MissingToken,
                token: t[0],
            }),
        )),
    }
}

pub fn acc_trivia<'t>(acc: &mut Vec<CstNode>, t: &'t [Token]) -> &'t [Token] {
    let (next, tok) = peek(t);
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
        let mut node = Node::new(kind);
        node.add(o);

        let mut tokens = t;

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
                ManySepBoundState::ExpectItem => match item.parse(tokens) {
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
                },
            }
        }
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
static ARROW: Matches = matches(TokenKind::Arrow);

static BANG_BANG: Matches = matches(TokenKind::BangBang);
static BANG: Matches = matches(TokenKind::Bang);
static PLUS_PLUS: Matches = matches(TokenKind::PlusPlus);
static MINUS_MINUS: Matches = matches(TokenKind::MinusMinus);
static OR_OR: Matches = matches(TokenKind::OrOr);
static QMARK_QMARK: Matches = matches(TokenKind::QuestionQuestion);
static AND_AND: Matches = matches(TokenKind::AndAnd);
static EQ_EQ: Matches = matches(TokenKind::EqEq);
static BANG_EQ: Matches = matches(TokenKind::BangEq);
static LT_EQ: Matches = matches(TokenKind::LtEq);
static GT_EQ: Matches = matches(TokenKind::GtEq);
static PLUS: Matches = matches(TokenKind::Plus);
static MINUS: Matches = matches(TokenKind::Minus);
static STAR: Matches = matches(TokenKind::Star);
static SLASH: Matches = matches(TokenKind::Slash);
static PERCENT: Matches = matches(TokenKind::Percent);
static CARET: Matches = matches(TokenKind::Caret);

static INT: Matches = matches(TokenKind::Int);
static FLOAT_T: Matches = matches(TokenKind::Float { terminated: true });
static FLOAT_U: Matches = matches(TokenKind::Float { terminated: false });
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

#[cfg(test)]
mod test {
    use crate::{
        span::{Pos, Span},
        tokenize,
    };

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
        let res = fn_body(&tokens);
        assert!(res.is_ok());
    }
}
