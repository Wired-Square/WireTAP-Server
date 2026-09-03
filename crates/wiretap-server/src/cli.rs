//! Command-line surface.
//!
//! Every flag the Python accepted is still accepted, including the ones for
//! the retired PostgreSQL sink: a deployed unit file may pass them, and
//! failing to parse a flag is a worse first impression than explaining why it
//! no longer does anything. The retired ones are hidden from `--help` and
//! rejected by [`crate::settings`] if they would change behaviour.
//!
//! Defaults here are the Python's `build_parser()` defaults, which differ in
//! places from the values in the shipped `wiretap-server.toml`.

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "wiretap-server",
    about = "SocketCAN to TCP GVRET bridge, forwarding captures to a WireTAP gateway",
    version
)]
pub struct Cli {
    /// CAN interface(s), comma-separated. Empty for an ingest-only deployment.
    #[arg(short, long, default_value = "can0")]
    pub iface: String,

    /// GVRET listen address.
    #[arg(long, default_value = "0.0.0.0")]
    pub host: String,

    /// GVRET listen port. 23 needs CAP_NET_BIND_SERVICE.
    #[arg(short, long, default_value_t = 23)]
    pub port: u16,

    /// GVRET bus number offset; the first interface becomes bus N.
    #[arg(long, default_value_t = 0)]
    pub bus_offset: u8,

    /// Echo frames to the console in candump format.
    #[arg(short, long)]
    pub echo_console: bool,

    /// Highlight printable ASCII in console echo.
    #[arg(short, long)]
    pub colour: bool,

    /// Direction tag recorded for captured frames.
    #[arg(long, default_value = "rx")]
    pub default_dir: String,

    /// Enable CAN FD (64-byte payloads).
    #[arg(long)]
    pub can_fd: bool,

    #[arg(long, default_value = "INFO", value_parser = ["DEBUG", "INFO", "WARNING", "ERROR"])]
    pub log_level: String,

    /// Seconds between periodic stats logs; 0 disables.
    #[arg(long, default_value_t = 10.0)]
    pub stats_interval: f64,

    // --- binary ingest listener (microcontroller clients) ---
    /// Enable the binary TCP ingest listener.
    #[arg(long)]
    pub ingest_enable: bool,
    #[arg(long, default_value = "0.0.0.0")]
    pub ingest_host: String,
    #[arg(long, default_value_t = 9323)]
    pub ingest_port: u16,
    /// Shared auth token, or `WIRETAP_INGEST_TOKEN`. Empty disables auth.
    #[arg(long)]
    pub ingest_token: Option<String>,
    /// Expected client keepalive; clients silent for 3x this are dropped.
    #[arg(long, default_value_t = 30.0)]
    pub ingest_keepalive_secs: f64,
    #[arg(long, default_value_t = 256)]
    pub ingest_max_batch_frames: usize,

    // --- forward to a gateway ---
    /// Forward captured frames to a WireTAP backend gateway.
    #[arg(long)]
    pub forward_enable: bool,
    #[arg(long, default_value = "127.0.0.1")]
    pub forward_host: String,
    #[arg(long, default_value_t = 9323)]
    pub forward_port: u16,
    /// Gateway API key, or `WIRETAP_FORWARD_TOKEN`.
    #[arg(long)]
    pub forward_api_key: Option<String>,
    /// Target capture database; empty uses the gateway's default.
    #[arg(long, default_value = "")]
    pub forward_database: String,

    // --- retired: the direct-to-PostgreSQL sink ---
    /// Retired. The gateway owns the database; use --forward-enable.
    #[arg(long, hide = true)]
    pub pg_enable: bool,
    #[arg(long, hide = true)]
    pub pg_dsn: Option<String>,
    #[arg(long, hide = true)]
    pub pg_func: Option<String>,
    #[arg(long, hide = true)]
    pub pg_write_mode: Option<String>,
    #[arg(long, hide = true)]
    pub pg_batch_size: Option<usize>,
    #[arg(long, hide = true)]
    pub pg_flush_interval: Option<f64>,
    #[arg(long, hide = true)]
    pub pg_queue_size: Option<usize>,
    #[arg(long, hide = true)]
    pub pg_dir: Option<String>,
    #[arg(long, hide = true)]
    pub pg_cache_path: Option<String>,
    #[arg(long, hide = true)]
    pub pg_cache_max_mb: Option<u64>,
    #[arg(long, hide = true)]
    pub pg_queue_flush_pct: Option<u8>,

