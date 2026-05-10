use std::{
    io::Write,
    path::{Path, PathBuf},
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

use crate::utils::{AnyError, ColorMode};

#[derive(clap::Parser)]
pub struct Lint {
    #[clap(help = "Path to a project.gcl entrypoint (or any single .gcl \
                file to lint in isolation). When omitted, looks for \
                `project.gcl` in the current working directory.")]
    project: Option<PathBuf>,
    #[clap(
        long,
        help = "Apply auto-fixable lint suggestions in place (max 5 passes)"
    )]
    fix: bool,
    #[clap(
        long,
        value_enum,
        help = "Diagnostic rendering style\n\
                compact  `path:line:col: severity: message` per diagnostic, plus summary\n\
                pretty   source-snippet, caret, colors, plus summary\n\
                csv      per-file timing rows (sorted by total descending), no summary\n\
                quiet    summary line only (exit code is the gate)\n"
    )]
    format: Option<OutputFormat>,
    #[clap(long, help = "Also lint `lib/<name>/` modules (default: project only)")]
    lint_libs: bool,
    #[clap(
        long,
        help = "Print every registered lint rule (one per line, with a one-line summary) and exit"
    )]
    list_rules: bool,
    #[clap(
        long,
        help = "Re-emit diagnostics silenced by `// gcl-lint-off …` directives"
    )]
    no_suppressions: bool,
    #[clap(
        long,
        value_enum,
        default_value_t = ColorMode::Auto,
        help = "auto    color when stdout is a TTY and `NO_COLOR` is unset (default)\n\
                always  always emit ANSI color escapes\n\
                never   never color\n"
    )]
    color: ColorMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// `path:line:col: severity: message` per diagnostic, plus the
    /// trailing severity-count summary.
    Compact,
    /// `miette` source-snippet + caret rendering per diagnostic,
    /// plus the trailing summary. Default on a TTY.
    Pretty,
    /// Per-file timing rows, sorted by total descending. No
    /// per-diagnostic dump and no summary — the consumer is a script
    /// piping into `awk` / a spreadsheet, not a human.
    Csv,
    /// Trailing severity-count summary only (no per-diagnostic
    /// output). Useful in CI / pre-commit where the exit code is
    /// the gate and you want a one-line pulse in the build log.
    Quiet,
}

impl OutputFormat {
    // P10.7
    /// TTY-aware default: `pretty` interactively, `compact` when
    /// piped (`miette` when stdout is a TTY OR `--format=pretty`).
    /// `csv` and `quiet` are explicit-opt-in only — neither is ever
    /// selected by `auto()`.
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

        // Force miette's color decision when the user explicitly
        // overrides — `auto` leaves miette to its own (correct) TTY
        // check so we don't second-guess the library on the default
        // path. `force_graphical(true)` keeps the source-snippet +
        // caret rendering even when stderr isn't a TTY (the case
        // when piping pretty diagnostics into `less -R`).
        if self.color != ColorMode::Auto {
            let want_color = self.color.enabled();
            let _ = miette::set_hook(Box::new(move |_| {
                Box::new(
                    miette::MietteHandlerOpts::new()
                        .color(want_color)
                        .force_graphical(want_color)
                        .build(),
                )
            }));
        }

        // **P23.6** — `--list-rules` short-circuits everything else and
        // dumps the rule registry. Auto-generated from
        // `lint::LINT_RULES`, so the listing never goes out of sync
        // with the live rule set.
        if self.list_rules {
            for rule in greycat_analyzer_analysis::lint::LINT_RULES {
                println!("{}\t{}", rule.name, rule.summary);
            }
            return Ok(ExitCode::SUCCESS);
        }

        // Convenience: when given a directory, look for `project.gcl`
        // at its root and use that as the entrypoint. Matches what
        // `greycat run` does and what the LSP's workspace-folder load
        // does — the CLI shouldn't be the odd one out. When omitted
        // entirely, default to `./project.gcl` in the current working
        // directory (the most common case from inside a project).
        let initial = match self.project {
            Some(p) => p,
            None => std::env::current_dir()?,
        };
        let mut project_filepath = initial.canonicalize()?;
        if project_filepath.is_dir() {
            let candidate = project_filepath.join("project.gcl");
            if candidate.is_file() {
                project_filepath = candidate;
            } else {
                eprintln!(
                    "error: no project.gcl found in {}; pass a .gcl entrypoint or a directory containing project.gcl",
                    project_filepath.display(),
                );
                return Ok(ExitCode::FAILURE);
            }
        }

