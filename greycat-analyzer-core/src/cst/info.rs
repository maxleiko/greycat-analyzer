use crate::{
    TokenKind,
    cst::{Node, NodeKind, SourceModule},
    span::SpanStr,
};

#[derive(Debug)]
pub struct Library<'src> {
    pub name: SpanStr<'src>,
    pub version: Option<SpanStr<'src>>,
}

pub type Libraries<'src> = Vec<Library<'src>>;
pub type Includes<'src> = Vec<SpanStr<'src>>;

#[derive(Debug)]
pub struct ModuleInfo<'src> {
    pub libraries: Libraries<'src>,
    pub includes: Includes<'src>,
}

impl<'src> From<&'src SourceModule> for ModuleInfo<'src> {
    fn from(value: &'src SourceModule) -> Self {
        let source = &value.source[..];
        let mut libraries = Default::default();
        let mut includes = Default::default();

        for pragma in value.module.children_with_kind(NodeKind::ModPragma) {
            if let Some(id) = pragma.get_token_by_kind(TokenKind::Ident) {
                match &source[id.span] {
                    "library" => parse_library(pragma, &mut libraries, source),
                    "include" => parse_include(pragma, &mut includes, source),
                    _ => (),
                }
            }
        }

        Self {
            libraries,
            includes,
        }
    }
}

fn parse_library<'src>(pragma: &Node, libraries: &mut Libraries<'src>, source: &'src str) {
    if let Some(args) = pragma.get_node_by_kind(NodeKind::CallArgs) {
        let args = args.get_nodes_by_kind(NodeKind::StringExpr);
        match args.len() {
            1 => {
                let name = args[0];
                if let Some(value) = name.get_token_by_kind(TokenKind::RawString) {
                    libraries.push(Library {
                        name: value.span.to_span_str(source),
                        version: None,
                    });
                }
            }
            2 => {
                let name = args[0].get_token_by_kind(TokenKind::RawString);
                let version = args[1].get_token_by_kind(TokenKind::RawString);
                if let (Some(name), Some(version)) = (name, version) {
                    libraries.push(Library {
                        name: name.span.to_span_str(source),
                        version: Some(version.span.to_span_str(source)),
                    });
                }
            }
            _ => (),
        }
    }
}

fn parse_include<'src>(pragma: &Node, includes: &mut Includes<'src>, source: &'src str) {
    if let Some(args) = pragma.get_node_by_kind(NodeKind::CallArgs) {
        if let Some(arg0) = args.get_node_by_kind(NodeKind::StringExpr) {
            if let Some(value) = arg0.get_token_by_kind(TokenKind::RawString) {
                includes.push(value.span.to_span_str(source));
            }
        }
    }
}
