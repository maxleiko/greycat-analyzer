//! C FFI bridge: exposes the `lint` / `fmt` / `server` CLI subcommands as
//! `extern "C"` entry points for the `greycat` runtime to `dlopen` and call
//! with the user's `argc` / `argv`. Built as `libgreycat_lang.so`.

use std::ffi::{CStr, OsString};
use std::os::raw::{c_char, c_int};

/// `greycat lint <args>` -> the `lint` subcommand.
#[unsafe(no_mangle)]
pub extern "C" fn greycat_lang_lint(argc: c_int, argv: *const *const c_char) -> c_int {
    ffi_dispatch("lint", argc, argv)
}

/// `greycat fmt <args>` -> the `fmt` subcommand.
#[unsafe(no_mangle)]
pub extern "C" fn greycat_lang_fmt(argc: c_int, argv: *const *const c_char) -> c_int {
    ffi_dispatch("fmt", argc, argv)
}

/// `greycat lsp <args>` -> the `server` subcommand (LSP over stdio).
#[unsafe(no_mangle)]
pub extern "C" fn greycat_lang_lsp(argc: c_int, argv: *const *const c_char) -> c_int {
    ffi_dispatch("server", argc, argv)
}

/// Rebuild a clap argv (`["greycat", subcommand, ...caller args]`), run the
/// dispatch under `catch_unwind` (a panic crossing the C boundary is UB), and
/// return the process exit code (101 on panic, mirroring Rust's convention).
fn ffi_dispatch(subcommand: &str, argc: c_int, argv: *const *const c_char) -> c_int {
    // SAFETY: caller passes a valid argc/argv pair per the C calling convention.
    let args = unsafe { collect_args(subcommand, argc, argv) };
    match std::panic::catch_unwind(move || greycat_analyzer::run_from_args(args)) {
        Ok(code) => code as c_int,
        Err(_) => {
            eprintln!("greycat-lang: fatal error while running `{subcommand}`");
            101
        }
    }
}

/// Build `[prog_name, subcommand, ...argv]` for clap. The caller passes only
/// the tail args (everything after `greycat <subcommand>`); the synthetic
/// `"greycat"` program name makes `--help` read naturally.
///
/// # Safety
/// `argv` must point to `argc` valid, NUL-terminated C strings (or be null
/// when `argc == 0`).
unsafe fn collect_args(subcommand: &str, argc: c_int, argv: *const *const c_char) -> Vec<OsString> {
    let count = argc.max(0) as usize;
    let mut args = Vec::with_capacity(count + 2);
    args.push(OsString::from("greycat"));
    args.push(OsString::from(subcommand));
    if !argv.is_null() {
        for i in 0..count {
            let ptr = unsafe { *argv.add(i) };
            if ptr.is_null() {
                continue;
            }
            let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
            args.push(os_string_from_bytes(bytes));
        }
    }
    args
}

#[cfg(unix)]
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(bytes).to_os_string()
}

#[cfg(not(unix))]
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}
