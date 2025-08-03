use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Instant,
};

use crate::utils::{AnyError, for_each_valid_entry};
use greycat_analyzer_core::{
    Token, TokenKind,
    cst::{ModuleInfo, Node, SourceModule},
    parse, tokenize,
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

        let project_dir = self
            .project
            .parent()
            .expect("unable to resolve project's parent directory");

        let mut mgr = SourceManager::new();
        parse_module(project_dir, &self.project, &mut mgr)?;

        Ok(())
    }
}

struct SourceManager {
    sources: HashMap<PathBuf, SourceModule>,
}

impl SourceManager {
    pub fn new() -> Self {
        Self {
            sources: Default::default(),
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
    println!("{took:>8.2?} {}", filepath.display());
    for lib in &info.libraries {
        println!(
            "@library(\"{}\", {:?})",
            lib.name.image,
            lib.version.map(|s| s.image)
        );
        resolve_library(lib.name.image, project_dir, mgr);
    }
    let mut files = Vec::new();
    for include in &info.includes {
        let include_dir = project_dir.join(include.image).canonicalize().unwrap();
        println!("@include(\"{}\")", include_dir.display());
        files.clear();
        find_gcl_files(&include_dir, &mut files);
        for file in &files {
            parse_module(project_dir, file, mgr)?;
        }
    }
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
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    if ext == "gcl" {
                        out.push(path.canonicalize().unwrap());
                    }
                }
            }
        }
    }
}
