//! Resolving the effective configuration from flags, the config file and the
//! environment.
//!
//! **The file overrides the flags**, not the other way round. That is what the
//! Python did — `apply_config_overrides` writes file values over the parsed
//! arguments — and deployed units pass both, so reversing it would silently
//! change what running systems do. The shipped `wiretap-server.toml` header
//! claims the opposite; `--check-config` exists so the result is inspectable
//! rather than argued about.

use std::fmt;

use wiretap_model::{config::FileConfig, parse_ifaces, Direction, Secret};

use crate::cli::Cli;

/// Apply a file value over a flag value, where the file speaks.
fn over<T>(dst: &mut T, src: Option<T>) {
    if let Some(v) = src {
        *dst = v;
    }
}

/// Where captured frames are sent for archiving.
#[derive(Debug, Clone)]
pub struct Forward {
    pub host: String,
    pub port: u16,
    pub api_key: Secret,
    /// Empty means the gateway's default capture database.
    pub database: String,
}

/// The binary TCP listener that accepts pushed frames from capture devices.
#[derive(Debug, Clone)]
pub struct Ingest {
    pub host: String,
    pub port: u16,
    /// Empty disables authentication — the Python's behaviour, kept.
    pub token: Secret,
    pub keepalive_secs: f64,
    pub max_batch_frames: usize,
}

impl Ingest {
    fn from_cli(cli: &Cli) -> Self {
        Self {
            host: cli.ingest_host.clone(),
            port: cli.ingest_port,
            token: Secret::new(cli.ingest_token.clone().unwrap_or_default()),
            keepalive_secs: cli.ingest_keepalive_secs,
            max_batch_frames: cli.ingest_max_batch_frames,
        }
    }
}

impl Forward {
    fn from_cli(cli: &Cli) -> Self {
        Self {
            host: cli.forward_host.clone(),
            port: cli.forward_port,
            api_key: Secret::new(cli.forward_api_key.clone().unwrap_or_default()),
            database: cli.forward_database.clone(),
        }
    }
}

/// Everything the server needs to run, with every source already applied.
#[derive(Debug, Clone)]
pub struct Settings {
    pub ifaces: Vec<String>,
    pub host: String,
    pub port: u16,
    pub bus_offset: u8,
    pub echo_console: bool,
    pub colour: bool,
    pub default_dir: Direction,
    pub can_fd: bool,
    pub log_level: LogLevel,
    pub stats_interval: f64,
    pub ingest: Option<Ingest>,
    pub forward: Option<Forward>,
}

/// Python's spellings, because deployed config files use them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
}

impl LogLevel {
    pub const NAMES: [&'static str; 4] = ["DEBUG", "INFO", "WARNING", "ERROR"];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warning => "WARNING",
            Self::Error => "ERROR",
        }
    }
}

impl std::str::FromStr for LogLevel {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s.to_ascii_uppercase().as_str() {
            "DEBUG" => Ok(Self::Debug),
            "INFO" => Ok(Self::Info),
            "WARNING" => Ok(Self::Warning),
            "ERROR" => Ok(Self::Error),
            _ => Err(()),
        }
    }
}

/// Why the server declined to start.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsError {
    /// `[postgres].enable` or `--pg-enable`.
    RetiredPostgresSink,
    BadDirection(String),
    BadLogLevel(String),
    /// The config file could not be read.
    Io {
        path: String,
        err: String,
    },
    /// The config file is not valid TOML, or a value has the wrong type.
    Parse(String),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RetiredPostgresSink => write!(
                f,
                "the direct-to-PostgreSQL sink has been removed: the gateway owns the \
                 database and nothing else writes to it.\n\
                 Set [postgres].enable = false and configure [forward] to point at a \
                 gateway. To move an existing archive, use tools/migrate_to_timescale.py.\n\
                 Refusing to start rather than capturing with no archive."
            ),
            Self::BadDirection(d) => write!(f, "direction must be \"rx\" or \"tx\", got {d:?}"),
            Self::BadLogLevel(l) => {
                write!(
                    f,
                    "log level must be one of {:?}, got {l:?}",
                    LogLevel::NAMES
                )
            }
            Self::Io { path, err } => write!(f, "cannot read {path}: {err}"),
            Self::Parse(e) => write!(f, "{e}"),
        }
    }
}