    /// TOML config file. Note: values in the file OVERRIDE these flags.
    #[arg(short = 'C', long)]
    pub config: Option<String>,

    /// Parse the configuration, print what it resolves to, and exit.
    #[arg(long)]
    pub check_config: bool,
}

impl Cli {
    /// Retired `--pg-*` flags the caller actually passed, for warning about.
    /// `--pg-enable` is handled separately: it is refused, not warned.
    pub fn retired_flags_used(&self) -> Vec<&'static str> {
        [
            ("--pg-dsn", self.pg_dsn.is_some()),
            ("--pg-func", self.pg_func.is_some()),
            ("--pg-write-mode", self.pg_write_mode.is_some()),
            ("--pg-batch-size", self.pg_batch_size.is_some()),
            ("--pg-flush-interval", self.pg_flush_interval.is_some()),
            ("--pg-queue-size", self.pg_queue_size.is_some()),
            ("--pg-dir", self.pg_dir.is_some()),
            ("--pg-cache-path", self.pg_cache_path.is_some()),
            ("--pg-cache-max-mb", self.pg_cache_max_mb.is_some()),
            ("--pg-queue-flush-pct", self.pg_queue_flush_pct.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, used)| used.then_some(name))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// The defaults are a compatibility surface: a unit file that passes no
    /// flags must behave as the Python did.
    #[test]
    fn defaults_match_the_python() {
        let c = Cli::parse_from(["wiretap-server"]);
        assert_eq!(c.iface, "can0");
        assert_eq!(c.host, "0.0.0.0");
        assert_eq!(c.port, 23);
        assert_eq!(c.bus_offset, 0);
        assert_eq!(c.default_dir, "rx");
        assert!(!c.can_fd);
        assert_eq!(c.log_level, "INFO");
        assert_eq!(c.stats_interval, 10.0);
        assert_eq!(c.ingest_port, 9323);
        assert_eq!(c.ingest_keepalive_secs, 30.0);
        assert_eq!(c.ingest_max_batch_frames, 256);
        assert_eq!(c.forward_host, "127.0.0.1");
        assert_eq!(c.forward_port, 9323);
        assert_eq!(c.forward_database, "");
    }

    /// Short forms the Python had; a deployed command line may use them.
    #[test]
    fn short_flags_are_preserved() {
        let c = Cli::parse_from(["wiretap-server", "-i", "can1", "-p", "2323", "-e", "-c"]);
        assert_eq!(c.iface, "can1");
        assert_eq!(c.port, 2323);
        assert!(c.echo_console);
        assert!(c.colour);

        let c = Cli::parse_from(["wiretap-server", "-C", "/etc/wiretap-server/x.toml"]);
        assert_eq!(c.config.as_deref(), Some("/etc/wiretap-server/x.toml"));
    }

    /// A unit file passing retired flags must still parse — refusing at the
    /// argument parser would give an operator no way to read the explanation.
    #[test]
    fn retired_flags_still_parse_and_are_reported() {
        let c = Cli::parse_from([
            "wiretap-server",
            "--pg-enable",
            "--pg-dsn",
            "postgresql://x/y",
            "--pg-batch-size",
            "500",
        ]);
        assert!(c.pg_enable);
        assert_eq!(c.retired_flags_used(), vec!["--pg-dsn", "--pg-batch-size"]);
    }

    #[test]
    fn an_unknown_log_level_is_rejected() {
        assert!(Cli::try_parse_from(["wiretap-server", "--log-level", "TRACE"]).is_err());
        // Python's spelling, not Rust's.
        assert!(Cli::try_parse_from(["wiretap-server", "--log-level", "WARNING"]).is_ok());
    }
}
