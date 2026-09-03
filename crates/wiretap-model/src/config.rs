//! The `wiretap-server.toml` schema.
//!
//! Two properties govern the shape here. **Every field is optional**, so
//! "absent" stays distinguishable from "set to the default" — the
//! config-over-CLI merge depends on telling those apart. And **an unknown key
//! never fails the parse**: deployed files carry stale and commented-out keys,
//! and refusing to start on one would turn a package upgrade into an outage.
//!
//! Unknown keys are collected by `#[serde(flatten)]` rather than checked
//! against a hand-maintained list of field names, so the two cannot drift.

use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub server: ServerSection,
    pub postgres: PostgresSection,
    pub ingest: IngestSection,
    pub forward: ForwardSection,
    pub logging: LoggingSection,
    /// Top-level tables this schema does not define.
    #[serde(flatten)]
    pub unknown: toml::Table,
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
    #[serde(flatten)]
    pub unknown: toml::Table,
}

/// The retired direct-to-PostgreSQL sink.
///
/// Only `enable` is modelled, and only so the server can refuse to start.
/// The remaining ten keys a migrated file still carries are swept into
/// `unknown` and deliberately not reported — the whole section is retired, so
/// warning key-by-key would be noise. Deleting this section later is deleting
/// one struct and one branch.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct PostgresSection {
    pub enable: Option<bool>,
    #[serde(flatten)]
    pub unknown: toml::Table,
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
    #[serde(flatten)]
    pub unknown: toml::Table,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ForwardSection {
    pub enable: Option<bool>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub api_key: Option<String>,
    pub database: Option<String>,
    #[serde(flatten)]
    pub unknown: toml::Table,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LoggingSection {
    /// `DEBUG` | `INFO` | `WARNING` | `ERROR`. Note `WARNING`, not `WARN` —
    /// it is Python's spelling and deployed files use it.
    pub level: Option<String>,
    pub stats_interval: Option<f64>,
    #[serde(flatten)]
    pub unknown: toml::Table,
}

impl FileConfig {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("config parse failed: {e}"))
    }

    /// Keys the schema does not define, as `section.key`, for warning about.
    ///
    /// `[postgres]` is skipped: the section is retired wholesale, so
    /// [`Self::retired_postgres_sink`] is the thing worth saying about it.
    pub fn unknown_keys(&self) -> Vec<String> {
        let sections: [(&str, &toml::Table); 4] = [
            ("server", &self.server.unknown),
            ("ingest", &self.ingest.unknown),
            ("forward", &self.forward.unknown),
            ("logging", &self.logging.unknown),
        ];
        let mut out: Vec<String> = self.unknown.keys().cloned().collect();
        for (name, unknown) in sections {
            out.extend(unknown.keys().map(|k| format!("{name}.{k}")));
        }
        out.sort();
        out
    }

    /// `true` when the file enables the retired direct-PostgreSQL sink.
    ///
    /// The server refuses to start in that case: it cannot honour the setting
    /// — the gateway owns the database now — and carrying on without a sink
    /// would present a working capture that archives nothing.
    pub fn retired_postgres_sink(&self) -> bool {
        self.postgres.enable == Some(true)
    }
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

    /// A migrated file still carries the retired sink's ten other keys. They
    /// must not produce warnings — the section as a whole is the message.
    #[test]
    fn the_shipped_reference_config_has_no_unknown_keys() {
        let cfg = FileConfig::parse(REFERENCE).unwrap();
        assert_eq!(cfg.unknown_keys(), Vec::<String>::new());
        assert!(
            !cfg.postgres.unknown.is_empty(),
            "its other keys are swept up, not reported"
        );
    }

    /// The property the config-over-CLI merge depends on: a key the file does
    /// not mention must arrive as `None`, not as a default that the merge
    /// would then treat as an explicit setting. Tested on a minimal file
    /// because the shipped reference deliberately sets nearly everything.
    #[test]
    fn absent_is_distinguishable_from_defaulted() {
        let cfg = FileConfig::parse("[server]\niface = \"can0\"\n").unwrap();
        assert_eq!(cfg.server.iface.as_deref(), Some("can0"), "present");
        assert_eq!(cfg.server.port, None, "absent, not 0");
        assert_eq!(cfg.server.can_fd, None, "absent, not false");
        assert_eq!(
            cfg.logging.stats_interval, None,
            "absent section stays empty"
        );

        // And a value that *is* set arrives as set, including a falsey one.
        let cfg = FileConfig::parse("[server]\ncan_fd = false\nport = 0\n").unwrap();
        assert_eq!(cfg.server.can_fd, Some(false));
        assert_eq!(cfg.server.port, Some(0));
    }

    #[test]
    fn unknown_keys_are_reported_not_rejected() {
        let text = "[server]\niface = \"can0\"\nnonsense = 1\n\n[bogus]\nx = 2\n";
        let cfg = FileConfig::parse(text).expect("still parses");
        assert_eq!(cfg.server.iface.as_deref(), Some("can0"));
        assert_eq!(cfg.unknown_keys(), vec!["bogus", "server.nonsense"]);
    }

    /// Typed fields must keep their coercions with `flatten` in play — a TOML
    /// integer still has to satisfy an `f64` field.
    #[test]
    fn flatten_does_not_disturb_typed_fields() {
        let cfg = FileConfig::parse("[logging]\nstats_interval = 5\n").unwrap();
        assert_eq!(cfg.logging.stats_interval, Some(5.0));
        assert!(cfg.logging.unknown.is_empty());

        assert!(
            FileConfig::parse("[server]\nport = 99999\n").is_err(),
            "still range-checked"
        );
        assert!(
            FileConfig::parse("[server]\nport = \"x\"\n").is_err(),
            "still type-checked"
        );
    }

    #[test]
    fn the_retired_sink_is_detected() {
        assert!(FileConfig::parse("[postgres]\nenable = true\n")
            .unwrap()
            .retired_postgres_sink());

        // Present but off is what every migrated file looks like.
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