        // Load the project graph properly: parse the entrypoint, walk
        // its `@library` / `@include` pragmas, and pull in only the
        // modules the entrypoint actually depends on. The previous
        // `iter_gcl(project_dir)` flat-walk picked up every `.gcl`
        // under the project directory regardless of inclusion — wrong
        // for GreyCat's project model, where the entrypoint's pragmas
        // are the source of truth.
        let total_start = Instant::now();
        let ctx_for_diags =
            FsContext::new().unwrap_or_else(|_| FsContext::with_greycat_home(PathBuf::new()));
        let mut mgr = SourceManager::with_context(Arc::new(ctx_for_diags));
        let report = mgr.load_project(&project_filepath);
        // P15.5 — `unresolved_libraries` is now surfaced as typed
        // `unresolved-library` diagnostics by `pragma_diagnostics`,
        // emitted alongside parse / semantic / lint diagnostics below.
        // Only loader-internal `errors` (file read failures, etc.)
        // remain free-form here.
        for err in &report.errors {
            eprintln!("warning: {err}");
        }
        // P15.5 — separate FsContext for pragma_diagnostics so the
        // manager keeps owning its `Arc<dyn Context>`. Both reads from
        // the same `FsContext::new()` shape.
        let pragma_ctx =
            FsContext::new().unwrap_or_else(|_| FsContext::with_greycat_home(PathBuf::new()));
        let pragma_root = project_filepath.parent().map(Path::to_path_buf);

        // One project-level analyzer pipeline over every reachable doc.
        // **P23.7** — `--no-suppressions` flips the project's
        // `bypass_suppressions` flag so every `// gcl-lint-off …` is
        // ignored and the underlying diagnostics resurface.
        let mut analysis = ProjectAnalysis::new();
        analysis.bypass_suppressions = self.no_suppressions;
        analysis.analyze_staged(&mgr);
        let total = total_start.elapsed();

        // P14.5: per-uri load-phase timings come from the load report;
        // build an index so the manager.iter() loop below can pick the
        // matching read / parse durations per file.
        #[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
        let load_by_uri: std::collections::HashMap<
            Uri,
            greycat_analyzer_core::LoadTimings,
        > = report.loaded.iter().cloned().collect();

