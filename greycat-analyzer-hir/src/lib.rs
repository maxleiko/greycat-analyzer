//! HIR for greycat — typed surface tree built by lowering tree-sitter CST.
//!
//! Single typed-AST + arena layout (Decision B). Each concrete shape is in
//! [`types`]; the lowering walker is in [`lower`]. P2.1 lays the
//! scaffolding; later phases (resolver / type system / analyzer) flesh
//! out shapes that don't yet have a non-`Unsupported` variant.

pub mod arena;
pub mod lower;
pub mod types;

use arena::Arena;
use types::*;

/// Per-source-file HIR. Holds typed arenas plus the top-level [`Module`]
/// (set by [`lower::lower_module`]).
#[derive(Debug, Default)]
pub struct Hir {
    pub module: Option<Module>,
    pub decls: Arena<Decl>,
    pub stmts: Arena<Stmt>,
    pub exprs: Arena<Expr>,
    pub idents: Arena<Ident>,
    pub fn_params: Arena<FnParam>,
    pub type_refs: Arena<TypeRef>,
    pub type_attrs: Arena<TypeAttr>,
    pub enum_fields: Arena<EnumField>,
}

pub use lower::{LowerCtx, lower_module};

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_syntax::parse;

    #[test]
    fn lowers_simple_function() {
        let src = "fn greet(name: String): String { return name; }\n";
        let tree = parse(src);
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().expect("module produced");
        assert_eq!(module.decls.len(), 1);

        let decl = &hir.decls[module.decls[0]];
        let Decl::Fn(fnd) = decl else {
            panic!("expected fn decl, got {decl:?}");
        };
        assert_eq!(hir.idents[fnd.name].text, "greet");
        assert_eq!(fnd.params.len(), 1);
        let param = &hir.fn_params[fnd.params[0]];
        assert_eq!(hir.idents[param.name].text, "name");
        assert!(fnd.return_type.is_some());
        assert!(fnd.body.is_some());
    }

    #[test]
    fn lowers_type_decl_with_attrs_and_methods() {
        let src = r#"
type Point {
    x: int;
    y: int;
    fn distance(): float { return 0; }
}
"#;
        let tree = parse(src);
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().unwrap();
        assert_eq!(module.decls.len(), 1);
        let Decl::Type(td) = &hir.decls[module.decls[0]] else {
            panic!("expected type decl");
        };
        assert_eq!(hir.idents[td.name].text, "Point");
        assert_eq!(td.attrs.len(), 2);
        assert_eq!(td.methods.len(), 1);
        assert_eq!(hir.idents[hir.type_attrs[td.attrs[0]].name].text, "x");
    }

    #[test]
    fn lowers_enum_decl() {
        let src = "enum Color { Red, Green, Blue }\n";
        let tree = parse(src);
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().unwrap();
        let Decl::Enum(ed) = &hir.decls[module.decls[0]] else {
            panic!("expected enum");
        };
        assert_eq!(ed.fields.len(), 3);
        let names: Vec<&str> = ed
            .fields
            .iter()
            .map(|f| hir.idents[hir.enum_fields[*f].name].text.as_str())
            .collect();
        assert_eq!(names, vec!["Red", "Green", "Blue"]);
    }

    #[test]
    fn lowers_module_pragmas() {
        let src = "@library(\"std\", \"1.0\");\n@expose;\n";
        let tree = parse(src);
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().unwrap();
        let pragma_names: Vec<&str> = module
            .decls
            .iter()
            .filter_map(|d| match &hir.decls[*d] {
                Decl::Pragma(p) => Some(hir.idents[p.name].text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(pragma_names, vec!["library", "expose"]);
    }

    #[test]
    fn lowers_expressions_inside_body() {
        let src = "fn calc(): int { return 1 + 2 * 3; }\n";
        let tree = parse(src);
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().unwrap();
        let Decl::Fn(fnd) = &hir.decls[module.decls[0]] else {
            panic!()
        };
        let body = fnd.body.unwrap();
        let Stmt::Block(stmts) = &hir.stmts[body] else {
            panic!()
        };
        assert_eq!(stmts.len(), 1);
        let Stmt::Return(Some(ret)) = &hir.stmts[stmts[0]] else {
            panic!()
        };
        let Expr::Binary(top) = &hir.exprs[*ret] else {
            panic!()
        };
        assert!(matches!(top.op, BinOp::Add));
        let Expr::Binary(rhs) = &hir.exprs[top.right] else {
            panic!()
        };
        assert!(matches!(rhs.op, BinOp::Mul));
    }
}
