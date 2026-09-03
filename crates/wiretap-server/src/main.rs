//! Entry point.
//!
//! `--check-config` is fully working; the capture pipeline is still being
//! ported, so anything else exits non-zero rather than idling and looking to a
//! supervisor like a daemon that started.

use std::process::ExitCode;

use clap::Parser;
use wiretap_model::FileConfig;
use wiretap_server::{
    cli::Cli,
    settings::{Settings, SettingsError},
};

fn main() -> ExitCode {
    let cli = Cli::parse();

    let file = match cli.config.as_deref().map(load) {
        Some(Ok(f)) => Some(f),
        Some(Err(e)) => {
            eprintln!("wiretap-server: {e}");
            return ExitCode::FAILURE;
        }
        None => None,
    };

    let resolved = match Settings::resolve(&cli, file.as_ref(), |k| std::env::var(k).ok()) {
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
        print!("{}", resolved.settings.describe());
        return ExitCode::SUCCESS;
    }

    eprintln!(
        "wiretap-server: {} — the capture pipeline is still being ported; \
         only --check-config works today",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::FAILURE
}

fn load(path: &str) -> Result<FileConfig, SettingsError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| SettingsError::Config(format!("cannot read {path}: {e}")))?;
    FileConfig::parse(&text).map_err(SettingsError::Config)
}
