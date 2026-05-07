use std::{
    io::Write,
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
    time::{Duration, Instant},
};

use greycat_analyzer_analysis::{analyzer::Severity, lint::LintSeverity, project::ProjectAnalysis};
use greycat_analyzer_core::{
    SourceManager,
    diagnostics::{format_cli, parse_diagnostics},
    lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range as LspRange, Uri},
    resolver::{Context, FsContext},
};

use crate::utils::AnyError;

#[derive(clap::Parser)]
#[clap(about = "Lint a project: parse every reachable .gcl and print syntax diagnostics")]
pub struct Lint {
    #[clap(help = "Path to a project.gcl")]
    project: PathBuf,
    #[clap(
        long,
        help = "CSV per-file timing summary instead of human-readable diagnostics"
    )]
    csv: bool,
    #[clap(
        long,
        help = "Apply auto-fixable lint suggestions in place (P8.4). \
                Sorts edits by start position, drops overlaps, applies \
                non-overlapping ones, and re-runs until convergence \
                (max 5 passes)."
    )]
    fix: bool,
}

impl Lint {
    pub fn run(self) -> Result<ExitCode, AnyError> {
        env_logger::init();

        let project_filepath = self.project.canonicalize()?;
        let project_dir = project_filepath
            .parent()
            .expect("unable to resolve project's parent directory");

        let ctx = FsContext::new().unwrap_or_else(|_| FsContext::with_greycat_home(PathBuf::new()));
        let files = ctx.iter_gcl(project_dir);

        // Pass 1: read + parse every file by feeding it through a
        // `SourceManager`. `add_simple` runs the tree-sitter parse so
        // per-file parse time falls out naturally.
        let total_start = Instant::now();
        let mut mgr = SourceManager::with_context(Arc::new(ctx));
        let mut stubs: Vec<EntryStub> = Vec::with_capacity(files.len());
        for path in files {
            let source = std::fs::read_to_string(&path)?;
            let parse_start = Instant::now();
            let uri = path_to_uri(&path);
            mgr.add_simple(uri.clone(), source, "project", false);
            stubs.push(EntryStub {
                path,
                uri,
                took: parse_start.elapsed(),
            });
        }

        // Pass 2: one project-level analyzer pipeline over every doc.
        // Replaces the previous per-file `lower → resolve → analyze →
        // run_lints` loop (P6.1 acceptance criterion).
        let analysis = ProjectAnalysis::analyze(&mgr);
        let total = total_start.elapsed();

        // Hydrate cli `Entry`s with diagnostics from the cache.
        let mut entries: Vec<Entry> = Vec::with_capacity(stubs.len());
        for stub in stubs {
            let cell = mgr.get(&stub.uri).expect("doc must be in manager");
            let doc = cell.borrow();
            let mut diagnostics = parse_diagnostics(doc.root_node(), &doc.text);
            if let Some(module) = analysis.module(&stub.uri) {
                for d in &module.analysis.diagnostics {
                    diagnostics.push(Diagnostic {
                        range: byte_range_to_lsp(&doc.text, &d.byte_range),
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
                for l in &module.lints {
                    diagnostics.push(Diagnostic {
                        range: byte_range_to_lsp(&doc.text, &l.byte_range),
                        severity: Some(match l.severity {
                            LintSeverity::Warning => DiagnosticSeverity::WARNING,
                            LintSeverity::Hint => DiagnosticSeverity::HINT,
                        }),
                        code: Some(NumberOrString::String(l.rule.into())),
                        source: Some("lint".into()),
                        message: l.message.clone(),
                        ..Default::default()
                    });
                }
            }
            entries.push(Entry {
                path: stub.path,
                took: stub.took,
                nodes: doc.root_node().descendant_count(),
                diagnostics,
            });
        }

        // P8.4: lint fix-application driver. Re-runs the pipeline up
        // to N times, each pass synthesizes auto-fixes for the live
        // diagnostics, applies non-overlapping ones in reverse order,
        // and writes the file back. Stops on convergence (no fixes
        // applied) or after 5 passes.
        let mut fixes_applied = 0usize;
        if self.fix {
            const MAX_PASSES: usize = 5;
            for _pass in 0..MAX_PASSES {
                let mut applied_this_pass = 0usize;
                for entry in &mut entries {
                    let original = std::fs::read_to_string(&entry.path)?;
                    let mut edits: Vec<(std::ops::Range<usize>, String)> = entry
                        .diagnostics
                        .iter()
                        .filter_map(|d| diag_to_edit(&original, d))
                        .collect();
                    if edits.is_empty() {
                        continue;
                    }
                    // Sort by start; drop overlaps; apply in reverse so
                    // earlier ranges keep their byte offsets.
                    edits.sort_by_key(|(r, _)| r.start);
                    let mut non_overlap: Vec<(std::ops::Range<usize>, String)> = Vec::new();
                    let mut last_end = 0usize;
                    for (r, t) in edits {
                        if r.start < last_end {
                            continue;
                        }
                        last_end = r.end;
                        non_overlap.push((r, t));
                    }
                    if non_overlap.is_empty() {
                        continue;
                    }
                    let mut new_text = original;
                    for (r, t) in non_overlap.into_iter().rev() {
                        new_text.replace_range(r, &t);
                        applied_this_pass += 1;
                    }
                    std::fs::write(&entry.path, new_text)?;
                }
                if applied_this_pass == 0 {
                    break;
                }
                fixes_applied += applied_this_pass;
                // Re-run analysis on the edited files for the next pass.
                let mut new_mgr = SourceManager::with_context(Arc::new(
                    FsContext::with_greycat_home(PathBuf::new()),
                ));
                let mut new_stubs: Vec<EntryStub> = Vec::with_capacity(entries.len());
                for entry in &entries {
                    let source = std::fs::read_to_string(&entry.path)?;
                    let uri = path_to_uri(&entry.path);
                    new_mgr.add_simple(uri.clone(), source, "project", false);
                    new_stubs.push(EntryStub {
                        path: entry.path.clone(),
                        uri,
                        took: entry.took,
                    });
                }
                let new_analysis = ProjectAnalysis::analyze(&new_mgr);
                entries.clear();
                for stub in new_stubs {
                    let cell = new_mgr.get(&stub.uri).expect("doc must be in manager");
                    let doc = cell.borrow();
                    let mut diagnostics = parse_diagnostics(doc.root_node(), &doc.text);
                    if let Some(module) = new_analysis.module(&stub.uri) {
                        for d in &module.analysis.diagnostics {
                            diagnostics.push(Diagnostic {
                                range: byte_range_to_lsp(&doc.text, &d.byte_range),
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
                        for l in &module.lints {
                            diagnostics.push(Diagnostic {
                                range: byte_range_to_lsp(&doc.text, &l.byte_range),
                                severity: Some(match l.severity {
                                    LintSeverity::Warning => DiagnosticSeverity::WARNING,
                                    LintSeverity::Hint => DiagnosticSeverity::HINT,
                                }),
                                code: Some(NumberOrString::String(l.rule.into())),
                                source: Some("lint".into()),
                                message: l.message.clone(),
                                ..Default::default()
                            });
                        }
                    }
                    entries.push(Entry {
                        path: stub.path,
                        took: stub.took,
                        nodes: doc.root_node().descendant_count(),
                        diagnostics,
                    });
                }
            }
        }

        let total_diagnostics: usize = entries.iter().map(|e| e.diagnostics.len()).sum();

        if self.fix && fixes_applied > 0 {
            println!("[fix] applied {fixes_applied} edit(s)");
        }

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
        }

        Ok(if total_diagnostics == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        })
    }
}

struct EntryStub {
    path: PathBuf,
    uri: Uri,
    took: Duration,
}

struct Entry {
    path: PathBuf,
    took: Duration,
    nodes: usize,
    diagnostics: Vec<Diagnostic>,
}

fn path_to_uri(path: &std::path::Path) -> Uri {
    let s = format!("file://{}", path.display());
    s.parse::<Uri>()
        .unwrap_or_else(|_| "file:///invalid".parse().unwrap())
}

/// P8.4 fix synthesis — diagnostic → byte-range + replacement text.
/// Returns `None` for diagnostics that don't have an automatic fix.
fn diag_to_edit(text: &str, diag: &Diagnostic) -> Option<(std::ops::Range<usize>, String)> {
    let code = match diag.code.as_ref()? {
        NumberOrString::String(s) => s.as_str(),
        _ => return None,
    };
    let start = lsp_to_byte(text, diag.range.start);
    let end = lsp_to_byte(text, diag.range.end);
    match code {
        "missing-token" => {
            let token = diag
                .message
                .split_once('`')
                .and_then(|(_, rest)| rest.split_once('`').map(|(t, _)| t))?;
            Some((start..start, token.to_string()))
        }
        "unused-local" | "unused-decl" => {
            if end > start && end <= text.len() {
                Some((start..end, String::new()))
            } else {
                None
            }
        }
        "unused-param" => {
            if end > start && end <= text.len() {
                let name = &text[start..end];
                Some((start..end, format!("_{name}")))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn lsp_to_byte(text: &str, pos: Position) -> usize {
    let mut line = 0u32;
    let mut byte = 0usize;
    for c in text.chars() {
        if line == pos.line && pos.character == 0 {
            return byte;
        }
        if c == '\n' {
            if line == pos.line {
                return byte;
            }
            line += 1;
            byte += c.len_utf8();
            continue;
        }
        if line == pos.line {
            let col = (byte - text[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
            if col == pos.character {
                return byte;
            }
        }
        byte += c.len_utf8();
    }
    byte
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
