//! The `wiretap-server.toml` schema.
//!
//! **Every field is optional.** A file only says what it wants to change, so
//! "absent" has to stay distinguishable from "set to the default" — that is
//! what makes the config-over-CLI merge in the server work at all.
//!
//! There is deliberately **no `deny_unknown_fields`**. Deployed files carry
//! commented-out and stale keys (`cache_path` is commented out in the shipped
//! reference file), and a hard parse failure on one of those would turn a
//! package upgrade into an outage. Unknown keys are reported by
//! [`unknown_keys`] and warned about instead.

use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub server: ServerSection,
    pub postgres: PostgresSection,
    pub ingest: IngestSection,
    pub forward: ForwardSection,
    pub logging: LoggingSection,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ServerSection {
    /// Comma-separated interface list, e.g. `can0,can1`. Empty means an
    /// ingest-only deployment with no local CAN hardware.
    pub iface: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub bus_offset: Option<u8>,
    pub echo_console: Option<bool>,
    pub colour: Option<bool>,
    pub default_dir: Option<String>,
    pub can_fd: Option<bool>,
}

/// The retired direct-to-PostgreSQL sink.
///
/// Still parsed, and only so the server can **refuse to start** when it is
/// enabled. Ignoring it would silently drop every frame an operator believes
/// is being archived; see [`FileConfig::retired_postgres_sink`].
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct PostgresSection {
    pub enable: Option<bool>,
    pub dsn: Option<String>,
    pub func: Option<String>,
    pub write_mode: Option<String>,
    pub batch_size: Option<usize>,
    pub flush_interval: Option<f64>,
    pub queue_size: Option<usize>,
    pub dir: Option<String>,
    pub cache_path: Option<String>,
    pub cache_max_mb: Option<u64>,
    pub queue_flush_pct: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct IngestSection {
    pub enable: Option<bool>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub token: Option<String>,
    pub keepalive_secs: Option<f64>,
    pub max_batch_frames: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ForwardSection {
    pub enable: Option<bool>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub api_key: Option<String>,
    pub database: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LoggingSection {
    /// `DEBUG` | `INFO` | `WARNING` | `ERROR`. Note `WARNING`, not `WARN` —
    /// it is Python's spelling and deployed files use it.
    pub level: Option<String>,
    pub stats_interval: Option<f64>,
}

/// Section name → the keys it accepts. Used only for the unknown-key warning;
/// parsing itself ignores anything not listed.
const KNOWN: &[(&str, &[&str])] = &[
    (
        "server",
        &[
            "iface",
            "host",
            "port",
            "bus_offset",
            "echo_console",
            "colour",
            "default_dir",
            "can_fd",
        ],
    ),
    (
        "postgres",
        &[
            "enable",
            "dsn",
            "func",
            "write_mode",
            "batch_size",
            "flush_interval",
            "queue_size",
            "dir",
            "cache_path",
            "cache_max_mb",
            "queue_flush_pct",
        ],
    ),
    (
        "ingest",
        &[
            "enable",
            "host",
            "port",
            "token",
            "keepalive_secs",
            "max_batch_frames",
        ],
    ),
    (
        "forward",
        &["enable", "host", "port", "api_key", "database"],
    ),
    ("logging", &["level", "stats_interval"]),
];

impl FileConfig {
    /// Parse a config file. Unknown keys do not fail; ask [`unknown_keys`] for
    /// them and warn.
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("config parse failed: {e}"))
    }

    /// `true` when the file enables the retired direct-PostgreSQL sink.
    ///
    /// The server refuses to start in that case. It cannot honour the setting
    /// — the gateway owns the database now — and carrying on without a sink
    /// would look like a working capture while archiving nothing.
    pub fn retired_postgres_sink(&self) -> bool {
        self.postgres.enable == Some(true)
    }

    /// `true` when both sinks are enabled. The Python implementation
    /// documented these as mutually exclusive; with `[postgres]` retired this
    /// can only be reached alongside [`Self::retired_postgres_sink`].
    pub fn both_sinks_enabled(&self) -> bool {
        self.postgres.enable == Some(true) && self.forward.enable == Some(true)
    }
}

/// Keys present in the file that no section recognises, as `section.key`.
///
/// Reported rather than rejected: an operator's stale key should produce a
/// warning naming it, not a daemon that will not start.
pub fn unknown_keys(text: &str) -> Result<Vec<String>, String> {
    let table: toml::Table =
        toml::from_str(text).map_err(|e| format!("config parse failed: {e}"))?;
    let mut out = Vec::new();
    for (section, value) in &table {
        let Some(known) = KNOWN.iter().find(|(n, _)| n == section).map(|(_, k)| *k) else {
            out.push(section.clone());
            continue;
        };
        if let Some(t) = value.as_table() {
            for key in t.keys() {
                if !known.contains(&key.as_str()) {
                    out.push(format!("{section}.{key}"));
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Split a comma-separated interface list, discarding blanks and whitespace.
/// `""` means no local capture, which is a valid ingest-only deployment.
pub fn parse_ifaces(spec: &str) -> Vec<String> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference config shipped to every deployment. If this stops
    /// parsing, an upgrade breaks in the field rather than in CI.
    const REFERENCE: &str = include_str!("../../../tools/oracle/wiretap-server.toml");

    #[test]
    fn the_shipped_reference_config_parses() {
        let cfg = FileConfig::parse(REFERENCE).expect("reference config parses");
        assert_eq!(cfg.server.iface.as_deref(), Some("can0"));
        assert_eq!(cfg.server.port, Some(23));
        assert_eq!(cfg.server.can_fd, Some(false));
        assert_eq!(cfg.ingest.port, Some(9323));
        assert_eq!(cfg.forward.host.as_deref(), Some("backend.local"));
        assert_eq!(cfg.logging.level.as_deref(), Some("INFO"));
    }

    #[test]
    fn the_shipped_reference_config_has_no_unknown_keys() {
        assert_eq!(unknown_keys(REFERENCE).unwrap(), Vec::<String>::new());
    }

    /// `cache_path` is commented out in the shipped file, so "absent" must
    /// survive parsing as `None` rather than becoming a default that the
    /// merge would then treat as an explicit setting.
    #[test]
    fn absent_is_distinguishable_from_defaulted() {
        let cfg = FileConfig::parse(REFERENCE).unwrap();
        assert_eq!(cfg.postgres.cache_path, None);
        assert_eq!(cfg.postgres.cache_max_mb, Some(1000));
    }

    #[test]
    fn unknown_keys_are_reported_not_rejected() {
        let text = "[server]\niface = \"can0\"\nnonsense = 1\n\n[bogus]\nx = 2\n";
        let cfg = FileConfig::parse(text).expect("still parses");
        assert_eq!(cfg.server.iface.as_deref(), Some("can0"));
        assert_eq!(
            unknown_keys(text).unwrap(),
            vec!["bogus", "server.nonsense"]
        );
    }

    #[test]
    fn the_retired_sink_is_detected() {
        let on = FileConfig::parse("[postgres]\nenable = true\n").unwrap();
        assert!(on.retired_postgres_sink());

        // Present but off is fine — that is what every migrated file looks like.
        let off = FileConfig::parse("[postgres]\nenable = false\ndsn = \"x\"\n").unwrap();
        assert!(!off.retired_postgres_sink());

        let absent = FileConfig::parse("[server]\niface = \"can0\"\n").unwrap();
        assert!(!absent.retired_postgres_sink());
    }

    #[test]
    fn ifaces_split_and_tolerate_whitespace_and_blanks() {
        assert_eq!(parse_ifaces("can0"), ["can0"]);
        assert_eq!(parse_ifaces("can0,can1"), ["can0", "can1"]);
        assert_eq!(parse_ifaces(" can0 , can1 "), ["can0", "can1"]);
        assert_eq!(parse_ifaces("can0,,can1,"), ["can0", "can1"]);
        assert!(parse_ifaces("").is_empty());
        assert!(parse_ifaces("  ").is_empty());
    }
}
