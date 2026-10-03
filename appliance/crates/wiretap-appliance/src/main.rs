//! The WireTAP appliance's web daemon: the chassis's screens, served beside
//! `wiretap-server`, which stays the headless capture daemon it is.

mod about;
mod backup;

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::time::Duration;

use appliance_backup_host::{Backups, Offline};
use appliance_config::{ApplianceArgs, Resolver};
use appliance_host::Host;
use appliance_server::{Logs, Server};
use appliance_store::Store;
use appliance_types::Branding;
use appliance_types::branding::ThemeTokens;
use clap::{Parser, Subcommand};

use crate::about::DaemonAbout;
use crate::backup::DaemonFiles;

/// Must match `name` in `appliance.toml`.
const NAME: &str = "wiretap-appliance";
/// Must match `env_prefix` in `appliance.toml`.
const ENV_PREFIX: &str = "WIRETAP_APPLIANCE";
const VERSION: &str = appliance_build_id::build_version!();
const KEEP_AUDIT: Duration = Duration::from_secs(90 * 24 * 60 * 60);
const LOGO: &[u8] = include_bytes!("../../../../crates/wiretap-backend/admin-ui/public/logo.svg");

#[derive(Debug, Parser)]
#[command(name = NAME, version = VERSION)]
#[command(
    about = "Administers a WireTAP appliance: the chassis's screens beside wiretap-server.",
    long_about = None
)]
struct Args {
    #[command(flatten)]
    appliance: ApplianceArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(flatten)]
    Offline(Offline),
}

/// `ExitCode` rather than `Result`, so a startup failure reaches the journal
/// through the chassis's `Display` and not as `Debug`.
#[tokio::main]
async fn main() -> ExitCode {
    let logs = Logs::install(&["wiretap_appliance"]);
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
    let bootstrap = Resolver::new(NAME, ENV_PREFIX).resolve(&args.appliance)?;
    tracing::info!(
        version = VERSION,
        config = %bootstrap.config_path.display(),
        db = %bootstrap.db.display(),
        "starting"
    );

    let host = Host::connect(&bootstrap).await;
    let backups = Backups::new(
        NAME,
        VERSION,
        &bootstrap.state_dir,
        host.clone(),
        DaemonFiles::installed(),
    );
    if let Some(Command::Offline(verb)) = args.command {
        return Ok(verb.run(&bootstrap, None, &backups).await?);
    }

    let store = Store::open(&bootstrap.db, None).await?;
    if store.user_count().await? == 0 {
        tracing::warn!(
            socket = %bootstrap.socket.display(),
            "this appliance has no accounts — the first person to reach it in a browser \
             makes one, or create it over the control socket"
        );
    }

    let server = Server::new(store.clone(), branding(), VERSION, logs);
    let sweeping = appliance_server::spawn_sweeper(
        store.clone(),
        host.clone(),
        Duration::from_secs(60 * 60),
        KEEP_AUDIT,
        server.shutdown(),
        |_store, _at| async {},
    );
    host.advertise(
        &store,
        appliance_host::Advert {
            product: NAME.to_string(),
        },
    )
    .await;

    let served = server
        .logo(LOGO, "image/svg+xml")
        .ui(bootstrap.ui.clone())
        .about(DaemonAbout::installed())
        .api(backups.routes())
        .api(appliance_host::routes(host, ()))
        .task(move |state, stop| backups.run_schedule(state, stop))
        .serve(&bootstrap)
        .await;

    sweeping.await?;
    Ok(served?)
}

fn branding() -> Branding {
    let mut branding = Branding::unbranded();
    branding.app_name = "WireTAP".to_string();
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
