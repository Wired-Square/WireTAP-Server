//! Resolving the effective configuration from flags, the config file and the
//! environment.
//!
//! **The file overrides the flags**, not the other way round. That is what the
//! Python did — `apply_config_overrides` writes file values over the parsed
//! arguments — and deployed units pass both, so reversing it would silently
//! change what running systems do. The shipped `wiretap-server.toml` header
//! claims the opposite; the code is what deployments depend on, and
//! `--check-config` exists so the result is inspectable rather than argued
//! about.

use std::fmt::Write as _;

use wiretap_model::{config::FileConfig, parse_ifaces, Direction};

use crate::cli::Cli;

/// Where captured frames are sent for archiving.
#[derive(Debug, Clone, PartialEq)]
pub struct Forward {
    pub host: String,
    pub port: u16,
    pub api_key: String,
    /// Empty means the gateway's default capture database.
    pub database: String,
}

/// The binary TCP listener that accepts pushed frames from capture devices.
#[derive(Debug, Clone, PartialEq)]
pub struct Ingest {
    pub host: String,
    pub port: u16,
    /// Empty disables authentication.
    pub token: String,
    pub keepalive_secs: f64,
    pub max_batch_frames: usize,
}

/// Everything the server needs to run, with every source already applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub ifaces: Vec<String>,
    pub host: String,
    pub port: u16,
    pub bus_offset: u8,
    pub echo_console: bool,
    pub colour: bool,
    pub default_dir: Direction,
    pub can_fd: bool,
    pub log_level: String,
    pub stats_interval: f64,
    pub ingest: Option<Ingest>,
    pub forward: Option<Forward>,
}

/// Why the server declined to start.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsError {
    /// `[postgres].enable` or `--pg-enable`.
    RetiredPostgresSink,
    /// A direction tag that is neither `rx` nor `tx`.
    BadDirection(String),
    /// The config file could not be read or parsed.
    Config(String),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Named at length because the alternative — starting anyway — is a
            // capture that looks healthy and archives nothing.
            Self::RetiredPostgresSink => write!(
                f,
                "the direct-to-PostgreSQL sink has been removed: the gateway owns the \
                 database and nothing else writes to it.\n\
                 Set [postgres].enable = false and configure [forward] to point at a \
                 gateway. To move an existing archive, use tools/migrate_to_timescale.py.\n\
                 Refusing to start rather than capturing with no archive."
            ),
            Self::BadDirection(d) => {
                write!(f, "direction must be \"rx\" or \"tx\", got {d:?}")
            }
            Self::Config(e) => write!(f, "{e}"),
        }
    }
}

/// What resolving produced, including anything the operator should hear about
/// but which is not fatal.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub settings: Settings,
    pub warnings: Vec<String>,
}

