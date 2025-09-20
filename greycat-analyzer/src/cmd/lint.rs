use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::utils::AnyError;
use greycat_analyzer_core::{
    bumpalo::Bump,
    cst::{CstNode, CstStats, ModuleInfo, SourceModule},
    lsp_types::Diagnostic,
};

#[derive(clap::Parser)]
#[clap(about = "Lints a project")]
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

        let start = Instant::now();
        let arena = Bump::with_capacity(std::mem::size_of::<CstNode>() * 2048);
        let mut mgr = SourceManager::new();
        mgr.parse_module(project_dir, &project_filepath, &arena)?;
        let took = start.elapsed();

        if self.csv {
            mgr.display_timings_csv(&mut std::io::stdout())?;
        } else {
            mgr.display_timings();
            println!("Total: {took:?}");

            if !mgr.errors.is_empty() {
                for (filepath, errors) in mgr.errors {
                    println!("{}: {} errors", filepath.display(), errors.len());
                    for error in errors {
                        println!(
                            "{} [{}:{}]",
                            error.message, error.range.start.line, error.range.start.character
                        );
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Default)]
struct SourceManager<'arena> {
    sources: HashMap<PathBuf, SourceModule<'arena>>,
    timings: HashMap<PathBuf, Duration>,
    errors: HashMap<PathBuf, Vec<Diagnostic>>,
}

impl<'arena> SourceManager<'arena> {
    pub fn new() -> Self {
        Self::default()
    }

    fn parse_module(
        &mut self,
        project_dir: &Path,
        filepath: &Path,
        arena: &'arena Bump,
    ) -> Result<(), AnyError> {
        let start = Instant::now();

        let module = greycat_analyzer_core::cst::parse_file(filepath, arena)?;
        let took = start.elapsed();
        let info = ModuleInfo::from(&module);
        for lib in &info.libraries {
            self.resolve_library(project_dir, lib.name.image, arena)?;
        }
        let mut files = Vec::new();
        for include in &info.includes {
            let include_dir = match project_dir.join(include.image).canonicalize() {
                Ok(path) => path,
                Err(err) => {
                    self.errors
                        .entry(filepath.to_path_buf())
                        .and_modify(|entry| {
                            entry.push(Diagnostic::new_simple(
                                include.span.to_range(),
                                err.to_string(),
                            ));
                        })
                        .or_insert_with(|| {
                            vec![Diagnostic::new_simple(
                                include.span.to_range(),
                                err.to_string(),
                            )]
                        });
                    continue;
                }
            };
            // println!("@include(\"{}\")", include_dir.display());
            files.clear();
            find_gcl_files(&include_dir, &mut files);
            for file in &files {
                self.parse_module(project_dir, file, arena)?;
            }
        }
        self.sources.insert(filepath.to_owned(), module);
        self.timings.insert(filepath.to_owned(), took);
        Ok(())
    }

    fn resolve_library(
        &mut self,
        project_dir: &Path,
        name: &str,
        arena: &'arena Bump,
    ) -> Result<(), AnyError> {
        let lib_dir = project_dir.join("lib").join(name);
        let mut files = Vec::new();
        find_gcl_files(&lib_dir, &mut files);
        for file in files {
            self.parse_module(project_dir, &file, arena)?
        }
        Ok(())
    }

    pub fn display_timings(&self) {
        // Collect all paths and their timings
        let mut timing_pairs: Vec<(&PathBuf, &Duration)> = self.timings.iter().collect();

        // Sort by duration (ascending - smallest first, largest last)
        timing_pairs.sort_by_key(|(_, duration)| *duration);

        // Display each file with its timing
        for ((path, duration), source) in timing_pairs.iter().zip(self.sources.values()) {
            let stats = CstStats::from(&source.module);
            println!("{:>8.2?} {:>4} {}", duration, stats.nodes, path.display());
        }
    }

    pub fn display_timings_csv(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        // Write CSV headers
        writeln!(w, "duration_us,nb_nodes,filepath")?;

        // Collect all paths and their timings
        let mut timing_pairs: Vec<(&PathBuf, &Duration)> = self.timings.iter().collect();

        // Sort by duration (ascending - smallest first, largest last)
        timing_pairs.sort_by_key(|(_, duration)| *duration);

        // Display each file with its timing
        for ((path, duration), source) in timing_pairs.iter().zip(self.sources.values()) {
            let stats = CstStats::from(&source.module);
            writeln!(
                w,
                "{},{},{}",
                duration.as_micros(),
                stats.nodes,
                path.display(),
            )?;
        }

        Ok(())
    }
}

fn find_gcl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    // read entries in the directory
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();

            // if it's a directory, recurse
            if path.is_dir() {
                find_gcl_files(&path, out);
            }

            // if it's a file ending with .gcl, collect it
            if path.is_file()
                && let Some(ext) = path.extension()
                && ext == "gcl"
            {
                out.push(path.canonicalize().unwrap());
            }
        }
    }
}
