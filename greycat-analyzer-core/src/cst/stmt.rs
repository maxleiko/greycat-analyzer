use crate::{
    Token,
    cst::{Node, Rule},
    lexer::TokenKind,
};

use super::{
    NodeRule,
    combi::span_from_nodes,
    cst_parser::{CstParser, ParserResult},
    error::ParseError,
    token_ext::TokenExt,
};

impl<'src> CstParser<'src> {
    pub fn parse_module(&mut self, source: &'src str) -> ParserResult<NodeRule> {
        let mut children = Vec::new();

        let mut bkp;
        while self.has_token() {
            bkp = self.clone();
            match self.peek() {
                Some(peek) if peek.token.kind == TokenKind::Semi => {
                    let semi = self.next().unwrap();
                    semi.merge_into(&mut children);
                }
                _ => {
                    match self.parse_function(source) {
                        Ok(function) => {
                            children.push(function);
                            continue;
                        }
                        Err(ParseError::UnexpectedEof) => return Err(ParseError::UnexpectedEof),
                        Err(_err) => {
                            // backtrack lexer
                            self.restore(&bkp);
                        }
                    }
                    match self.parse_pragma_stmt(source) {
                        Ok(function) => {
                            children.push(function);
                            continue;
                        }
                        Err(ParseError::UnexpectedEof) => return Err(ParseError::UnexpectedEof),
                        Err(err) => {
                            eprintln!("{}", err.as_source_error(source));
                            // backtrack lexer
                            self.restore(&bkp);
                        }
                    }
                }
            }
        }

        Ok(NodeRule::new(Rule::Module, children))
    }

    fn parse_pragma_stmt(&mut self, source: &'src str) -> ParserResult<Node> {
        let at = self.expect(TokenKind::At)?;
        let name = self.expect(TokenKind::Ident)?;
        let args = match self.peek() {
            Some(tok) if tok.token.kind == TokenKind::OpenParen => Some(self.many_sep(
                source,
                TokenKind::OpenParen,
                TokenKind::Comma,
                TokenKind::CloseParen,
                CstParser::parse_expr,
                Rule::PragmaArgs,
            )?),
            Some(_) => None,
            None => return Err(ParseError::UnexpectedEof),
        };
        let semi = self.expect_opt(TokenKind::Semi)?;

        let mut children = Vec::new();
        at.merge_into(&mut children);
        name.merge_into_as(&mut children, as_name);
        if let Some(args) = args {
            children.push(args);
        }
        if let Some(semi) = semi {
            semi.merge_into(&mut children);
        }
        Ok(Node::rule(Rule::PragmaStmt, children))
    }

    fn parse_function(&mut self, source: &'src str) -> ParserResult<Node> {
        let modifiers = self.parse_fn_modifiers(source)?;
        let kw = self.expect_ident(source, "fn")?;
        let name = self.expect(TokenKind::Ident)?;
        let generic_params = self.parse_fn_generic_params(source)?;
        let params = self.parse_fn_params(source)?;
        let return_type = self.parse_fn_return_type(source)?;
        let body = self.parse_fn_body(source)?;

        let mut children = Vec::new();
        if let Some(modifiers) = modifiers {
            children.push(modifiers);
        }
        kw.merge_into(&mut children);
        name.merge_into_as(&mut children, as_name);
        if let Some(generic_params) = generic_params {
            children.push(generic_params);
        }
        children.push(params);
        if let Some(return_type) = return_type {
            children.push(return_type);
        }
        if let Some(body) = body {
            children.push(body);
        }
        Ok(Node::rule(Rule::Function, children))
    }

    fn parse_fn_modifiers(&mut self, source: &'src str) -> ParserResult<Option<Node>> {
        let mut children = Vec::new();
        while let Some(modifier) = self.parse_fn_modifier(source)? {
            modifier.merge_into(&mut children);
        }
        if children.is_empty() {
            return Ok(None);
        }
        Ok(Some(Node::rule(Rule::FnModifiers, children)))
    }

    fn parse_fn_modifier(&mut self, source: &'src str) -> ParserResult<Option<TokenExt>> {
        match self.expect_ident_n(source, &["native"]) {
            Ok(tok) => Ok(Some(tok)),
            Err(ParseError::ExpectedIdents(_, _)) => Ok(None),
            Err(ParseError::UnexpectedEof) => Err(ParseError::UnexpectedEof),
            Err(_) => unreachable!(),
        }
    }

