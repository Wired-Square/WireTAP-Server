//! Entry point. See the library crate for everything of substance.
//!
//! Placeholder until the CLI lands: it exits non-zero so a supervisor cannot
//! mistake it for a daemon that started and is running.

fn main() -> std::process::ExitCode {
    eprintln!(
        "wiretap-server {} — not yet runnable; the capture pipeline is still being ported",
        env!("CARGO_PKG_VERSION")
    );
    std::process::ExitCode::FAILURE
}