/// Something an operator should hear about that is not fatal.
///
/// A value rather than a sentence, so the control socket and the web UI can
/// surface these structurally — and so tests assert on a condition instead of
/// on wording.
#[derive(Debug, Clone, PartialEq)]
pub enum Warning {
    /// Flags for the retired sink, accepted and ignored.
    RetiredFlags(Vec<&'static str>),
    /// A config key no section defines.
    UnknownKey(String),
    /// Neither a CAN interface nor an ingest listener.
    NoCaptureSource,
    /// Capturing, but with nowhere to archive to.
    NoForwardSink,
    /// Forwarding, but with no credential to authenticate with.
    ForwardKeyMissing,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RetiredFlags(v) => {
                write!(
                    f,
                    "ignoring flags for the retired PostgreSQL sink: {}",
                    v.join(", ")
                )
            }
            Self::UnknownKey(k) => write!(f, "unknown config key, ignored: {k}"),
            Self::NoCaptureSource => write!(
                f,
                "no CAN interfaces and no ingest listener: this server will capture nothing"
            ),
            Self::NoForwardSink => write!(
                f,
                "no [forward] gateway configured: frames will be bridged to GVRET clients \
                 but not archived"
            ),
            Self::ForwardKeyMissing => write!(
                f,
                "[forward] has no api_key and WIRETAP_FORWARD_TOKEN is unset: the gateway \
                 will reject this server"
            ),
        }
    }
}

/// What resolving produced.
#[derive(Debug)]
pub struct Resolved {
    pub settings: Settings,
    pub warnings: Vec<Warning>,
}

/// The credentials the server takes from the environment when the config does
/// not supply them. Named here so the variables exist in one greppable place.
#[derive(Debug, Default)]
pub struct Secrets {
    pub ingest_token: Option<Secret>,
    pub forward_api_key: Option<Secret>,
}

impl Secrets {
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok().map(Secret::new);
        Self {
            ingest_token: get("WIRETAP_INGEST_TOKEN"),
            forward_api_key: get("WIRETAP_FORWARD_TOKEN"),
        }
    }
}

/// Read and parse a config file.
///
/// Lives here rather than in `main` so a future config reload over the control
/// socket takes the same path, and so the failure modes are testable.
pub fn load(path: &str) -> Result<FileConfig, SettingsError> {
    let text = std::fs::read_to_string(path).map_err(|e| SettingsError::Io {
        path: path.to_string(),
        err: e.to_string(),
    })?;
    FileConfig::parse(&text).map_err(SettingsError::Parse)
}

impl Settings {
    /// Apply flags, then the file over them, then the environment for secrets.
    pub fn resolve(
        cli: &Cli,
        file: Option<&FileConfig>,
        secrets: &Secrets,
    ) -> Result<Resolved, SettingsError> {
        // Refused before anything else: with the retired sink on, no other
        // setting can make the result correct.
        if cli.pg_enable || file.is_some_and(FileConfig::retired_postgres_sink) {
            return Err(SettingsError::RetiredPostgresSink);
        }

        let mut warnings = Vec::new();
        let retired = cli.retired_flags_used();
        if !retired.is_empty() {
            warnings.push(Warning::RetiredFlags(retired));
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
            log_level: parse_log_level(&cli.log_level)?,
            stats_interval: cli.stats_interval,
            ingest: cli.ingest_enable.then(|| Ingest::from_cli(cli)),
            forward: cli.forward_enable.then(|| Forward::from_cli(cli)),
        };

        if let Some(f) = file {
            warnings.extend(f.unknown_keys().into_iter().map(Warning::UnknownKey));
            s.apply_file(cli, f)?;
        }

        // `--pg-dir` last, and so above even the file: the Python resolved
        // `args.pg_dir or args.default_dir` in `main`, after the config merge
        // had already written `[server].default_dir` over the flag. It tags
        // both the archive and the frames handed to GVRET clients.
        if let Some(d) = &cli.pg_dir {
            s.default_dir = parse_direction(d)?;
        }

        // Secrets last, and only to fill a gap: an explicitly configured value
        // beats the environment, so a unit file's EnvironmentFile cannot
        // silently override what an operator wrote down.
        if let Some(i) = s.ingest.as_mut() {
            if i.token.is_empty() {
                if let Some(t) = &secrets.ingest_token {
                    i.token = t.clone();
                }
            }
        }
        if let Some(fwd) = s.forward.as_mut() {
            if fwd.api_key.is_empty() {
                if let Some(k) = &secrets.forward_api_key {
                    fwd.api_key = k.clone();
                }
            }
        }

        if s.ifaces.is_empty() && s.ingest.is_none() {
            warnings.push(Warning::NoCaptureSource);
        }
        match &s.forward {
            None => warnings.push(Warning::NoForwardSink),
            Some(f) if f.api_key.is_empty() => warnings.push(Warning::ForwardKeyMissing),
            Some(_) => {}
        }

        Ok(Resolved {
            settings: s,
            warnings,
        })
    }

