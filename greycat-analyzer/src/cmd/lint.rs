use std::{
    io::Write,
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};

use greycat_analyzer_core::{
    diagnostics::{format_cli, parse_diagnostics},
    lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range as LspRange},
    resolver::{Context, FsContext},
};
use greycat_analyzer_analysis::{
    analyzer::{Severity, analyze},
    lint::{LintSeverity, run_lints},
    resolver::resolve,
};
use greycat_analyzer_hir::lower_module;

use crate::utils::AnyError;

#[derive(clap::Parser)]
#[clap(about = "Lint a project: parse every reachable .gcl and print syntax diagnostics")]
pub struct Lint {
    #[clap(help = "Path to a project.gcl")]
    project: PathBuf,
    #[clap(long, help = "CSV per-file timing summary instead of human-readable diagnostics")]
    csv: bool,
}

impl Lint {
    pub fn run(self) -> Result<ExitCode, AnyError> {
        env_logger::init();

        let project_filepath = self.project.canonicalize()?;
        let project_dir = project_filepath
            .parent()
            .expect("unable to resolve project's parent directory");

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
            let mut diagnostics = parse_diagnostics(tree.root_node(), &source);
            // Pipe semantic + lint diagnostics through the cli output too.
            let hir = lower_module(&source, "module", "project", tree.root_node());
            let resolutions = resolve(&hir);
            let analysis = analyze(&hir, &resolutions);
            for d in &analysis.diagnostics {
                diagnostics.push(Diagnostic {
                    range: byte_range_to_lsp(&source, &d.byte_range),
                    severity: Some(match d.severity {
                        Severity::Error => DiagnosticSeverity::ERROR,
                        Severity::Warning => DiagnosticSeverity::WARNING,
                        Severity::Hint => DiagnosticSeverity::HINT,
                    }),
                    code: Some(NumberOrString::String("semantic".into())),
                    source: Some("greycat-analyzer".into()),
                    message: d.message.clone(),
                    ..Default::default()
                });
            }
            for l in run_lints(&hir, &resolutions) {
                diagnostics.push(Diagnostic {
                    range: byte_range_to_lsp(&source, &l.byte_range),
                    severity: Some(match l.severity {
                        LintSeverity::Warning => DiagnosticSeverity::WARNING,
                        LintSeverity::Hint => DiagnosticSeverity::HINT,
                    }),
                    code: Some(NumberOrString::String(l.rule.into())),
                    source: Some("lint".into()),
                    message: l.message,
                    ..Default::default()
                });
            }
            entries.push(Entry {
                path,
                source,
                took,
                nodes: tree.root_node().descendant_count(),
                diagnostics,
            });
        }
        let total = total_start.elapsed();

        let total_diagnostics: usize = entries.iter().map(|e| e.diagnostics.len()).sum();

        if self.csv {
            let mut w = std::io::stdout();
            writeln!(w, "duration_us,nb_nodes,nb_diagnostics,filepath")?;
            // CSV mode preserves the timing-sorted view from the previous stub.
            entries.sort_by_key(|e| e.took);
            for e in &entries {
                writeln!(
                    w,
                    "{},{},{},{}",
                    e.took.as_micros(),
                    e.nodes,
                    e.diagnostics.len(),
                    e.path.display()
                )?;
            }
        } else {
            // Human-readable: per-file diagnostic dump (matching the TS
            // reference shape: `path:line:col: severity: message`),
            // followed by a one-line summary.
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            for e in &entries {
                let path = e.path.display().to_string();
                for diag in &e.diagnostics {
                    println!("{}", format_cli(&path, diag));
                }
            }
            println!(
                "{} diagnostic(s) across {} file(s) in {total:?}",
                total_diagnostics,
                entries.len(),
            );
            // Suppress unused warning while we don't print the source snippet.
            for e in &entries {
                let _ = &e.source;
            }
        }

        Ok(if total_diagnostics == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        })
    }
}

struct Entry {
    path: PathBuf,
    source: String,
    took: Duration,
    nodes: usize,
    diagnostics: Vec<Diagnostic>,
}

fn byte_range_to_lsp(text: &str, range: &std::ops::Range<usize>) -> LspRange {
    fn position_at(text: &str, byte: usize) -> Position {
        let mut line = 0u32;
        let mut col = 0u32;
        let prefix = &text[..byte.min(text.len())];
        for c in prefix.chars() {
            if c == '\n' {
                line += 1;
                col = 0;
            } else {
                col += c.len_utf8() as u32;
            }
        }
        Position {
            line,
            character: col,
        }
    }
    LspRange {
        start: position_at(text, range.start),
        end: position_at(text, range.end),
    }
}
