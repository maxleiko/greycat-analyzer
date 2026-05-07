use tree_sitter::{Language, Parser, Tree};

pub use tree_sitter;
pub use tree_sitter_greycat;

#[allow(non_snake_case, dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/generated.rs"));
}
pub use generated::*;

pub fn language() -> Language {
    tree_sitter_greycat::LANGUAGE.into()
}

pub fn parser() -> Parser {
    let mut p = Parser::new();
    p.set_language(&language())
        .expect("tree-sitter-greycat language loads");
    p
}

pub fn parse(source: &str) -> Tree {
    parser()
        .parse(source, None)
        .expect("tree-sitter parse never returns None without a cancellation flag")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hello_world() {
        let src = r#"fn main() {
    println("Hello world");
}
"#;
        let tree = parse(src);
        let root = tree.root_node();
        assert_eq!(root.kind(), kind::MODULE);
        assert!(!root.has_error(), "tree has errors: {}", root.to_sexp());
    }

    #[test]
    fn parses_empty_input() {
        let tree = parse("");
        assert_eq!(tree.root_node().kind(), kind::MODULE);
    }

    #[test]
    fn surfaces_syntax_errors() {
        let tree = parse("fn main( {");
        assert!(tree.root_node().has_error());
    }

    #[test]
    fn typed_fn_decl_field_accessors() {
        let src = "fn greet(name: String): String { return name; }\n";
        let tree = parse(src);
        let module = Module::cast(tree.root_node()).expect("root is module");
        let fn_node = module
            .children()
            .find(|n| n.kind() == kind::FN_DECL)
            .expect("module contains an fn_decl");
        let fn_decl = FnDecl::cast(fn_node).expect("cast to FnDecl");

        let name = fn_decl.name().expect("fn has name");
        assert_eq!(name.kind(), kind::IDENT);
        assert_eq!(&src[name.byte_range()], "greet");

        assert!(fn_decl.params().is_some(), "fn has params");
        assert!(fn_decl.return_type().is_some(), "fn has return type");
        assert!(fn_decl.body().is_some(), "fn has body");
    }

    #[test]
    fn node_ext_cast_round_trip() {
        let tree = parse("fn f() {}\n");
        let module = tree.root_node();
        let typed = module.cast::<Module>().expect("module casts");
        assert_eq!(typed.node().kind(), kind::MODULE);
        // Wrong-kind cast returns None.
        assert!(module.cast::<FnDecl>().is_none());
    }

}