    fn parse_fn_generic_params(&mut self, source: &'src str) -> ParserResult<Option<Node>> {
        if let Some(tok) = self.peek() {
            if tok.kind() != TokenKind::Lt {
                return Ok(None);
            }
        }
        match self.many_sep(
            source,
            TokenKind::Lt,
            TokenKind::Comma,
            TokenKind::Gt,
            CstParser::parse_generic_param,
            Rule::GenericParams,
        ) {
            Ok(node) => Ok(Some(node)),
            Err(ParseError::NoMatch) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn parse_generic_param(&mut self, _source: &'src str) -> ParserResult<Node> {
        let ident = self.expect(TokenKind::Ident)?;
        let mut children = Vec::new();
        ident.merge_into(&mut children);
        Ok(Node::rule(Rule::GenericParam, children))
    }

    fn parse_fn_params(&mut self, source: &'src str) -> ParserResult<Node> {
        self.many_sep(
            source,
            TokenKind::OpenParen,
            TokenKind::Comma,
            TokenKind::CloseParen,
            CstParser::parse_fn_param,
            Rule::FnParams,
        )
    }

    fn parse_fn_param(&mut self, source: &'src str) -> ParserResult<Node> {
        let name = self.expect(TokenKind::Ident)?;
        let colon = self.expect(TokenKind::Colon)?;
        let type_ident = self.parse_type_ident(source)?;
        let mut children = Vec::new();
        name.merge_into_as(&mut children, as_name);
        colon.merge_into(&mut children);
        children.push(type_ident);
        Ok(Node::rule(Rule::FnParam, children))
    }

    fn parse_type_ident(&mut self, _source: &'src str) -> ParserResult<Node> {
        // TODO complete type ident grammar
        let name = self.expect(TokenKind::Ident)?;
        let mut children = Vec::new();
        name.merge_into_as(&mut children, as_name);
        Ok(Node::rule(Rule::TypeIdent, children))
    }

    fn parse_fn_return_type(&mut self, source: &'src str) -> ParserResult<Option<Node>> {
        match self.expect_opt(TokenKind::Colon)? {
            Some(colon) => {
                let type_ident = self.parse_type_ident(source)?;
                let mut children = Vec::new();
                colon.merge_into(&mut children);
                children.push(type_ident);
                Ok(Some(Node::rule(Rule::ReturnType, children)))
            }
            None => Ok(None),
        }
    }

    fn parse_fn_body(&mut self, source: &'src str) -> ParserResult<Option<Node>> {
        match self.expect_opt(TokenKind::OpenCurly)? {
            Some(ocurly) => {
                let mut stmts = Vec::new();
                while let Some(tok) = self.peek() {
                    match tok.kind() {
                        TokenKind::CloseCurly => {
                            let ccurly = self.next().unwrap();
                            let mut children = Vec::new();
                            ocurly.merge_into(&mut children);
                            children.extend(stmts);
                            ccurly.merge_into(&mut children);
                            return Ok(Some(Node::rule(Rule::Body, children)));
                        }
                        _ => {
                            let stmt = self.parse_body_stmt(source)?;
                            stmts.push(stmt);
                        }
                    }
                }
                Err(ParseError::UnexpectedEof)
            }
            None => Ok(None),
        }
    }

    fn parse_body_stmt(&mut self, _source: &'src str) -> ParserResult<Node> {
        // TODO actual body stmt parsing, right now we just eat everything until ';'
        let mut children = Vec::new();
        while let Some(tok) = self.peek() {
            match tok.kind() {
                TokenKind::Semi => {
                    let semi = self.next().unwrap();
                    semi.merge_into(&mut children);
                    return Ok(Node::rule(Rule::BodyStmt, children));
                }
                _ => {
                    let tok = self.next().unwrap();
                    tok.merge_into(&mut children);
                }
            }
        }
        Err(ParseError::UnexpectedEof)
    }
}

fn as_name(token: Token) -> Node {
    Node::rule(Rule::Name, vec![Node::Token(token)])
}
