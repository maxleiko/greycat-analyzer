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
        if self.pretty {
            let mut out = String::new();
            write_pretty(tree.root_node(), 0, true, &mut out);
            println!("{out}");
        } else {
            println!("{}", tree.root_node().to_sexp());
        }
        Ok(())
    }
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
                write_pretty(cursor.node(), indent + 1, false, out);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    out.push(')');
}
