use anyhow::Result;
use clap::Parser;
use greycat_analyzer_ls::Backend;
use tower_lsp::{LspService, Server};

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
    pub fn run(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .thread_name("greycat-analyzer-worker")
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let stdin = tokio::io::stdin();
                let stdout = tokio::io::stdout();
                let (service, socket) = LspService::new(Backend::new);
                Server::new(stdin, stdout, socket).serve(service).await;
            });
        Ok(())
    }
}
