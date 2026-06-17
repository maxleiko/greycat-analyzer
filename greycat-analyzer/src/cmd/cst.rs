use std::path::PathBuf;

use crate::utils::AnyError;

#[derive(clap::Parser)]
#[clap(about = "Prints the CST s-expression of a .gcl file")]
pub struct Cst {
    #[clap(help = "Path to a .gcl file (or a directory containing \
                project.gcl). When omitted, looks for `project.gcl` in the \
                current working directory.")]
    file: Option<PathBuf>,
    #[clap(short, long, help = "Pretty-print the s-expr", default_value = "false")]
    pretty: bool,
}

impl Cst {
    pub fn run(self) -> Result<(), AnyError> {
        env_logger::init();
        // When omitted, default to `./project.gcl`; a directory argument
        // resolves to its `project.gcl` (mirrors `lint` / `fmt`).
        let mut file = match self.file {
            Some(p) => p,
            None => std::env::current_dir()?,
        };
        if file.is_dir() {
            file = file.join("project.gcl");
        }
        let source = std::fs::read_to_string(&file)
            .map_err(|e| format!("failed to read {}: {e}", file.display()))?;
        let tree = greycat_analyzer_syntax::parse(&source);
        let src = source.as_bytes();
        let mut out = String::new();
        if self.pretty {
            write_pretty(tree.root_node(), 0, true, src, &mut out);
        } else {
            write_compact(tree.root_node(), src, &mut out);
        }
        println!("{out}");
        Ok(())
    }
}

/// Compact one-line s-expr, mirroring `Node::to_sexp()` but appending the
/// source text of named leaf nodes (idents, literals) as a quoted string.
fn write_compact(
    node: greycat_analyzer_syntax::tree_sitter::Node<'_>,
    src: &[u8],
    out: &mut String,
) {
    out.push('(');
    out.push_str(node.kind());
    if node.named_child_count() == 0 {
        push_leaf_text(node, src, out);
        out.push(')');
        return;
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().is_named() {
                out.push(' ');
                if let Some(field) = cursor.field_name() {
                    out.push_str(field);
                    out.push_str(": ");
                }
                write_compact(cursor.node(), src, out);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    out.push(')');
}

/// Recursive s-expr pretty-printer. Emits `(kind field: child …)` with
/// two-space indent per level. Each named child lands on its own line;
/// anonymous tokens are skipped (matching `to_sexp()`'s behavior).
/// `write_lead_pad` controls whether the opening `(` is prefixed by the
/// indent — `false` when the caller already wrote a `field: ` prefix on
/// the current line.
fn write_pretty(
    node: greycat_analyzer_syntax::tree_sitter::Node<'_>,
    indent: usize,
    write_lead_pad: bool,
    src: &[u8],
    out: &mut String,
) {
    if write_lead_pad {
        for _ in 0..indent {
            out.push_str("  ");
        }
    }
    out.push('(');
    out.push_str(node.kind());
    if node.named_child_count() == 0 {
        push_leaf_text(node, src, out);
        out.push(')');
        return;
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().is_named() {
                out.push('\n');
                for _ in 0..indent + 1 {
                    out.push_str("  ");
                }
                if let Some(field) = cursor.field_name() {
                    out.push_str(field);
                    out.push_str(": ");
                }
                write_pretty(cursor.node(), indent + 1, false, src, out);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    out.push(')');
}

/// Append the source text of a named leaf node (ident, number, string, …).
/// String leaves are quoted; everything else is emitted bare. Newlines are
/// escaped to `\n` so each leaf stays on one line. Empty (MISSING) text is
/// skipped.
fn push_leaf_text(
    node: greycat_analyzer_syntax::tree_sitter::Node<'_>,
    src: &[u8],
    out: &mut String,
) {
    let Ok(text) = node.utf8_text(src) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    out.push(' ');
    for ch in text.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
}