    /// File values over flag values, matching `apply_config_overrides`.
    fn apply_file(&mut self, cli: &Cli, f: &FileConfig) -> Result<(), SettingsError> {
        let srv = &f.server;
        over(&mut self.ifaces, srv.iface.as_deref().map(parse_ifaces));
        over(&mut self.host, srv.host.clone());
        over(&mut self.port, srv.port);
        over(&mut self.bus_offset, srv.bus_offset);
        over(&mut self.echo_console, srv.echo_console);
        over(&mut self.colour, srv.colour);
        over(&mut self.can_fd, srv.can_fd);
        if let Some(v) = &srv.default_dir {
            self.default_dir = parse_direction(v)?;
        }

        if let Some(v) = &f.logging.level {
            self.log_level = parse_log_level(v)?;
        }
        over(&mut self.stats_interval, f.logging.stats_interval);

        // A section turns its listener on, but its other keys apply whether or
        // not it did — so a file can carry settings for a listener the command
        // line enables, which is how the Python behaved.
        if f.ingest.enable == Some(true) || self.ingest.is_some() {
            let i = self.ingest.get_or_insert_with(|| Ingest::from_cli(cli));
            over(&mut i.host, f.ingest.host.clone());
            over(&mut i.port, f.ingest.port);
            over(&mut i.token, f.ingest.token.clone().map(Secret::new));
            over(&mut i.keepalive_secs, f.ingest.keepalive_secs);
            over(&mut i.max_batch_frames, f.ingest.max_batch_frames);
        }

        if f.forward.enable == Some(true) || self.forward.is_some() {
            let fwd = self.forward.get_or_insert_with(|| Forward::from_cli(cli));
            over(&mut fwd.host, f.forward.host.clone());
            over(&mut fwd.port, f.forward.port);
            over(&mut fwd.api_key, f.forward.api_key.clone().map(Secret::new));
            over(&mut fwd.database, f.forward.database.clone());
        }
        Ok(())
    }

