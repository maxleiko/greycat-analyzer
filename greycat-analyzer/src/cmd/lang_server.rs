use clap::Parser;
use greycat_analyzer_ls::start_server;

use crate::utils::AnyError;

#[derive(Parser)]
#[clap(about = "Starts a language server")]
pub struct LangServer {
    #[clap(
        long,
        help = "Whether or not you specify --stdio it is the same, we only support this mode"
    )]
    stdio: bool,
}

impl LangServer {
    pub fn run(self) -> Result<(), AnyError> {
        start_server()?;
        Ok(())
    }
}
