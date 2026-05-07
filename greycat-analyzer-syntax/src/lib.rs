use tree_sitter::{Language, Parser, Tree};

pub use tree_sitter;
pub use tree_sitter_greycat;

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
        assert_eq!(root.kind(), "module");
        assert!(!root.has_error(), "tree has errors: {}", root.to_sexp());
    }

    #[test]
    fn parses_empty_input() {
        let tree = parse("");
        assert_eq!(tree.root_node().kind(), "module");
    }

    #[test]
    fn surfaces_syntax_errors() {
        let tree = parse("fn main( {");
        assert!(tree.root_node().has_error());
    }
}
