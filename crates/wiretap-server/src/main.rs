//! Entry point.
//!
//! Configuration is resolved and reported before a runtime exists, so
//! `--check-config` costs nothing and a bad config fails where an operator can
//! read why. Logs go to stderr, as the Python's did, leaving stdout to the
//! config dump and to `--echo-console`.

use std::process::ExitCode;

use clap::Parser;
use tracing::{error, warn};
use tracing_subscriber::filter::LevelFilter;
use wiretap_server::{
    cli::Cli,
    settings::{self, Env, LogLevel, Settings},
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

    let resolved = match Settings::resolve(&cli, file.as_ref(), &Env::from_env()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wiretap-server: {e}");
            return ExitCode::FAILURE;
        }
    };

    init_logging(resolved.settings.log_level);
    for w in &resolved.warnings {
        warn!("{w}");
    }

    if cli.check_config {
        print!("{}", resolved.settings);
        return ExitCode::SUCCESS;
    }

    serve(&resolved.settings)
}

fn init_logging(level: LogLevel) {
    tracing_subscriber::fmt()
        .with_max_level(match level {
            LogLevel::Debug => LevelFilter::DEBUG,
            LogLevel::Info => LevelFilter::INFO,
            LogLevel::Warning => LevelFilter::WARN,
            LogLevel::Error => LevelFilter::ERROR,
        })
        .with_writer(std::io::stderr)
        .init();
}

fn serve(settings: &Settings) -> ExitCode {
    // Built here rather than with `#[tokio::main]` so that everything above —
    // parsing, the config merge, `--check-config` — runs without one.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            error!("cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(wiretap_server::pipeline::run(settings)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}