impl Settings {
    /// Apply flags, then the file over them, then the environment for secrets.
    ///
    /// `file` is the already-parsed config, so this is testable without
    /// touching a filesystem; `env` is passed in for the same reason.
    pub fn resolve(
        cli: &Cli,
        file: Option<&FileConfig>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Resolved, SettingsError> {
        let mut warnings = Vec::new();

        // The retired sink is refused before anything else: if it is on, no
        // other setting can make the result correct.
        if cli.pg_enable || file.is_some_and(FileConfig::retired_postgres_sink) {
            return Err(SettingsError::RetiredPostgresSink);
        }
        let retired = cli.retired_flags_used();
        if !retired.is_empty() {
            warnings.push(format!("ignoring retired flags: {}", retired.join(", ")));
        }

        let mut s = Settings {
            ifaces: parse_ifaces(&cli.iface),
            host: cli.host.clone(),
            port: cli.port,
            bus_offset: cli.bus_offset,
            echo_console: cli.echo_console,
            colour: cli.colour,
            default_dir: parse_direction(&cli.default_dir)?,
            can_fd: cli.can_fd,
            log_level: cli.log_level.to_uppercase(),
            stats_interval: cli.stats_interval,
            ingest: cli.ingest_enable.then(|| Ingest {
                host: cli.ingest_host.clone(),
                port: cli.ingest_port,
                token: cli.ingest_token.clone().unwrap_or_default(),
                keepalive_secs: cli.ingest_keepalive_secs,
                max_batch_frames: cli.ingest_max_batch_frames,
            }),
            forward: cli.forward_enable.then(|| Forward {
                host: cli.forward_host.clone(),
                port: cli.forward_port,
                api_key: cli.forward_api_key.clone().unwrap_or_default(),
                database: cli.forward_database.clone(),
            }),
        };

        if let Some(f) = file {
            warnings.extend(
                f.unknown_keys()
                    .iter()
                    .map(|k| format!("unknown config key, ignored: {k}")),
            );
            s.apply_file(cli, f)?;
        }

        // Secrets last, and only to fill a gap: an explicitly configured value
        // beats the environment, so a unit file's EnvironmentFile cannot
        // silently override what an operator wrote down.
        if let Some(i) = s.ingest.as_mut() {
            if i.token.is_empty() {
                i.token = env("WIRETAP_INGEST_TOKEN").unwrap_or_default();
            }
        }
        if let Some(fwd) = s.forward.as_mut() {
            if fwd.api_key.is_empty() {
                fwd.api_key = env("WIRETAP_FORWARD_TOKEN").unwrap_or_default();
            }
        }

        if s.ifaces.is_empty() && s.ingest.is_none() {
            warnings.push(
                "no CAN interfaces and no ingest listener: this server will capture nothing".into(),
            );
        }
        if s.forward.is_none() {
            warnings.push(
                "no [forward] gateway configured: frames will be bridged to GVRET clients \
                 but not archived"
                    .into(),
            );
        }

        Ok(Resolved {
            settings: s,
            warnings,
        })
    }

    /// File values over flag values, matching `apply_config_overrides`.
    fn apply_file(&mut self, cli: &Cli, f: &FileConfig) -> Result<(), SettingsError> {
        let srv = &f.server;
        if let Some(v) = &srv.iface {
            self.ifaces = parse_ifaces(v);
        }
        if let Some(v) = &srv.host {
            self.host = v.clone();
        }
        if let Some(v) = srv.port {
            self.port = v;
        }
        if let Some(v) = srv.bus_offset {
            self.bus_offset = v;
        }
        if let Some(v) = srv.echo_console {
            self.echo_console = v;
        }
        if let Some(v) = srv.colour {
            self.colour = v;
        }
        if let Some(v) = &srv.default_dir {
            self.default_dir = parse_direction(v)?;
        }
        if let Some(v) = srv.can_fd {
            self.can_fd = v;
        }

        if let Some(v) = &f.logging.level {
            self.log_level = v.to_uppercase();
        }
        if let Some(v) = f.logging.stats_interval {
            self.stats_interval = v;
        }

        // A section turns its listener on, but its other keys apply whether or
        // not it did — so a file can carry settings for a listener the command
        // line enables, which is how the Python behaved.
        if f.ingest.enable == Some(true) && self.ingest.is_none() {
            self.ingest = Some(Ingest {
                host: cli.ingest_host.clone(),
                port: cli.ingest_port,
                token: cli.ingest_token.clone().unwrap_or_default(),
                keepalive_secs: cli.ingest_keepalive_secs,
                max_batch_frames: cli.ingest_max_batch_frames,
            });
        }
        if let Some(i) = self.ingest.as_mut() {
            if let Some(v) = &f.ingest.host {
                i.host = v.clone();
            }
            if let Some(v) = f.ingest.port {
                i.port = v;
            }
            if let Some(v) = &f.ingest.token {
                i.token = v.clone();
            }
            if let Some(v) = f.ingest.keepalive_secs {
                i.keepalive_secs = v;
            }
            if let Some(v) = f.ingest.max_batch_frames {
                i.max_batch_frames = v;
            }
        }

        if f.forward.enable == Some(true) && self.forward.is_none() {
            self.forward = Some(Forward {
                host: cli.forward_host.clone(),
                port: cli.forward_port,
                api_key: cli.forward_api_key.clone().unwrap_or_default(),
                database: cli.forward_database.clone(),
            });
        }
        if let Some(fwd) = self.forward.as_mut() {
            if let Some(v) = &f.forward.host {
                fwd.host = v.clone();
            }
            if let Some(v) = f.forward.port {
                fwd.port = v;
            }
            if let Some(v) = &f.forward.api_key {
                fwd.api_key = v.clone();
            }
            if let Some(v) = &f.forward.database {
                fwd.database = v.clone();
            }
        }
        Ok(())
    }

    /// What `--check-config` prints. Secrets are shown as set/unset, never
    /// echoed — the output is meant to be safe to paste into a bug report.
    pub fn describe(&self) -> String {
        let mut o = String::new();
        let ifaces = if self.ifaces.is_empty() {
            "(none)".to_string()
        } else {
            self.ifaces.join(", ")
        };
        let _ = writeln!(o, "interfaces      {ifaces}");
        let _ = writeln!(o, "gvret listen    {}:{}", self.host, self.port);
        let _ = writeln!(o, "bus offset      {}", self.bus_offset);
        let _ = writeln!(o, "can fd          {}", self.can_fd);
        let _ = writeln!(o, "direction       {:?}", self.default_dir);
        let _ = writeln!(
            o,
            "console echo    {} (colour {})",
            self.echo_console, self.colour
        );
        let _ = writeln!(o, "log level       {}", self.log_level);
        let _ = writeln!(o, "stats interval  {}s", self.stats_interval);
        match &self.ingest {
            Some(i) => {
                let _ = writeln!(o, "ingest listen   {}:{}", i.host, i.port);
                let _ = writeln!(
                    o,
                    "ingest auth     {}",
                    if i.token.is_empty() {
                        "disabled"
                    } else {
                        "token set"
                    }
                );
                let _ = writeln!(
                    o,
                    "ingest limits   {} frames/batch, keepalive {}s",
                    i.max_batch_frames, i.keepalive_secs
                );
            }
            None => {
                let _ = writeln!(o, "ingest listen   disabled");
            }
        }
        match &self.forward {
            Some(f) => {
                let _ = writeln!(o, "forward to      {}:{}", f.host, f.port);
                let _ = writeln!(
                    o,
                    "forward db      {}",
                    if f.database.is_empty() {
                        "(gateway default)"
                    } else {
                        &f.database
                    }
                );
                let _ = writeln!(
                    o,
                    "forward auth    {}",
                    if f.api_key.is_empty() {
                        "MISSING"
                    } else {
                        "key set"
                    }
                );
            }
            None => {
                let _ = writeln!(o, "forward to      disabled — frames will not be archived");
            }
        }
        o
    }
}

fn parse_direction(s: &str) -> Result<Direction, SettingsError> {
    match s.to_ascii_lowercase().as_str() {
        "rx" => Ok(Direction::Rx),
        "tx" => Ok(Direction::Tx),
        _ => Err(SettingsError::BadDirection(s.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        let mut v = vec!["wiretap-server"];
        v.extend_from_slice(args);
        Cli::parse_from(v)
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn resolve(args: &[&str], toml: Option<&str>) -> Result<Resolved, SettingsError> {
        let parsed = toml.map(|t| FileConfig::parse(t).expect("test config parses"));
        Settings::resolve(&cli(args), parsed.as_ref(), no_env)
    }

    #[test]
    fn flags_alone_resolve() {
        let r = resolve(&["-i", "can0,can1", "-p", "2323"], None).unwrap();
        assert_eq!(r.settings.ifaces, ["can0", "can1"]);
        assert_eq!(r.settings.port, 2323);
        assert_eq!(r.settings.default_dir, Direction::Rx);
    }

    /// The precedence that deployed units depend on, and that the shipped
    /// config file's own header gets backwards.
    #[test]
    fn the_config_file_overrides_the_command_line() {
        let r = resolve(
            &["-i", "can9", "-p", "1111"],
            Some("[server]\niface = \"can0\"\nport = 23\n"),
        )
        .unwrap();
        assert_eq!(r.settings.ifaces, ["can0"], "file wins over -i");
        assert_eq!(r.settings.port, 23, "file wins over -p");
    }

    /// A key the file does not mention must leave the flag's value alone.
    #[test]
    fn the_file_only_overrides_what_it_mentions() {
        let r = resolve(
            &["-p", "2323", "--can-fd"],
            Some("[server]\niface = \"can1\"\n"),
        )
        .unwrap();
        assert_eq!(r.settings.ifaces, ["can1"]);
        assert_eq!(r.settings.port, 2323, "untouched by the file");
        assert!(r.settings.can_fd, "untouched by the file");
    }

    #[test]
    fn the_retired_sink_is_refused_from_either_source() {
        assert_eq!(
            resolve(&["--pg-enable"], None).unwrap_err(),
            SettingsError::RetiredPostgresSink
        );
        assert_eq!(
            resolve(&[], Some("[postgres]\nenable = true\n")).unwrap_err(),
            SettingsError::RetiredPostgresSink
        );
        // The message has to tell an operator what to do instead.
        let msg = SettingsError::RetiredPostgresSink.to_string();
        assert!(msg.contains("[forward]"), "names the replacement");
        assert!(
            msg.contains("migrate_to_timescale"),
            "names the migration tool"
        );
    }

    /// A migrated file still carries the section, switched off. That must
    /// start normally and say nothing about it.
    #[test]
    fn a_disabled_postgres_section_is_accepted_silently() {
        let r = resolve(
            &[],
            Some("[postgres]\nenable = false\ndsn = \"postgresql://old/db\"\nbatch_size = 1000\n"),
        )
        .unwrap();
        assert!(
            r.warnings.iter().all(|w| !w.contains("postgres")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn retired_flags_warn_without_failing() {
        let r = resolve(&["--pg-batch-size", "500", "--pg-dsn", "x"], None).unwrap();
        assert!(
            r.warnings.iter().any(|w| w.contains("--pg-dsn")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn sections_enable_their_listeners() {
        let r = resolve(
            &[],
            Some("[forward]\nenable = true\nhost = \"gw.local\"\nport = 9999\napi_key = \"k\"\n"),
        )
        .unwrap();
        let f = r.settings.forward.expect("forward enabled by the file");
        assert_eq!(
            (f.host.as_str(), f.port, f.api_key.as_str()),
            ("gw.local", 9999, "k")
        );

        let r = resolve(&[], Some("[ingest]\nenable = true\nport = 9000\n")).unwrap();
        assert_eq!(r.settings.ingest.expect("ingest enabled").port, 9000);
    }

    /// A file may carry settings for a listener the command line switches on.
    #[test]
    fn file_settings_apply_to_a_listener_enabled_on_the_command_line() {
        let r = resolve(
            &["--forward-enable"],
            Some("[forward]\nhost = \"gw.local\"\n"),
        )
        .unwrap();
        let f = r.settings.forward.unwrap();
        assert_eq!(f.host, "gw.local");
        assert_eq!(f.port, 9323, "default kept");
    }

    #[test]
    fn secrets_come_from_the_environment_only_when_not_configured() {
        let env = |k: &str| match k {
            "WIRETAP_FORWARD_TOKEN" => Some("from-env".to_string()),
            _ => None,
        };
        let r = Settings::resolve(&cli(&["--forward-enable"]), None, env).unwrap();
        assert_eq!(r.settings.forward.unwrap().api_key, "from-env");

        // An explicit value wins: an EnvironmentFile must not quietly replace
        // what an operator wrote in the config.
        let f = FileConfig::parse("[forward]\nenable = true\napi_key = \"from-file\"\n").unwrap();
        let r = Settings::resolve(&cli(&[]), Some(&f), env).unwrap();
        assert_eq!(r.settings.forward.unwrap().api_key, "from-file");
    }

    #[test]
    fn unknown_config_keys_warn() {
        let r = resolve(&[], Some("[server]\nnonsense = 1\n")).unwrap();
        assert!(
            r.warnings.iter().any(|w| w.contains("server.nonsense")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn capturing_nothing_is_worth_saying_out_loud() {
        let r = resolve(&["-i", ""], None).unwrap();
        assert!(
            r.warnings.iter().any(|w| w.contains("capture nothing")),
            "{:?}",
            r.warnings
        );

        let r = resolve(&[], None).unwrap();
        assert!(
            r.warnings.iter().any(|w| w.contains("not archived")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn a_bad_direction_is_refused() {
        assert_eq!(
            resolve(&["--default-dir", "sideways"], None).unwrap_err(),
            SettingsError::BadDirection("sideways".into())
        );
        assert_eq!(
            resolve(&["--default-dir", "TX"], None)
                .unwrap()
                .settings
                .default_dir,
            Direction::Tx
        );
    }

    /// The shipped reference config must resolve, and land on the values it
    /// documents rather than the flag defaults.
    #[test]
    fn the_shipped_reference_config_resolves() {
        const REFERENCE: &str = include_str!("../../../tools/oracle/wiretap-server.toml");
        let r = resolve(&[], Some(REFERENCE)).unwrap();
        assert_eq!(r.settings.ifaces, ["can0"]);
        assert_eq!(r.settings.port, 23);
        assert_eq!(
            r.settings.stats_interval, 60.0,
            "the file's value, not the flag default of 10"
        );
        assert!(
            r.settings.ingest.is_none(),
            "the reference has ingest disabled"
        );
        assert!(r.settings.forward.is_none(), "and forward disabled");
    }

    #[test]
    fn describe_does_not_echo_secrets() {
        let f = FileConfig::parse(
            "[forward]\nenable = true\napi_key = \"super-secret\"\n\
             [ingest]\nenable = true\ntoken = \"also-secret\"\n",
        )
        .unwrap();
        let out = Settings::resolve(&cli(&[]), Some(&f), no_env)
            .unwrap()
            .settings
            .describe();
        assert!(!out.contains("super-secret"), "{out}");
        assert!(!out.contains("also-secret"), "{out}");
        assert!(
            out.contains("key set") && out.contains("token set"),
            "{out}"
        );
    }
}
