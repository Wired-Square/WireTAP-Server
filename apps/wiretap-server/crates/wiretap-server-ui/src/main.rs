//! The capture daemon's web UI. It reaches `wiretap-server` only through the
//! daemon's control socket, so the daemon stays headless.
//!
//! One binary, two backends, picked at start as the chassis picks: with
//! `WIRETAP_SERVER_UI_APP_SOCKET` and `_CONSOLE_UID` set it serves behind an
//! ApplianceOS console (`wslib-appliance-server`), and without them its own
//! HTTPS and accounts (`wslib-appliance-https`).

mod daemon;
mod routes;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use wslib_appliance_config::{ApplianceArgs, Resolver, ServingArgs};
use wslib_appliance_server::{AppSocket, Logs};
use wslib_appliance_store::Store;
use wslib_appliance_types::Branding;
use wslib_appliance_types::branding::ThemeTokens;
use wslib_telemetry::Telemetry;

use crate::daemon::Daemon;

const NAME: &str = "wiretap-server-ui";
const ENV_PREFIX: &str = "WIRETAP_SERVER_UI";
const VERSION: &str = wslib_build_id::build_version!();
const KEEP_AUDIT: Duration = Duration::from_secs(90 * 24 * 60 * 60);
const LOGO: &[u8] =
    include_bytes!("../../../../../crates/wiretap-backend/admin-ui/public/logo.svg");

#[derive(Debug, Parser)]
#[command(name = NAME, version = VERSION)]
#[command(about = "The capture daemon's web UI, on its control socket.", long_about = None)]
struct Args {
    #[command(flatten)]
    appliance: ApplianceArgs,

    #[command(flatten)]
    serving: ServingArgs,

    /// The capture daemon's control socket.
    #[arg(
        long,
        value_name = "PATH",
        env = "WIRETAP_SERVER_UI_DAEMON_SOCKET",
        default_value = daemon::DEFAULT_SOCKET
    )]
    daemon_socket: PathBuf,
}

fn main() -> ExitCode {
    let args = Args::parse();
    install_crypto_provider();
    let logs = wslib_appliance_server::logs(&["wiretap_server_ui"]);
    logs.feed(Telemetry::new("info").journald()).init();
    wslib_appliance_server::daemon(NAME)
        .run(|stop| run(args, logs, stop))
        .map_or_else(ExitCode::from, |()| ExitCode::SUCCESS)
}

async fn run(
    args: Args,
    logs: Logs,
    stop: impl Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let bootstrap = Resolver::new(NAME, ENV_PREFIX)
        .serving(&args.serving)
        .resolve(&args.appliance)?;
    let daemon = Daemon::new(args.daemon_socket);
    tracing::info!(
        version = VERSION,
        config = %bootstrap.config_path.display(),
        db = %bootstrap.db.display(),
        daemon = %daemon.socket().display(),
        "starting"
    );
    let store = Store::open(&bootstrap.db, None).await?;
    let sweep = |stop| {
        wslib_appliance_server::spawn_sweeper(
            store.clone(),
            (),
            Duration::from_secs(60 * 60),
            KEEP_AUDIT,
            stop,
            |_store, _at| async {},
        )
    };

    let (sweeping, served) = match &bootstrap.proxied {
        Some(console) => {
            let server =
                wslib_appliance_server::Server::new(store.clone(), branding(), VERSION, logs);
            let sweeping = sweep(server.shutdown());
            let mut socket = AppSocket::new(console.app_socket.clone(), console.console_uid);
            if let Some(hooks) = &console.hooks_socket {
                socket = socket.hooks(hooks.clone());
            }
            let served = server
                .logo(LOGO, "image/svg+xml")
                .ui(bootstrap.ui.clone())
                .api(routes::routes(daemon))
                .serve(&bootstrap, socket, stop)
                .await;
            (sweeping, served)
        }
        None => {
            if store.user_count().await? == 0 {
                let how = if bootstrap.browser_onboarding {
                    "the first person to reach it in a browser makes one, or create it over the control socket"
                } else {
                    "create the first over the control socket"
                };
                tracing::warn!(socket = %bootstrap.socket.display(), "no accounts yet: {how}");
            }
            let server =
                wslib_appliance_https::Server::new(store.clone(), branding(), VERSION, logs);
            let sweeping = sweep(server.shutdown());
            let served = server
                .logo(LOGO, "image/svg+xml")
                .ui(bootstrap.ui.clone())
                .api(routes::routes(daemon))
                .users_routes()
                .serve(&bootstrap, stop)
                .await;
            (sweeping, served)
        }
    };

    sweeping.await?;
    Ok(served?)
}

fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn branding() -> Branding {
    let mut branding = Branding::unbranded();
    branding.app_name = "WireTAP Capture".to_string();
    branding.manufacturer = "Wired Square".to_string();
    // The same accent as frontend/src/theme.css, so the page does not repaint
    // when the branding request lands.
    branding.tokens = ThemeTokens::new(BTreeMap::from([(
        "--app-accent".to_string(),
        "#c2410c".to_string(),
    )]))
    .expect("the compiled-in theme tokens are legal");
    branding
}
