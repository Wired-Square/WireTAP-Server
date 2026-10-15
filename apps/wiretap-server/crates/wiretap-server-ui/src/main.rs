//! The capture daemon's web UI. It reaches `wiretap-server` only through the
//! daemon's control socket, so the daemon stays headless.
//!
//! One binary, two backends, picked at start as the chassis picks: with
//! `WIRETAP_SERVER_UI_APP_SOCKET` and `_CONSOLE_UID` set it serves behind an
//! ApplianceOS console (`appliance-server`), and without them its own HTTPS
//! and accounts (`appliance-https`).

mod daemon;
mod routes;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use appliance_config::{ApplianceArgs, Resolver, ServingArgs};
use appliance_server::{AppSocket, Logs};
use appliance_store::Store;
use appliance_types::Branding;
use appliance_types::branding::ThemeTokens;
use clap::Parser;

use crate::daemon::Daemon;

const NAME: &str = "wiretap-server-ui";
const ENV_PREFIX: &str = "WIRETAP_SERVER_UI";
const VERSION: &str = appliance_build_id::build_version!();
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

/// `ExitCode` rather than `Result`, so a startup failure reaches the journal
/// through the chassis's `Display` and not as `Debug`.
#[tokio::main]
async fn main() -> ExitCode {
    let logs = Logs::install(&["wiretap_server_ui"]);
    match run(logs).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(logs: Logs) -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
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
        appliance_server::spawn_sweeper(
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
            let server = appliance_server::Server::new(store.clone(), branding(), VERSION, logs);
            let sweeping = sweep(server.shutdown());
            let mut socket = AppSocket::new(console.app_socket.clone(), console.console_uid);
            if let Some(hooks) = &console.hooks_socket {
                socket = socket.hooks(hooks.clone());
            }
            let served = server
                .logo(LOGO, "image/svg+xml")
                .ui(bootstrap.ui.clone())
                .api(routes::routes(daemon))
                .serve(&bootstrap, socket)
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
            let server = appliance_https::Server::new(store.clone(), branding(), VERSION, logs);
            let sweeping = sweep(server.shutdown());
            let served = server
                .logo(LOGO, "image/svg+xml")
                .ui(bootstrap.ui.clone())
                .api(routes::routes(daemon))
                .users_routes()
                .serve(&bootstrap)
                .await;
            (sweeping, served)
        }
    };

    sweeping.await?;
    Ok(served?)
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