        // Hydrate cli `Entry`s from the manager's loaded set.
        //
        // P26.6 — under `--features parallel`, hydrate every entry off
        // the rayon work-stealing pool. Each entry's work is fully
        // independent (per-doc parse_diagnostics + pragma_diagnostics +
        // per-doc translation of analyzer-emitted diagnostics into LSP
        // shape). The serial path is preserved verbatim; the parallel
        // preamble's per-doc clones (text + tree) are skipped under
        // `cfg(not(feature = "parallel"))` so the serial cost stays
        // at zero.
        let lint_libs = self.lint_libs;
        let hydrate = |uri: &Uri,
                       path: PathBuf,
                       text: &str,
                       root: greycat_analyzer_syntax::tree_sitter::Node<'_>|
         -> Entry {
            let load = load_by_uri.get(uri).copied().unwrap_or_default();
            let timings = analysis.module(uri).map(|m| m.timings).unwrap_or_default();
            let mut diagnostics = parse_diagnostics(root, text);
            if let Some(proot) = pragma_root.as_ref() {
                let desc =
                    greycat_analyzer_core::module_desc::parse_module_desc(uri.clone(), text, root);
                diagnostics.extend(greycat_analyzer_core::diagnostics::pragma_diagnostics(
                    text,
                    &desc,
                    proot,
                    &pragma_ctx,
                ));
            }
            if let Some(module) = analysis.module(uri) {
                for d in &module.analysis.diagnostics {
                    diagnostics.push(Diagnostic {
                        range: byte_range_to_lsp(text, &d.byte_range),
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
                if lint_libs || module.lib == "project" {
                    for l in &module.lints {
                        diagnostics.push(Diagnostic {
                            range: byte_range_to_lsp(text, &l.byte_range),
                            severity: Some(match l.severity {
                                LintSeverity::Error => DiagnosticSeverity::ERROR,
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
            }
            Entry {
                path,
                read: load.read,
                parse: load.parse,
                lower: timings.lower,
                resolve: timings.resolve,
                analyze: timings.analyze,
                lint: timings.lint,
                nodes: root.descendant_count(),
                diagnostics,
            }
        };

        // P27.1 — single dispatch through the analysis crate's
        // parallel shim (rayon on native, serial on wasm).
        // Pre-extract per-doc data into Send-safe form (text + tree
        // clone). RefCell<Document> is !Sync so we can't hold its
        // borrow across workers.
        let docs: Vec<(
            Uri,
            PathBuf,
            String,
            greycat_analyzer_syntax::tree_sitter::Tree,
        )> = mgr
            .iter()
            .map(|(uri, cell)| {
                let doc = cell.borrow();
                let path = uri_to_path(uri).unwrap_or_else(|| PathBuf::from(uri.as_str()));
                (uri.clone(), path, doc.text.clone(), doc.tree.clone())
            })
            .collect();
        let mut entries: Vec<Entry> =
            greycat_analyzer_analysis::parallel::par_map(docs, |(uri, path, text, tree)| {
                hydrate(&uri, path, &text, tree.root_node())
            });

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
                    let original_had_errors = greycat_analyzer_syntax::parse(&original)
                        .root_node()
                        .has_error();
                    let edit_count = non_overlap.len();
                    let mut new_text = original.clone();
                    for (r, t) in non_overlap.into_iter().rev() {
                        new_text.replace_range(r, &t);
                    }
                    // **P22.5** — re-parse before committing. If the
                    // edits would introduce parse errors that the
                    // original didn't have, REVERT the file and warn.
                    // The safety net catches any per-rule fix bug —
                    // even one we haven't found yet.
                    let new_has_errors = greycat_analyzer_syntax::parse(&new_text)
                        .root_node()
                        .has_error();
                    if new_has_errors && !original_had_errors {
                        eprintln!(
                            "[fix] reverted {}: would have introduced parse error(s) — \
                             skipped {} edit(s) on this pass",
                            entry.path.display(),
                            edit_count,
                        );
                        // Leave the file untouched, do not count edits
                        // as applied. Outer loop will exit naturally.
                        continue;
                    }
                    applied_this_pass += edit_count;
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
                let new_report = new_mgr.load_project(&project_filepath);
                #[allow(clippy::mutable_key_type)]
                let new_load_by_uri: std::collections::HashMap<
                    Uri,
                    greycat_analyzer_core::LoadTimings,
                > = new_report.loaded.iter().cloned().collect();
                let mut new_analysis = ProjectAnalysis::new();
                new_analysis.bypass_suppressions = self.no_suppressions;
                new_analysis.analyze_staged(&new_mgr);
                entries.clear();
                for (uri, cell) in new_mgr.iter() {
                    let doc = cell.borrow();
                    let path = uri_to_path(uri).unwrap_or_else(|| PathBuf::from(uri.as_str()));
                    let load = new_load_by_uri.get(uri).copied().unwrap_or_default();
                    let timings = new_analysis
                        .module(uri)
                        .map(|m| m.timings)
                        .unwrap_or_default();
                    let mut diagnostics = parse_diagnostics(doc.root_node(), &doc.text);
                    if let Some(root) = pragma_root.as_ref() {
                        let desc = greycat_analyzer_core::module_desc::parse_module_desc(
                            uri.clone(),
                            &doc.text,
                            doc.root_node(),
                        );
                        diagnostics.extend(greycat_analyzer_core::diagnostics::pragma_diagnostics(
                            &doc.text,
                            &desc,
                            root,
                            &pragma_ctx,
                        ));
                    }
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
                        if self.lint_libs || module.lib == "project" {
                            for l in &module.lints {
                                diagnostics.push(Diagnostic {
                                    range: byte_range_to_lsp(&doc.text, &l.byte_range),
                                    severity: Some(match l.severity {
                                        LintSeverity::Error => DiagnosticSeverity::ERROR,
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
                    }
                    entries.push(Entry {
                        path,
                        read: load.read,
                        parse: load.parse,
                        lower: timings.lower,
                        resolve: timings.resolve,
                        analyze: timings.analyze,
                        lint: timings.lint,
                        nodes: doc.root_node().descendant_count(),
                        diagnostics,
                    });
                }
            }
        }

        if self.fix && fixes_applied > 0 {
            println!("[fix] applied {fixes_applied} edit(s)");
        }

        // P10.7 — pretty by default when stdout is a TTY (miette:
        // snippet + caret + color), compact when piped so the parity
        // oracle and grep-style consumers keep a stable shape. `csv`
        // and `quiet` are always explicit; `auto()` never picks them.
        let render = self.format.unwrap_or_else(OutputFormat::auto);
        match render {
            OutputFormat::Csv => render_csv_timings(&mut entries)?,
            OutputFormat::Compact | OutputFormat::Pretty => {
                entries.sort_by(|a, b| a.path.cmp(&b.path));
                for e in &entries {
                    let path = e.path.display().to_string();
                    let source = std::fs::read_to_string(&e.path).unwrap_or_default();
                    for diag in &e.diagnostics {
                        match render {
                            OutputFormat::Compact => println!("{}", format_cli(&path, diag)),
                            OutputFormat::Pretty => print_pretty_diagnostic(&path, &source, diag),
                            OutputFormat::Csv | OutputFormat::Quiet => {}
                        }
                    }
                }
                print_summary(&entries, total, self.color.enabled());
            }
            OutputFormat::Quiet => print_summary(&entries, total, self.color.enabled()),
        }

        // Hints are advisory by design (return-type inference suggestions,
        // style nudges) — they must not flip CI red. Only errors and
        // warnings count toward the failure exit code.
        let blocking = count_blocking(&entries);
        Ok(if blocking == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        })
    }
}

/// Sum of error + warning diagnostics across every entry. Hints
/// (and `INFORMATION`-severity entries) are advisory and never
/// contribute to the failure exit code.
fn count_blocking(entries: &[Entry]) -> usize {
    let mut n = 0usize;
    for e in entries {
        for d in &e.diagnostics {
            match d.severity {
                Some(DiagnosticSeverity::ERROR) | Some(DiagnosticSeverity::WARNING) => n += 1,
                // Diagnostics with an unknown / missing severity are
                // counted as blocking — fail-closed so a misclassified
                // diag never silently passes CI.
                None => n += 1,
                _ => {}
            }
        }
    }
    n
}

/// Per-file timing rows, sorted by total descending so the heaviest
/// file lands at the top. P14.5 — per-phase microsecond columns
/// (`read` = file I/O, `parse` = tree-sitter, `lower` = HIR walker,
/// `resolve` / `analyze` / `lint` = analyzer pipeline). `total_us` is
/// the sum of all phase columns. No human summary follows — the
/// consumer is `awk` / a spreadsheet.
fn render_csv_timings(entries: &mut [Entry]) -> std::io::Result<()> {
    let mut w = std::io::stdout();
    writeln!(
        w,
        "total_us,read_us,parse_us,lower_us,resolve_us,analyze_us,lint_us,nb_nodes,nb_diagnostics,filepath"
    )?;
    entries.sort_by_key(|e| std::cmp::Reverse(e.total()));
    for e in entries {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{}",
            e.total().as_micros(),
            e.read.as_micros(),
            e.parse.as_micros(),
            e.lower.as_micros(),
            e.resolve.as_micros(),
            e.analyze.as_micros(),
            e.lint.as_micros(),
            e.nodes,
            e.diagnostics.len(),
            e.path.display(),
        )?;
    }
    Ok(())
}

struct Entry {
    path: PathBuf,
    /// File I/O (`Context::read`).
    read: Duration,
    /// Tree-sitter parse (`syntax::parse`).
    parse: Duration,
    /// CST → HIR walker (`lower_module`).
    lower: Duration,
    /// Resolver (`resolve_with_index`).
    resolve: Duration,
    /// Analyzer (`analyze_with_index`).
    analyze: Duration,
    /// Lint rules (`run_lints`).
    lint: Duration,
    nodes: usize,
    diagnostics: Vec<Diagnostic>,
}

impl Entry {
    fn total(&self) -> Duration {
        self.read + self.parse + self.lower + self.resolve + self.analyze + self.lint
    }
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

/// Trailing one-liner that aggregates diagnostics by severity. Each
/// non-zero category is colored to its conventional severity hue (red
/// errors, yellow warnings, cyan hints); the error count is always
/// shown so the success badge is visible — green when zero, red
/// otherwise. The module count and total wall-clock are dimmed to keep
/// the colored counts as the focal point.
///
/// `color` toggles all ANSI escapes off so `--color=never` /
/// non-TTY-without-override stays grep-friendly.
fn print_summary(entries: &[Entry], total: Duration, color: bool) {
    const RED: &str = "\x1b[31m";
    const GREEN: &str = "\x1b[32m";
    const YELLOW: &str = "\x1b[33m";
    const CYAN: &str = "\x1b[36m";
    const RESET: &str = "\x1b[0m";

    let paint = |code: &str, s: String| -> String {
        if color {
            format!("{code}{s}{RESET}")
        } else {
            s
        }
    };
    let pluralize = |n: usize, stem: &str| -> String {
        if n == 1 {
            format!("1 {stem}")
        } else {
            format!("{n} {stem}s")
        }
    };

    let mut errors = 0usize;
    let mut warnings = 0usize;
    let mut hints = 0usize;
    for e in entries {
        for d in &e.diagnostics {
            match d.severity {
                Some(DiagnosticSeverity::ERROR) => errors += 1,
                Some(DiagnosticSeverity::WARNING) => warnings += 1,
                Some(DiagnosticSeverity::HINT) | Some(DiagnosticSeverity::INFORMATION) => {
                    hints += 1
                }
                // Unknown severity ⇒ count as an error so the exit
                // status stays fail-closed.
                _ => errors += 1,
            }
        }
    }

    let err_color = if errors == 0 { GREEN } else { RED };
    let mut parts: Vec<String> = vec![paint(err_color, pluralize(errors, "error"))];
    if warnings > 0 {
        parts.push(paint(YELLOW, pluralize(warnings, "warning")));
    }
    if hints > 0 {
        parts.push(paint(CYAN, pluralize(hints, "hint")));
    }
    println!(
        "{summary} across {modules} in {dur}",
        summary = parts.join(", "),
        modules = paint(CYAN, pluralize(entries.len(), "module")),
        dur = paint(GREEN, format!("{total:?}")),
    );
}

// P10.7
/// Pretty-rendered diagnostic — pipes through `miette` so the
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

// P22.7
/// Diagnostic → byte-range + replacement text. Routes through the
/// shared [`greycat_analyzer_analysis::quickfix`] module.
/// Returns `None` for diagnostics that don't have an automatic fix or
/// whose preconditions don't hold.
///
/// Each rule may produce multiple `TextEdit`s in principle; the
/// current cli `--fix` driver expects a single range+text pair, so we
/// flatten "one edit" rules and drop multi-edit cases (none today).
fn diag_to_edit(text: &str, diag: &Diagnostic) -> Option<(std::ops::Range<usize>, String)> {
    let code = match diag.code.as_ref()? {
        NumberOrString::String(s) => s.as_str(),
        _ => return None,
    };
    let start = lsp_to_byte(text, diag.range.start);
    let end = lsp_to_byte(text, diag.range.end);
    let edits = greycat_analyzer_analysis::quickfix::edit_for_diagnostic(
        text,
        code,
        &(start..end),
        &diag.message,
    );
    if edits.len() != 1 {
        return None;
    }
    let e = edits.into_iter().next()?;
    Some((e.byte_range, e.new_text))
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