    /// Label/value pairs for `--check-config`. Secrets render through
    /// [`Secret`]'s redacting `Display`, so this cannot echo one.
    fn rows(&self) -> Vec<(&'static str, String)> {
        let mut r = vec![
            (
                "interfaces",
                if self.ifaces.is_empty() {
                    "(none)".into()
                } else {
                    self.ifaces.join(", ")
                },
            ),
            ("gvret listen", format!("{}:{}", self.host, self.port)),
            ("bus offset", self.bus_offset.to_string()),
            ("can fd", self.can_fd.to_string()),
            ("direction", self.default_dir.to_string()),
            (
                "console echo",
                format!("{} (colour {})", self.echo_console, self.colour),
            ),
            ("log level", self.log_level.as_str().into()),
            ("stats interval", format!("{}s", self.stats_interval)),
        ];
        match &self.ingest {
            Some(i) => {
                r.push(("ingest listen", format!("{}:{}", i.host, i.port)));
                r.push((
                    "ingest auth",
                    if i.token.is_empty() {
                        "disabled".into()
                    } else {
                        i.token.to_string()
                    },
                ));
                r.push((
                    "ingest limits",
                    format!(
                        "{} frames/batch, keepalive {}s",
                        i.max_batch_frames, i.keepalive_secs
                    ),
                ));
            }
            None => r.push(("ingest listen", "disabled".into())),
        }
        match &self.forward {
            Some(f) => {
                r.push(("forward to", format!("{}:{}", f.host, f.port)));
                r.push((
                    "forward db",
                    if f.database.is_empty() {
                        "(gateway default)".into()
                    } else {
                        f.database.clone()
                    },
                ));
                r.push((
                    "forward auth",
                    if f.api_key.is_empty() {
                        "MISSING".into()
                    } else {
                        f.api_key.to_string()
                    },
                ));
            }
            None => r.push((
                "forward to",
                "disabled — frames will not be archived".into(),
            )),
        }
        r
    }
}

impl fmt::Display for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (label, value) in self.rows() {
            writeln!(f, "{label:<16}{value}")?;
        }
        Ok(())
    }
}

fn parse_direction(s: &str) -> Result<Direction, SettingsError> {
    s.parse()
        .map_err(|()| SettingsError::BadDirection(s.to_string()))
}

