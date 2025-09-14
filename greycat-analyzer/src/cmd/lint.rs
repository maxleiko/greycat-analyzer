use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::utils::AnyError;
use greycat_analyzer_core::{
    cst::{ModuleInfo, SourceModule},
    lsp_types::Diagnostic,
};

#[derive(clap::Parser)]
#[clap(about = "Lints a project")]
pub struct Lint {
    #[clap(help = "Path to a project.gcl")]
    project: PathBuf,
}

impl Lint {
    pub fn run(self) -> Result<(), AnyError> {
        env_logger::init();

        let project_filepath = self.project.canonicalize()?;
        let project_dir = project_filepath
            .parent()
            .expect("unable to resolve project's parent directory");

        let start = Instant::now();
        let mut mgr = SourceManager::new();
        parse_module(project_dir, &project_filepath, &mut mgr)?;
        let took = start.elapsed();

        mgr.display_timings();
        println!("Total: {took:?}");

        Ok(())
    }
}

#[derive(Default, Debug)]
struct SourceManager {
    sources: HashMap<PathBuf, SourceModule>,
    timings: HashMap<PathBuf, Duration>,
    errors: HashMap<PathBuf, Vec<Diagnostic>>,
}

impl SourceManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn display_timings(&self) {
        // Collect all paths and their timings
        let mut timing_pairs: Vec<(&PathBuf, &Duration)> = self.timings.iter().collect();

        // Sort by duration (ascending - smallest first, largest last)
        timing_pairs.sort_by_key(|(_, duration)| *duration);

        // Display each file with its timing
        for (path, duration) in timing_pairs {
            println!("{:>8.2?} {}", duration, path.display());
        }
    }
}

fn parse_module(
    project_dir: &Path,
    filepath: &Path,
    mgr: &mut SourceManager,
) -> Result<(), AnyError> {
    let start = Instant::now();

    let module = greycat_analyzer_core::cst::parse_file(filepath)?;
    let took = start.elapsed();
    let info = ModuleInfo::from(&module);
    for lib in &info.libraries {
        // println!(
        //     "@library(\"{}\", {:?})",
        //     lib.name.image,
        //     lib.version.map(|s| s.image)
        // );
        resolve_library(lib.name.image, project_dir, mgr)?;
    }
    let mut files = Vec::new();
    for include in &info.includes {
        let include_dir = match project_dir.join(include.image).canonicalize() {
            Ok(path) => path,
            Err(err) => {
                mgr.errors
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
            parse_module(project_dir, file, mgr)?;
        }
    }
    mgr.sources.insert(filepath.to_owned(), module);
    mgr.timings.insert(filepath.to_owned(), took);
    Ok(())
}

fn resolve_library(
    name: &str,
    project_dir: &Path,
    mgr: &mut SourceManager,
) -> Result<(), AnyError> {
    let lib_dir = project_dir.join("lib").join(name);
    let mut files = Vec::new();
    find_gcl_files(&lib_dir, &mut files);
    for file in files {
        parse_module(project_dir, &file, mgr)?
    }
    Ok(())
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
