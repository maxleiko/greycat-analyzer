use std::{
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

use greycat_analyzer_core::resolver::{Context, FsContext};

use crate::utils::AnyError;

#[derive(clap::Parser)]
#[clap(about = "Lints a project (parse-only stub; semantic diagnostics land in P1.4)")]
pub struct Lint {
    #[clap(help = "Path to a project.gcl")]
    project: PathBuf,
    #[clap(long, help = "Output in CSV", default_value = "false")]
    csv: bool,
}

impl Lint {
    pub fn run(self) -> Result<(), AnyError> {
        env_logger::init();

        let project_filepath = self.project.canonicalize()?;
        let project_dir = project_filepath
            .parent()
            .expect("unable to resolve project's parent directory");

        // FsContext::new resolves $GREYCAT_HOME; for the parse-only stub
        // we don't actually use greycat_home, so swallow that error and
        // fall back to a placeholder. Once P1.2 lands, the SourceManager
        // will own the Context and surface HomeError properly.
        let ctx = FsContext::new()
            .unwrap_or_else(|_| FsContext::with_greycat_home(PathBuf::new()));
        let files = ctx.iter_gcl(project_dir);

        let mut parser = greycat_analyzer_syntax::parser();

        let total_start = Instant::now();
        let mut entries: Vec<Entry> = Vec::with_capacity(files.len());
        for path in files {
            let source = std::fs::read_to_string(&path)?;
            let start = Instant::now();
            let tree = parser
                .parse(&source, None)
                .expect("tree-sitter parse never returns None without a cancellation flag");
            let took = start.elapsed();
            let root = tree.root_node();
            entries.push(Entry {
                path,
                took,
                nodes: root.descendant_count(),
                errors: count_error_nodes(root),
            });
        }
        let total = total_start.elapsed();
        entries.sort_by_key(|e| e.took);

        if self.csv {
            let mut w = std::io::stdout();
            writeln!(w, "duration_us,nb_nodes,nb_errors,filepath")?;
            for e in &entries {
                writeln!(
                    w,
                    "{},{},{},{}",
                    e.took.as_micros(),
                    e.nodes,
                    e.errors,
                    e.path.display()
                )?;
            }
        } else {
            println!("took     nodes    errors filepath");
            for e in &entries {
                println!(
                    "{:>8.2?} {:>8} {:>6} {}",
                    e.took,
                    e.nodes,
                    e.errors,
                    e.path.display()
                );
            }
            let total_errors: usize = entries.iter().map(|e| e.errors).sum();
            println!(
                "Total: {total:?} ({total_errors} parse errors across {} files)",
                entries.len()
            );
        }

        Ok(())
    }
}

struct Entry {
    path: PathBuf,
    took: Duration,
    nodes: usize,
    errors: usize,
}

/// Count `ERROR` and `MISSING` nodes anywhere in the tree.
fn count_error_nodes(node: greycat_analyzer_syntax::tree_sitter::Node<'_>) -> usize {
    if !node.has_error() && !node.is_missing() {
        return 0;
    }
    let mut count = 0;
    if node.is_error() || node.is_missing() {
        count += 1;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count += count_error_nodes(child);
    }
    count
}
