use std::io::IsTerminal;

pub type AnyError = Box<dyn std::error::Error + Sync + Send>;

/// Standard `--color=auto|always|never` knob shared across CLI
/// subcommands. `auto` follows the GNU / `git` convention — color
/// when stdout is a TTY *and* `NO_COLOR` is unset; `always` forces
/// it on (handy when piping through `less -R`), `never` forces it
/// off.
///
/// Centralized here so subcommands don't drift on TTY-check
/// semantics. Subcommand `--help` strings own their own copy of the
/// description because clap doesn't inherit doc-comments across
/// crates the way `#[derive(Args)]` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ColorMode {
    /// color when stdout is a TTY and `NO_COLOR` is unset (default)
    #[default]
    Auto,
    /// always emit ANSI color escapes
    Always,
    /// never color
    Never,
}

impl ColorMode {
    /// Resolve to a concrete on/off decision for the current run.
    pub fn enabled(self) -> bool {
        match self {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => {
                std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
            }
        }
    }
}
