use std::collections::{HashMap, HashSet};

use crate::{
    TokenKind,
    cst::{CstNode, Node, NodeKind, SourceModule},
};

pub type Libraries<'src> = HashMap<&'src str, Option<&'src str>>;
pub type Includes<'src> = HashSet<&'src str>;

#[derive(Debug)]
pub struct ModuleInfo<'src> {
    pub libraries: Libraries<'src>,
    pub includes: Includes<'src>,
}

impl<'src> From<&'src SourceModule> for ModuleInfo<'src> {
    fn from(value: &'src SourceModule) -> Self {
        let source = &value.source[..];
        let mut libraries = HashMap::new();
        let mut includes = HashSet::new();

        for pragma in value.module.children_with_kind(NodeKind::ModPragma) {
            if let Some(id) = pragma.get_token_by_kind(TokenKind::Ident) {
                let name = &source[id.span];
                if name == "library" {
                    parse_library(pragma, &mut libraries, source);
                } else if name == "include" {
                    parse_include(pragma, &mut includes, source);
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
                    libraries.insert(&source[value.span], None);
                }
            }
            2 => {
                let name = args[0].get_token_by_kind(TokenKind::RawString);
                let version = args[1].get_token_by_kind(TokenKind::RawString);
                if let (Some(name), Some(version)) = (name, version) {
                    libraries.insert(&source[name.span], Some(&source[version.span]));
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
                includes.insert(&source[value.span]);
            }
        }
    }
}
