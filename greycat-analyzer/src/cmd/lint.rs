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
    resolver::FsContext,
};

use crate::utils::AnyError;

#[derive(clap::Parser)]
#[clap(about = "Lint a GreyCat project. Loads the entrypoint and walks its \
             @library / @include pragmas to discover modules — only \
             reachable files are analyzed.")]
pub struct Lint {
    #[clap(help = "Path to a project.gcl entrypoint (or any single .gcl \
                file to lint in isolation).")]
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
    #[clap(
        long,
        value_enum,
        help = "Diagnostic rendering style. Defaults to `pretty` (miette: \
                source-snippet + caret + color) when stdout is a TTY and \
                `compact` (`path:line:col: severity: message`) when piped — \
                so the P10.3 parity oracle still gets a stable diffable \
                shape. Pass explicitly to override."
    )]
    format: Option<OutputFormat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Compact,
    Pretty,
}

impl OutputFormat {
    /// TTY-aware default: `pretty` interactively, `compact` when
    /// piped. Mirrors what the P10.7 roadmap entry promised
    /// (`miette when stdout is a TTY OR --format=pretty`).
    fn auto() -> Self {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            OutputFormat::Pretty
        } else {
            OutputFormat::Compact
        }
    }
}

impl Lint {
    pub fn run(self) -> Result<ExitCode, AnyError> {
        env_logger::init();

        let project_filepath = self.project.canonicalize()?;
        if project_filepath.is_dir() {
            eprintln!(
                "error: expected a path to project.gcl (or any .gcl entrypoint), got directory {}",
                project_filepath.display()
            );
            return Ok(ExitCode::FAILURE);
        }

        // Load the project graph properly: parse the entrypoint, walk
        // its `@library` / `@include` pragmas, and pull in only the
        // modules the entrypoint actually depends on. The previous
        // `iter_gcl(project_dir)` flat-walk picked up every `.gcl`
        // under the project directory regardless of inclusion — wrong
        // for GreyCat's project model, where the entrypoint's pragmas
        // are the source of truth.
        let total_start = Instant::now();
        let ctx = FsContext::new().unwrap_or_else(|_| FsContext::with_greycat_home(PathBuf::new()));
        let mut mgr = SourceManager::with_context(Arc::new(ctx));
        let report = mgr.load_project(&project_filepath);
        for lib in &report.unresolved_libraries {
            eprintln!("warning: unresolved @library('{lib}')");
        }
        for err in &report.errors {
            eprintln!("warning: {err}");
        }

        // One project-level analyzer pipeline over every reachable doc.
        let analysis = ProjectAnalysis::analyze(&mgr);
        let total = total_start.elapsed();

        // Hydrate cli `Entry`s from the manager's loaded set.
        let mut entries: Vec<Entry> = Vec::with_capacity(mgr.len());
        for (uri, cell) in mgr.iter() {
            let doc = cell.borrow();
            let path = uri_to_path(uri).unwrap_or_else(|| PathBuf::from(uri.as_str()));
            let mut diagnostics = parse_diagnostics(doc.root_node(), &doc.text);
            if let Some(module) = analysis.module(uri) {
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
                path,
                took: Duration::ZERO,
                nodes: doc.root_node().descendant_count(),
                diagnostics,
            });
        }

