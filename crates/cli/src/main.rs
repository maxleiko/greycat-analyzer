use std::process::ExitCode;

fn main() -> ExitCode {
    // Restore default SIGPIPE handler so piping into a pager (`... | less`)
    // and quitting early exits the process cleanly with status 141 instead
    // of panicking inside println!. Rust's runtime ignores SIGPIPE by
    // default, which surfaces every closed-pipe write as an io::Error that
    // print!/println! turn into "failed printing to stdout: Broken pipe".
    #[cfg(unix)]
    // SAFETY: main has not yet spawned threads; resetting a signal disposition
    // here is race-free.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    ExitCode::from(greycat_analyzer::run_from_args(std::env::args_os()) as u8)
}
