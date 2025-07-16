use crate::{
    Token,
    cst::{CstNode, NodeKind},
    lexer::TokenKind,
};

use super::{
    Node,
    combi::span_from_nodes,
    error::ParseError,
    parser::{CstParser, ParserResult},
    token_ext::TokenExt,
};

impl<'src> CstParser<'src> {
    pub fn parse_module(&mut self, source: &'src str) -> ParserResult<Node> {
        let mut node = Node::new(NodeKind::Module);

        let mut bkp;
        while self.has_token() {
            bkp = self.clone();
            match self.peek() {
                Some(peek) if peek.token.kind == TokenKind::Semi => {
                    node.add_token_ext(self.next().unwrap());
                }
                _ => {
                    match self.parse_function(source) {
                        Ok(n) => {
                            node.add_node(n);
                            continue;
                        }
                        Err(ParseError::UnexpectedEof) => return Err(ParseError::UnexpectedEof),
                        Err(_err) => {
                            // backtrack lexer
                            self.restore(&bkp);
                        }
                    }
                    match self.parse_pragma_stmt(source) {
                        Ok(n) => {
                            node.add_node(n);
                            continue;
                        }
                        Err(ParseError::UnexpectedEof) => return Err(ParseError::UnexpectedEof),
                        Err(err) => {
                            eprintln!("{}", err.as_source_error(source));
                        }
                    }
                }
            }
        }

        Ok(node)
    }

    fn parse_pragma_stmt(&mut self, source: &'src str) -> ParserResult<CstNode> {
        let mut node = Node::new(NodeKind::PragmaStmt);
        let at = self.expect(TokenKind::At)?;
        node.add_token_ext(at);
        let name = self.expect(TokenKind::Ident)?;
        node.add_token_ext(name);
        let args = match self.peek() {
            Some(tok) if tok.token.kind == TokenKind::OpenParen => Some(self.many_sep(
                source,
                TokenKind::OpenParen,
                TokenKind::Comma,
                TokenKind::CloseParen,
                CstParser::parse_expr,
                NodeKind::PragmaArgs,
            )?),
            Some(_) => None,
            None => return Err(ParseError::UnexpectedEof),
        };
        if let Some(args) = args {
            node.add_node(args);
        }
        let semi = self.expect_opt(TokenKind::Semi)?;
        if let Some(semi) = semi {
            node.add_token_ext(semi);
        }
        Ok(CstNode::Node(node))
    }

    fn parse_function(&mut self, source: &'src str) -> ParserResult<CstNode> {
        let mut node = Node::new(NodeKind::Function);
        if let Some(modifiers) = self.parse_fn_modifiers(source)? {
            node.add_node(CstNode::Node(modifiers));
        }
        let kw = self.expect_ident(source, "fn")?;
        node.add_token_ext(kw);
        let name = self.expect(TokenKind::Ident)?;
        node.add_token_ext(name);
        if let Some(generic_params) = self.parse_fn_generic_params(source)? {
            node.add_node(generic_params);
        }
        let params = self.parse_fn_params(source)?;
        node.add_node(params);
        if let Some(return_type) = self.parse_fn_return_type(source)? {
            node.add_node(return_type);
        }
        if let Some(body) = self.parse_fn_body(source)? {
            node.add_node(body);
        }
        Ok(CstNode::Node(node))
    }

    fn parse_fn_modifiers(&mut self, source: &'src str) -> ParserResult<Option<Node>> {
        let mut node = Node::new(NodeKind::FnModifiers);
        while let Some(modifier) = self.parse_fn_modifier(source)? {
            node.add_token_ext(modifier);
        }
        if node.is_empty() {
            return Ok(None);
        }
        Ok(Some(node))
    }

    fn parse_fn_modifier(&mut self, source: &'src str) -> ParserResult<Option<TokenExt>> {
        match self.expect_ident_n(source, &["native"]) {
            Ok(tok) => Ok(Some(tok)),
            Err(ParseError::ExpectedIdents(_, _)) => Ok(None),
            Err(ParseError::UnexpectedEof) => Err(ParseError::UnexpectedEof),
            Err(_) => unreachable!(),
        }
    }

    fn parse_fn_generic_params(&mut self, source: &'src str) -> ParserResult<Option<CstNode>> {
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
            NodeKind::GenericParams,
        ) {
            Ok(node) => Ok(Some(node)),
            Err(ParseError::NoMatch) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn parse_generic_param(&mut self, _source: &'src str) -> ParserResult<CstNode> {
        let mut node = Node::new(NodeKind::GenericParam);
        let ident = self.expect(TokenKind::Ident)?;
        node.add_token_ext(ident);
        Ok(CstNode::Node(node))
    }

    fn parse_fn_params(&mut self, source: &'src str) -> ParserResult<CstNode> {
        self.many_sep(
            source,
            TokenKind::OpenParen,
            TokenKind::Comma,
            TokenKind::CloseParen,
            CstParser::parse_fn_param,
            NodeKind::FnParams,
        )
    }

    fn parse_fn_param(&mut self, source: &'src str) -> ParserResult<CstNode> {
        let mut node = Node::new(NodeKind::FnParam);
        let name = self.expect(TokenKind::Ident)?;
        node.add_token_ext(name);
        let colon = self.expect(TokenKind::Colon)?;
        node.add_token_ext(colon);
        let type_ident = self.parse_type_ident(source)?;
        node.add_node(type_ident);
        Ok(CstNode::Node(node))
    }

    fn parse_type_ident(&mut self, _source: &'src str) -> ParserResult<CstNode> {
        // TODO complete type ident grammar
        let mut node = Node::new(NodeKind::TypeIdent);
        let name = self.expect(TokenKind::Ident)?;
        Ok(CstNode::Node(node))
    }

    fn parse_fn_return_type(&mut self, source: &'src str) -> ParserResult<Option<CstNode>> {
        match self.expect_opt(TokenKind::Colon)? {
            Some(colon) => {
                let mut node = Node::new(NodeKind::ReturnType);
                let type_ident = self.parse_type_ident(source)?;
                node.add_token_ext(colon);
                node.add_node(type_ident);
                Ok(Some(CstNode::Node(node)))
            }
            None => Ok(None),
        }
    }

    fn parse_fn_body(&mut self, source: &'src str) -> ParserResult<Option<CstNode>> {
        match self.expect_opt(TokenKind::OpenCurly)? {
            Some(ocurly) => {
                let mut node = Node::new(NodeKind::Body);
                node.add_token_ext(ocurly);
                while let Some(tok) = self.peek() {
                    match tok.kind() {
                        TokenKind::CloseCurly => {
                            let ccurly = self.next().unwrap();
                            node.add_token_ext(ccurly);
                            return Ok(Some(CstNode::Node(node)));
                        }
                        _ => {
                            let stmt = self.parse_body_stmt(source)?;
                            node.add_node(stmt);
                        }
                    }
                }
                Err(ParseError::UnexpectedEof)
            }
            None => Ok(None),
        }
    }

    fn parse_body_stmt(&mut self, _source: &'src str) -> ParserResult<CstNode> {
        // TODO actual body stmt parsing, right now we just eat everything until ';'
        let mut node = Node::new(NodeKind::BodyStmt);
        while let Some(tok) = self.peek() {
            match tok.kind() {
                TokenKind::Semi => {
                    let semi = self.next().unwrap();
                    node.add_token_ext(semi);
                    return Ok(CstNode::Node(node));
                }
                _ => {
                    let tok = self.next().unwrap();
                    node.add_token_ext(tok);
                }
            }
        }
        Err(ParseError::UnexpectedEof)
    }
}
