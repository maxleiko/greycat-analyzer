use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

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

        let mut files = Vec::new();
        find_gcl_files(project_dir, &mut files);
        files.sort();

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

fn find_gcl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_gcl_files(&path, out);
        } else if path.is_file()
            && path.extension().and_then(|s| s.to_str()) == Some("gcl")
            && let Ok(canonical) = path.canonicalize()
        {
            out.push(canonical);
        }
    }
}
