//! Entry point.
//!
//! Configuration is resolved and reported before a runtime exists, so
//! `--check-config` costs nothing and a bad config fails where an operator can
//! read why. Logs go to stderr, as the Python's did, leaving stdout to the
//! config dump and to `--echo-console`.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Duration;

use tracing::{error, info, warn};
use wiretap_server::{
    catalogues::Catalogues,
    cli::Cli,
    control::{self, Control},
    pipeline::Stopped,
    settings::{self, Env, LogLevel, Settings},
    ui,
};
use wslib_telemetry::Telemetry;

/// Inside the unit's TimeoutStopSec of 30 s, for the archives' flush.
const STOP_GRACE: Duration = Duration::from_secs(25);

fn main() -> ExitCode {
    // Executed again through /proc/self/exe, the process would be called `exe`
    // by ps, pgrep and pkill.
    #[cfg(target_os = "linux")]
    // SAFETY: a nul-terminated name of under 16 bytes.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c"wiretap-server".as_ptr());
    }
    let cli = Cli::parse_args(std::env::args_os());
    let env = Env::from_env();

    let file = match cli.config.as_deref().map(settings::load).transpose() {
        Ok(file) => file.flatten(),
        Err(e) => {
            eprintln!("wiretap-server: {e}");
            return ExitCode::FAILURE;
        }
    };

    let ui_path = ui::path(&cli, &env);
    let (resolved, ui) = match ui::resolve_at_start(&cli, file.as_ref(), ui_path.as_deref(), &env) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wiretap-server: {e}");
            return ExitCode::FAILURE;
        }
    };

    init_logging(resolved.settings.log_level);
    // The journal's first line, before any complaint about the configuration,
    // so a report about a long-running daemon can name the build it is about
    // without stopping it to ask.
    info!("wiretap-server {}", wiretap_server::VERSION);
    if let (Some(path), None) = (&cli.config, &file) {
        info!("no config file at {path}; running on the defaults and the UI's settings");
    }
    for w in &resolved.warnings {
        warn!("{w}");
    }

    let listing = ui::listing(&cli, file.as_ref(), &ui, &env, &resolved.settings);
    if cli.check_config {
        let settings = &resolved.settings;
        print!("{}", settings.report(&Catalogues::open(settings)));
        println!("\nsettings a UI may change, and where each comes from:");
        for s in &listing {
            print!("{}", s.row(cli.config.as_deref()));
        }
        return ExitCode::SUCCESS;
    }

    let control = Control::socket(&cli, &env).map(|socket| Control {
        socket,
        cli: cli.clone(),
        ui_path,
        settings: listing,
        warnings: resolved.warnings.iter().map(ToString::to_string).collect(),
    });
    serve(resolved.settings, control)
}

fn init_logging(level: LogLevel) {
    Telemetry::new(level.as_str()).stderr().init();
}

fn serve(settings: Settings, control: Option<Control>) -> ExitCode {
    let ran =
        wslib_service::Daemon::new("wiretap-server")
            .grace(STOP_GRACE)
            .run(move |stop| async move {
                wiretap_server::pipeline::run(&settings, control, stop).await
            });
    match ran {
        Ok(Stopped::Signal) => ExitCode::SUCCESS,
        Ok(Stopped::ToApply) => reexec(),
        Err(failure) => failure.into(),
    }
}

/// Start this binary again on the same PID, with the same arguments and
/// environment, so the UI's settings apply with no supervisor's restart delay.
/// Exits 75 for the supervisor instead if that fails.
fn reexec() -> ExitCode {
    let exe = if cfg!(target_os = "linux") {
        Ok(PathBuf::from("/proc/self/exe"))
    } else {
        std::env::current_exe()
    };
    let mut args = std::env::args_os();
    let err = match exe {
        Ok(exe) => Command::new(exe)
            .arg0(args.next().unwrap_or_default())
            .args(args)
            .exec(),
        Err(e) => e,
    };
    error!("cannot re-execute to apply the UI's settings: {err}; exiting for the supervisor");
    ExitCode::from(control::EXIT_TO_APPLY)
}