        // P8.4: lint fix-application driver. Each pass synthesizes
        // auto-fixes for the live diagnostics, applies non-overlapping
        // ones in reverse order, writes the file back, and re-runs
        // `load_project` + `ProjectAnalysis` so the @library/@include
        // closure stays the source of truth. Stops on convergence (no
        // fixes applied) or after 5 passes.
        let mut fixes_applied = 0usize;
        if self.fix {
            const MAX_PASSES: usize = 5;
            for _pass in 0..MAX_PASSES {
                let mut applied_this_pass = 0usize;
                for entry in &mut entries {
                    let Ok(original) = std::fs::read_to_string(&entry.path) else {
                        continue;
                    };
                    let mut edits: Vec<(std::ops::Range<usize>, String)> = entry
                        .diagnostics
                        .iter()
                        .filter_map(|d| diag_to_edit(&original, d))
                        .collect();
                    if edits.is_empty() {
                        continue;
                    }
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
                // Re-derive the project graph on disk now that files
                // have been edited.
                let mut new_mgr = SourceManager::with_context(Arc::new(
                    FsContext::new()
                        .unwrap_or_else(|_| FsContext::with_greycat_home(PathBuf::new())),
                ));
                let _ = new_mgr.load_project(&project_filepath);
                let new_analysis = ProjectAnalysis::analyze(&new_mgr);
                entries.clear();
                for (uri, cell) in new_mgr.iter() {
                    let doc = cell.borrow();
                    let path = uri_to_path(uri).unwrap_or_else(|| PathBuf::from(uri.as_str()));
                    let mut diagnostics = parse_diagnostics(doc.root_node(), &doc.text);
                    if let Some(module) = new_analysis.module(uri) {
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
                        path,
                        took: Duration::ZERO,
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
            // Human-readable: per-file diagnostic dump. P10.7: pretty
            // by default when stdout is a TTY (miette: snippet + caret
            // + color), compact when piped so the parity oracle and
            // grep-style consumers keep a stable shape. Explicit
            // `--format` always wins.
            let render = self.format.unwrap_or_else(OutputFormat::auto);
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            for e in &entries {
                let path = e.path.display().to_string();
                let source = std::fs::read_to_string(&e.path).unwrap_or_default();
                for diag in &e.diagnostics {
                    match render {
                        OutputFormat::Compact => {
                            println!("{}", format_cli(&path, diag));
                        }
                        OutputFormat::Pretty => {
                            print_pretty_diagnostic(&path, &source, diag);
                        }
                    }
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

struct Entry {
    path: PathBuf,
    took: Duration,
    nodes: usize,
    diagnostics: Vec<Diagnostic>,
}

/// Best-effort conversion of a `file://` URI back to a local path so
/// the cli can render `path:line:col:` shapes and read source for
/// pretty rendering. Non-file schemes return `None` and the caller
/// falls back to the URI string.
fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://")?;
    Some(PathBuf::from(stripped))
}

/// P10.7 pretty-rendered diagnostic — pipes through `miette` so the
/// user sees a source snippet, a caret pointing at the offending span,
/// and the diagnostic's severity / code / message rendered with color
/// when stdout is a TTY. Falls back to a plain `path:line:col:` line
/// if the byte range can't be resolved (e.g., zero-length spans).
fn print_pretty_diagnostic(path: &str, source: &str, diag: &Diagnostic) {
    use miette::{LabeledSpan, MietteDiagnostic, Severity as MietteSeverity};
    let start = lsp_to_byte(source, diag.range.start);
    let end = lsp_to_byte(source, diag.range.end).max(start + 1);
    let span = LabeledSpan::at(start..end.min(source.len()), diag.message.clone());
    let severity = match diag.severity {
        Some(DiagnosticSeverity::ERROR) => MietteSeverity::Error,
        Some(DiagnosticSeverity::WARNING) => MietteSeverity::Warning,
        Some(DiagnosticSeverity::INFORMATION) => MietteSeverity::Advice,
        Some(DiagnosticSeverity::HINT) => MietteSeverity::Advice,
        _ => MietteSeverity::Error,
    };
    let code = match &diag.code {
        Some(NumberOrString::String(s)) => s.clone(),
        _ => "diag".into(),
    };
    let mietted = MietteDiagnostic::new(diag.message.clone())
        .with_code(code)
        .with_severity(severity)
        .with_label(span);
    let report = miette::Report::new(mietted)
        .with_source_code(miette::NamedSource::new(path, source.to_string()));
    eprintln!("{report:?}");
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
