use std::path::PathBuf;
use std::process::ExitCode;

use crate::utils::AnyError;

#[derive(clap::Parser)]
#[clap(about = "Format a .gcl file (or check formatting with --check)")]
pub struct Fmt {
    #[clap(help = "Path to a .gcl file")]
    file: PathBuf,
    #[clap(long, help = "Don't write the file; exit non-zero if it would change")]
    check: bool,
}

impl Fmt {
    pub fn run(self) -> Result<ExitCode, AnyError> {
        env_logger::init();
        let source = std::fs::read_to_string(&self.file)?;
        let formatted = greycat_analyzer_fmt::format(&source);
        if self.check {
            if formatted != source {
                eprintln!("{}: would reformat", self.file.display());
                return Ok(ExitCode::FAILURE);
            }
            return Ok(ExitCode::SUCCESS);
        }
        if formatted != source {
            std::fs::write(&self.file, &formatted)?;
        }
        Ok(ExitCode::SUCCESS)
    }
}