fn parse_log_level(s: &str) -> Result<LogLevel, SettingsError> {
    s.parse()
        .map_err(|()| SettingsError::BadLogLevel(s.to_string()))
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

    fn resolve(args: &[&str], toml: Option<&str>) -> Result<Resolved, SettingsError> {
        let parsed = toml.map(|t| FileConfig::parse(t).expect("test config parses"));
        Settings::resolve(&cli(args), parsed.as_ref(), &Secrets::default())
    }

    #[test]
    fn flags_alone_resolve() {
        let r = resolve(&["-i", "can0,can1", "-p", "2323"], None).unwrap();
        assert_eq!(r.settings.ifaces, ["can0", "can1"]);
        assert_eq!(r.settings.port, 2323);
        assert_eq!(r.settings.default_dir, Direction::Rx);
    }

    /// The precedence deployed units depend on, and that the shipped config
    /// file's own header gets backwards.
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
            !r.warnings
                .iter()
                .any(|w| matches!(w, Warning::UnknownKey(_))),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn retired_flags_warn_without_failing() {
        let r = resolve(&["--pg-write-mode", "copy", "--pg-dsn", "x"], None).unwrap();
        assert!(
            r.warnings
                .contains(&Warning::RetiredFlags(vec!["--pg-dsn", "--pg-write-mode"])),
            "{:?}",
            r.warnings
        );
    }

    /// The batcher flags outlived the sink they are named after: the Python's
    /// `ForwardSink` inherits `PostgresWriter`'s queue and disk cache and is
    /// handed exactly these. Calling them retired would tell an operator their
    /// queue size had stopped applying, which is the opposite of true.
    #[test]
    fn the_batcher_flags_are_not_retired() {
        let r = resolve(
            &[
                "--pg-batch-size",
                "500",
                "--pg-queue-size",
                "200000",
                "--pg-cache-path",
                "/mnt/cache.db",
            ],
            None,
        )
        .unwrap();
        assert!(
            !r.warnings
                .iter()
                .any(|w| matches!(w, Warning::RetiredFlags(_))),
            "{:?}",
            r.warnings
        );
    }

    /// `--pg-dir` sets the direction every frame is tagged with, above both
    /// `--default-dir` and the config file — the order `main` resolved it in.
    #[test]
    fn pg_dir_overrides_the_direction_from_either_source() {
        let r = resolve(&["--default-dir", "rx", "--pg-dir", "tx"], None).unwrap();
        assert_eq!(r.settings.default_dir, Direction::Tx);

        let r = resolve(
            &["--pg-dir", "tx"],
            Some("[server]\ndefault_dir = \"rx\"\n"),
        )
        .unwrap();
        assert_eq!(r.settings.default_dir, Direction::Tx, "above the file too");

        // And it is validated, where the Python would have written the string
        // straight into the archive's `dir` column.
        assert_eq!(
            resolve(&["--pg-dir", "sideways"], None).unwrap_err(),
            SettingsError::BadDirection("sideways".into())
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
            (f.host.as_str(), f.port, f.api_key.expose()),
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
        let env = Secrets {
            forward_api_key: Some(Secret::new("from-env")),
            ingest_token: None,
        };
        let r = Settings::resolve(&cli(&["--forward-enable"]), None, &env).unwrap();
        assert_eq!(r.settings.forward.unwrap().api_key.expose(), "from-env");

        // An explicit value wins: an EnvironmentFile must not quietly replace
        // what an operator wrote in the config.
        let f = FileConfig::parse("[forward]\nenable = true\napi_key = \"from-file\"\n").unwrap();
        let r = Settings::resolve(&cli(&[]), Some(&f), &env).unwrap();
        assert_eq!(r.settings.forward.unwrap().api_key.expose(), "from-file");
    }

    #[test]
    fn unknown_config_keys_warn() {
        let r = resolve(&[], Some("[server]\nnonsense = 1\n")).unwrap();
        assert!(
            r.warnings
                .contains(&Warning::UnknownKey("server.nonsense".into())),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn capturing_nothing_is_worth_saying_out_loud() {
        let r = resolve(&["-i", ""], None).unwrap();
        assert!(
            r.warnings.contains(&Warning::NoCaptureSource),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn archiving_nowhere_is_worth_saying_out_loud() {
        let r = resolve(&[], None).unwrap();
        assert!(
            r.warnings.contains(&Warning::NoForwardSink),
            "{:?}",
            r.warnings
        );

        // Forwarding with no credential fails at the gateway, so say it here.
        let r = resolve(&["--forward-enable"], None).unwrap();
        assert!(
            r.warnings.contains(&Warning::ForwardKeyMissing),
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

    /// The flag is validated by clap; the file was not validated at all until
    /// the level became a type.
    #[test]
    fn a_bad_log_level_is_refused_from_the_file_too() {
        assert_eq!(
            resolve(&[], Some("[logging]\nlevel = \"TRACE\"\n")).unwrap_err(),
            SettingsError::BadLogLevel("TRACE".into())
        );
        assert_eq!(
            resolve(&[], Some("[logging]\nlevel = \"warning\"\n"))
                .unwrap()
                .settings
                .log_level,
            LogLevel::Warning
        );
    }

    /// The shipped reference config must resolve, and land on the values it
    /// documents rather than the flag defaults.
    #[test]
    fn the_shipped_reference_config_resolves() {
        const REFERENCE: &str = include_str!("../../../tools/oracle/wiretap-server.toml");
        let r = resolve(&[], Some(REFERENCE)).unwrap();
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
    fn nothing_that_prints_settings_can_echo_a_secret() {
        let f = FileConfig::parse(
            "[forward]\nenable = true\napi_key = \"super-secret\"\n\
             [ingest]\nenable = true\ntoken = \"also-secret\"\n",
        )
        .unwrap();
        let r = Settings::resolve(&cli(&[]), Some(&f), &Secrets::default()).unwrap();

        let shown = r.settings.to_string();
        assert!(!shown.contains("super-secret"), "{shown}");
        assert!(!shown.contains("also-secret"), "{shown}");

        // The one that used to leak: a derived Debug anywhere in the tree.
        let debugged = format!("{:?}", r);
        assert!(!debugged.contains("super-secret"), "{debugged}");
        assert!(!debugged.contains("also-secret"), "{debugged}");
    }

    #[test]
    fn a_missing_config_file_names_the_path() {
        let err = load("/nonexistent/wiretap.toml").unwrap_err();
        assert!(matches!(err, SettingsError::Io { .. }));
        assert!(
            err.to_string().contains("/nonexistent/wiretap.toml"),
            "{err}"
        );
    }
}
