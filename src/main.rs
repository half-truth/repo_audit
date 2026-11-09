//! Thin binary wrapper: all logic lives in the library so `cargo test` and the
//! CLI exercise the same code paths.

use std::process::ExitCode;

fn main() -> ExitCode {
    let argv = repo_audit::cli::normalize_argv(std::env::args().skip(1));
    let code = match std::panic::catch_unwind(|| repo_audit::cli::run(argv)) {
        Ok(c) => c,
        // A panic here is a bug, not user input; say so instead of dumping a
        // backtrace the way an unhandled Rust panic would.
        Err(_) => {
            eprintln!(
                "Internal error: analysis panicked (this is a defect; please report it with the repository path)"
            );
            3
        }
    };
    ExitCode::from(code as u8)
}
