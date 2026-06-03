//! HIR for greycat — typed surface tree built by lowering tree-sitter CST.
//! Shapes are in [`types`]; the lowering walker is in [`lower`].

pub mod arena;
pub mod lower;
pub mod types;

use arena::Arena;
use arena::Idx;
use rustc_hash::FxHashSet;
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
    // P43.2
    /// Statement ids salvaged from inside a CST `ERROR` wrapper by
    /// [`lower::flatten_errors_named_children`]. Consumers that assume
    /// complete code skip these. Empty for well-formed sources.
    pub salvaged_stmts: FxHashSet<Idx<Stmt>>,
}

pub use lower::{LowerCtx, lower_module};

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_core::SymbolTable;
    use greycat_analyzer_syntax::parse;

    #[test]
    fn lowers_simple_function() {
        let src = "fn greet(name: String): String { return name; }\n";
        let tree = parse(src);
        let s = SymbolTable::default();
        let hir = lower_module(src, &s, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().expect("module produced");
        assert_eq!(module.decls.len(), 1);

        let decl = &hir.decls[module.decls[0]];
        let Decl::Fn(fnd) = decl else {
            panic!("expected fn decl, got {decl:?}");
        };
        assert_eq!(&s[hir.idents[fnd.name].symbol], "greet");
        assert_eq!(fnd.params.len(), 1);
        let param = &hir.fn_params[fnd.params[0]];
        assert_eq!(&s[hir.idents[param.name].symbol], "name");
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
        let symbols = SymbolTable::default();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().unwrap();
        assert_eq!(module.decls.len(), 1);
        let Decl::Type(td) = &hir.decls[module.decls[0]] else {
            panic!("expected type decl");
        };
        assert_eq!(&symbols[hir.idents[td.name].symbol], "Point");
        assert_eq!(td.attrs.len(), 2);
        assert_eq!(td.methods.len(), 1);
        assert_eq!(
            &symbols[hir.idents[hir.type_attrs[td.attrs[0]].name].symbol],
            "x"
        );
    }

    #[test]
    fn lowers_enum_decl() {
        let src = "enum Color { Red, Green, Blue }\n";
        let tree = parse(src);
        let symbols = SymbolTable::default();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().unwrap();
        let Decl::Enum(ed) = &hir.decls[module.decls[0]] else {
            panic!("expected enum");
        };
        assert_eq!(ed.fields.len(), 3);
        let names: Vec<&str> = ed
            .fields
            .iter()
            .map(|f| &symbols[hir.idents[hir.enum_fields[*f].name].symbol])
            .collect();
        assert_eq!(names, vec!["Red", "Green", "Blue"]);
    }

    #[test]
    fn lowers_module_pragmas() {
        let src = "@library(\"std\", \"1.0\");\n@expose;\n";
        let tree = parse(src);
        let symbols = SymbolTable::default();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().unwrap();
        let pragma_names: Vec<&str> = module
            .decls
            .iter()
            .filter_map(|d| match &hir.decls[*d] {
                Decl::Pragma(p) => Some(&symbols[hir.idents[p.name].symbol]),
                _ => None,
            })
            .collect();
        assert_eq!(pragma_names, vec!["library", "expose"]);
    }

    #[test]
    fn lowers_expressions_inside_body() {
        let src = "fn calc(): int { return 1 + 2 * 3; }\n";
        let tree = parse(src);
        let symbols = SymbolTable::default();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().unwrap();
        let Decl::Fn(fnd) = &hir.decls[module.decls[0]] else {
            panic!()
        };
        let body = fnd.body.unwrap();
        let Stmt::Block(block) = &hir.stmts[body] else {
            panic!()
        };
        assert_eq!(block.stmts.len(), 1);
        let Stmt::Return(ReturnStmt {
            value: Some(ret), ..
        }) = &hir.stmts[block.stmts[0]]
        else {
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

    #[test]
    fn object_expr_named_fields_lower_their_values() {
        // Named-object (`object_fields`) value exprs must be lowered.
        let src = r#"
type Foo { name: String; age: int; }
fn build(n: String, a: int): Foo {
    return Foo { name: n, age: a };
}
"#;
        let tree = parse(src);
        let symbols = SymbolTable::default();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().unwrap();
        let Decl::Fn(fnd) = &hir.decls[module.decls[1]] else {
            panic!("expected fn decl")
        };
        let body = fnd.body.unwrap();
        let Stmt::Block(block) = &hir.stmts[body] else {
            panic!("expected block")
        };
        let Stmt::Return(ReturnStmt {
            value: Some(ret), ..
        }) = &hir.stmts[block.stmts[0]]
        else {
            panic!("expected return")
        };
        let Expr::Object(obj) = &hir.exprs[*ret] else {
            panic!("expected object expr, got {:?}", &hir.exprs[*ret])
        };
        assert_eq!(obj.fields.len(), 2, "named fields must be lowered");
        // The name slot is a full `Expr`; a classic field is the
        // bare-ident key (`name`).
        let Expr::Ident { name: key_use, .. } = &hir.exprs[obj.fields[0].name] else {
            panic!("expected ident key for field name")
        };
        assert_eq!(&symbols[hir.idents[*key_use].symbol], "name");
        let Expr::Ident { name: name_use, .. } = &hir.exprs[obj.fields[0].value] else {
            panic!("expected ident use for value")
        };
        assert_eq!(&symbols[hir.idents[*name_use].symbol], "n");
    }

    /// `if (c.sim.)` parses as a well-formed nested `member_expr` with
    /// a missing property. `salvage_incomplete_members_in_block` lifts
    /// the receiver as `Stmt::Expr` salvage so the analyzer types it and
    /// IDE capabilities work on the receiver.
    #[test]
    fn block_lowering_salvages_incomplete_member_receiver() {
        let src = "type Ctx { sim: int; }\nfn test(c: Ctx) {\n    if (c.sim.)\n}\n";
        let tree = parse(src);
        let symbols = SymbolTable::default();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().unwrap();
        let fn_decl = module
            .decls
            .iter()
            .find_map(|d| match &hir.decls[*d] {
                Decl::Fn(fnd) => Some(fnd),
                _ => None,
            })
            .expect("fn_decl lowered");
        let body = fn_decl.body.expect("fn body lowered");
        let Stmt::Block(block) = &hir.stmts[body] else {
            panic!("body is a block");
        };
        // Exactly one salvaged Stmt::Expr wrapping a `Member(c, sim)`
        // receiver, tagged salvaged so lints skip it.
        let salvaged_members: Vec<_> = block
            .stmts
            .iter()
            .filter(|s| match &hir.stmts[**s] {
                Stmt::Expr(e) => matches!(&hir.exprs[*e], Expr::Member(_)),
                _ => false,
            })
            .collect();
        assert_eq!(
            salvaged_members.len(),
            1,
            "exactly one salvaged member-receiver stmt expected, got: {:?}",
            block.stmts
        );
        assert!(
            hir.salvaged_stmts.contains(salvaged_members[0]),
            "salvaged stmt id must be tagged in Hir::salvaged_stmts"
        );
    }

    // P43.3
    /// Well-formed sources never populate `salvaged_stmts` — the
    /// marker fires only when tree-sitter actually recovered something
    /// from an `ERROR` wrapper.
    #[test]
    fn salvaged_stmts_empty_on_well_formed_source() {
        let src = "fn f() { var x = 1; var y = x + 2; return y; }\n";
        let tree = parse(src);
        let symbols = SymbolTable::default();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        assert!(
            hir.salvaged_stmts.is_empty(),
            "well-formed source must not populate salvaged_stmts; got {:?}",
            hir.salvaged_stmts
        );
    }

    #[test]
    fn object_expr_positional_initializers_lower_each_value() {
        // Positional form `node<T> { val }` — each child is a bare
        // `_expr`, lowered as a positional field.
        let src = r#"
type Group { name: String; }
fn make(g: Group) {
    var n = node<Group> { g };
}
"#;
        let tree = parse(src);
        let symbols = SymbolTable::default();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().unwrap();
        let Decl::Fn(fnd) = &hir.decls[module.decls[1]] else {
            panic!("expected fn decl")
        };
        let body = fnd.body.unwrap();
        let Stmt::Block(block) = &hir.stmts[body] else {
            panic!("expected block")
        };
        let Stmt::Var(var) = &hir.stmts[block.stmts[0]] else {
            panic!("expected var stmt")
        };
        let init_expr = var.init.expect("initializer present");
        let Expr::PositionalObject(obj) = &hir.exprs[init_expr] else {
            panic!("expected positional object expr")
        };
        assert_eq!(obj.fields.len(), 1, "single positional value");
        // Positional fields are bare value exprs — no name slot.
        let Expr::Ident { name: used, .. } = &hir.exprs[obj.fields[0]] else {
            panic!("expected ident use")
        };
        assert_eq!(&symbols[hir.idents[*used].symbol], "g");
    }

    /// Comments are named nodes that show up as children of expression
    /// lists; they must NOT lower to phantom elements. `Foo { /* c */ }`
    /// stays an empty positional body, and `[1, /* c */ 2]` stays a
    /// two-element array — not three.
    #[test]
    fn comments_are_not_lowered_as_expressions() {
        let src = r#"
type Foo {}
fn main() {
    var _a = Foo { /* nClusters + 2 */ };
    var _b = [1, /* skip me */ 2];
}
"#;
        let tree = parse(src);
        let symbols = SymbolTable::default();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let module = hir.module.as_ref().unwrap();
        let Decl::Fn(fnd) = &hir.decls[module.decls[1]] else {
            panic!("expected fn decl")
        };
        let Stmt::Block(block) = &hir.stmts[fnd.body.unwrap()] else {
            panic!("expected block")
        };
        // `Foo { /* c */ }` — empty positional body, no phantom field.
        let Stmt::Var(a) = &hir.stmts[block.stmts[0]] else {
            panic!("expected var _a")
        };
        let Expr::PositionalObject(obj) = &hir.exprs[a.init.unwrap()] else {
            panic!("expected positional object")
        };
        assert!(
            obj.fields.is_empty(),
            "comment must not become a field: {:?}",
            obj.fields
        );
        // `[1, /* c */ 2]` — two elements, comment skipped.
        let Stmt::Var(b) = &hir.stmts[block.stmts[1]] else {
            panic!("expected var _b")
        };
        let Expr::Array(items, _) = &hir.exprs[b.init.unwrap()] else {
            panic!("expected array")
        };
        assert_eq!(items.len(), 2, "comment must not become an element");
        // And no `Unsupported` leaked into the arena anywhere.
        assert!(
            !hir.exprs
                .iter()
                .any(|(_, e)| matches!(e, Expr::Unsupported { .. })),
            "no phantom Unsupported expr should be minted from comments"
        );
    }
}
