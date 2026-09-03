//! Entry point.
//!
//! `--check-config` is fully working; the capture pipeline is still being
//! ported, so anything else exits non-zero rather than idling and looking to a
//! supervisor like a daemon that started.

use std::process::ExitCode;

use clap::Parser;
use wiretap_server::{
    cli::Cli,
    settings::{self, Secrets, Settings},
};

fn main() -> ExitCode {
    let cli = Cli::parse();

    let file = match cli.config.as_deref().map(settings::load) {
        Some(Ok(f)) => Some(f),
        Some(Err(e)) => {
            eprintln!("wiretap-server: {e}");
            return ExitCode::FAILURE;
        }
        None => None,
    };

    let resolved = match Settings::resolve(&cli, file.as_ref(), &Secrets::from_env()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wiretap-server: {e}");
            return ExitCode::FAILURE;
        }
    };

    for w in &resolved.warnings {
        eprintln!("wiretap-server: warning: {w}");
    }

    if cli.check_config {
        print!("{}", resolved.settings);
        return ExitCode::SUCCESS;
    }

    eprintln!(
        "wiretap-server: {} — the capture pipeline is still being ported; \
         only --check-config works today",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::FAILURE
}
