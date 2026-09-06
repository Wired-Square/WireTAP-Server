//! Resolving the effective configuration from flags, the config file and the
//! environment.
//!
//! **The file overrides the flags**, not the other way round. That is what the
//! Python did — `apply_config_overrides` writes file values over the parsed
//! arguments — and deployed units pass both, so reversing it would silently
//! change what running systems do. The Python's own
//! `tools/oracle/wiretap-server.toml` header claims the opposite;
//! `packaging/wiretap-server.toml`, the one this repo ships, states it
//! correctly. `--check-config` exists so the result is inspectable rather than
//! argued about either way.

use std::fmt;
use std::path::{Path, PathBuf};

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
    pub batching: Batching,
}

/// How frames are grouped on the way to the gateway, and where they wait when
/// it is not there.
///
/// One struct because it is one mechanism: the batcher owns the queue, decides
/// when a batch goes, and owns the cache it spills to. The Python spelled the
/// same settings `--pg-*`, which is why they are still called that on the
/// command line.
#[derive(Debug, Clone)]
pub struct Batching {
    pub size: usize,
    pub flush_interval: f64,
    pub queue_size: usize,
    pub cache_path: PathBuf,
    /// How [`cache_path`] was arrived at, when that is worth saying out loud.
    pub cache_origin: CacheOrigin,
    pub cache_max_mb: u64,
    pub queue_flush_pct: u8,
    /// A cache an older install left in `$HOME`, to be taken over at startup,
    /// or `None` when there is nothing to adopt.
    ///
    /// Resolved here rather than looked for later so `--check-config` can say
    /// whether an upgrade is about to move something, and so the one filename
    /// this and [`cache_path`] have to agree on is written once.
    pub legacy_cache_path: Option<PathBuf>,
}

/// Why [`cache_path`] answered as it did, when that is worth saying out loud,
/// and `None` when the path speaks for itself.
///
/// `--check-config` is documented as being run with `STATE_DIRECTORY` set,
/// because that is what the packaged unit sets. Run without it the answer
/// silently becomes a `$HOME` fallback the unit never opens — and no `adopt on
/// start` row appears either, on exactly the path where an upgrade has just
/// staged a cache to be adopted. A clause on the end of the line is the
/// difference between a report that is quietly wrong and one that says which
/// question it answered.
type CacheOrigin = Option<&'static str>;

/// The Python's default, and what an existing Pi has a populated copy of.
const LEGACY_CACHE_FILE: &str = ".wiretap-server-cache.db";

/// Where `debian/postinst` leaves a cache it found under someone's home
/// directory, for this process to adopt.
///
/// The packaging cannot simply leave it where it was: the unit sets
/// `ProtectHome=`, so the derivation from `$HOME` below sees nothing at all
/// under a packaged install. It cannot merge one either — that is
/// [`crate::cache::SqliteCache::adopt`]'s job, and doing it in shell would be a
/// second, untested implementation of an ordered drain. So it moves the file
/// here and stops, and the daemon adopts it exactly as it adopts any other.
const STAGED_CACHE_FILE: &str = "adopt.db";

impl Batching {
    /// Flags first; the file overrides them in [`Settings::apply_file`].
    fn from_cli(cli: &Cli, env: &Env) -> Self {
        let (cache_path, cache_origin) = cache_path(cli.pg_cache_path.as_deref(), env);
        Self {
            size: cli.pg_batch_size,
            flush_interval: cli.pg_flush_interval,
            queue_size: cli.pg_queue_size,
            cache_path,
            cache_origin,
            cache_max_mb: cli.pg_cache_max_mb,
            queue_flush_pct: cli.pg_queue_flush_pct,
            // Filled in by `Settings::resolve`, once the file has had its say
            // about where the cache lives.
            legacy_cache_path: None,
        }
    }
}

/// The cache an older install would have written, if it is not the one in use
/// and it is actually there.
///
/// Staged before `$HOME`, because a packaged install can only produce the first
/// and a hand-run one can only produce the second — under the unit `$HOME` is
/// hidden, and off it nothing stages anything.
///
/// `None` once it has been adopted, so the answer changes across a restart —
/// which is what makes it worth showing in `--check-config`.
fn legacy_cache_path(in_use: &Path, env: &Env) -> Option<PathBuf> {
    let staged = env
        .state_dir
        .as_deref()
        .map(|dir| Path::new(dir).join(STAGED_CACHE_FILE));
    let in_home = env
        .home
        .as_deref()
        .map(|home| Path::new(home).join(LEGACY_CACHE_FILE));
    staged
        .into_iter()
        .chain(in_home)
        .find(|p| p != in_use && p.exists())
}

/// Where the disk cache lives, given whatever was configured.
///
/// `$STATE_DIRECTORY` before `$HOME` is a deliberate move: the Python opened
/// `~/.wiretap-server-cache.db` unconditionally, and its own unit set
/// `ProtectHome=read-only`, so anyone running that unit with archiving on had
/// already edited it or was not using it. Under `StateDirectory=` the default
/// lands somewhere the unit can actually write. An existing `$HOME` cache is
/// still found when there is no state directory, so a hand-run upgrade does not
/// strand one.
///
/// The packaged unit goes further and sets `ProtectHome=true`, which hides
/// `$HOME` entirely — so under it the adoption below can never fire, and
/// `debian/postinst` moves a legacy cache across before the daemon first runs.
///
/// The [`CacheOrigin`] comes back with the path because the path alone does not
/// say whether it is the one the daemon will use.
fn cache_path(configured: Option<&str>, env: &Env) -> (PathBuf, CacheOrigin) {
    if let Some(p) = configured.or(env.cache_path.as_deref()) {
        // Nothing to explain: someone named this path.
        return (PathBuf::from(p), None);
    }
    match (&env.state_dir, &env.home) {
        (Some(state), _) => (Path::new(state).join("cache.db"), None),
        (None, Some(home)) => (
            Path::new(home).join(LEGACY_CACHE_FILE),
            Some("from $HOME; the packaged unit sets STATE_DIRECTORY instead"),
        ),
        // Neither: relative to the working directory, which is at least
        // somewhere a hand-run server can write.
        (None, None) => (
            PathBuf::from(LEGACY_CACHE_FILE),
            Some("relative to the working directory; no $STATE_DIRECTORY, no $HOME"),
        ),
    }
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

/// The Test Pattern responder: answers a link validation run on the bus.
///
/// **Off unless asked for.** Everything else this server does is read-only on
/// the bus; this transmits, unbidden, in reply to whatever asks. On a
/// production capture bus that is a hazard rather than a feature, so there is
/// no default that arms it and the startup log names every interface it armed.
///
/// No `Env` here. The other two optional sections carry a secret that
/// `resolve` fills in from the environment afterwards; a responder has none.
#[derive(Debug, Clone)]
pub struct TestPattern {
    /// Interfaces that answer, by name. Empty arms all of them.
    pub ifaces: Vec<String>,
}

impl TestPattern {
    fn from_cli(cli: &Cli) -> Self {
        Self {
            ifaces: parse_ifaces(&cli.test_pattern_ifaces),
        }
    }
}

impl Forward {
    fn from_cli(cli: &Cli, env: &Env) -> Self {
        Self {
            host: cli.forward_host.clone(),
            port: cli.forward_port,
            api_key: Secret::new(cli.forward_api_key.clone().unwrap_or_default()),
            database: cli.forward_database.clone(),
            batching: Batching::from_cli(cli, env),
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
    pub test_pattern: Option<TestPattern>,
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
    /// `--test-pattern-enable` was given and the config file disarmed it.
    ///
    /// Its own variant because the packaged config ships `enable = false` for
    /// three sections that look identical, and this is the only one where that
    /// beats the flag. Without a line saying so, the flag simply does nothing.
    TestPatternDisarmedByFile,
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
            Self::TestPatternDisarmedByFile => write!(
                f,
                "--test-pattern-enable was overridden by [test_pattern] enable = false in the \
                 config file: the responder is disarmed"
            ),
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

/// What the server reads from the environment: credentials the config does not
/// supply, and the paths systemd hands a unit.
///
/// Read once and passed in, so resolving is a pure function of its inputs and a
/// test never has to mutate the process environment.
#[derive(Debug, Default)]
pub struct Env {
    pub ingest_token: Option<Secret>,
    pub forward_api_key: Option<Secret>,
    /// `PG_CACHE_PATH`, kept under its Python name because deployed units set
    /// it.
    pub cache_path: Option<String>,
    /// `StateDirectory=` in the unit; `/var/lib/wiretap-server` in the package.
    pub state_dir: Option<String>,
    pub home: Option<String>,
}

impl Env {
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok();
        Self {
            ingest_token: get("WIRETAP_INGEST_TOKEN").map(Secret::new),
            forward_api_key: get("WIRETAP_FORWARD_TOKEN").map(Secret::new),
            cache_path: get("PG_CACHE_PATH"),
            state_dir: get("STATE_DIRECTORY"),
            home: get("HOME"),
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
        env: &Env,
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
            forward: cli.forward_enable.then(|| Forward::from_cli(cli, env)),
            test_pattern: cli.test_pattern_enable.then(|| TestPattern::from_cli(cli)),
        };

        if let Some(f) = file {
            warnings.extend(f.unknown_keys().into_iter().map(Warning::UnknownKey));
            s.apply_file(cli, env, f)?;
        }

        // `--pg-dir` last, and so above even the file: the Python resolved
        // `args.pg_dir or args.default_dir` in `main`, after the config merge
        // had already written `[server].default_dir` over the flag. It tags
        // both the archive and the frames handed to GVRET clients.
        if let Some(d) = &cli.pg_dir {
            s.default_dir = parse_direction(d)?;
        }

        // Env last, and only to fill a gap: an explicitly configured value
        // beats the environment, so a unit file's EnvironmentFile cannot
        // silently override what an operator wrote down.
        if let Some(i) = s.ingest.as_mut() {
            if i.token.is_empty() {
                if let Some(t) = &env.ingest_token {
                    i.token = t.clone();
                }
            }
        }
        if let Some(fwd) = s.forward.as_mut() {
            if fwd.api_key.is_empty() {
                if let Some(k) = &env.forward_api_key {
                    fwd.api_key = k.clone();
                }
            }
            // Last, because it depends on where the cache ended up, and every
            // source has now had its say about that.
            fwd.batching.legacy_cache_path = legacy_cache_path(&fwd.batching.cache_path, env);
        }

        if cli.test_pattern_enable && s.test_pattern.is_none() {
            warnings.push(Warning::TestPatternDisarmedByFile);
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
    fn apply_file(&mut self, cli: &Cli, env: &Env, f: &FileConfig) -> Result<(), SettingsError> {
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

        // `enable = false` **disarms**, where the other two sections let a
        // flag survive it. The file overriding the flags is this module's rule, and
        // this is the one setting where honouring it in the off direction
        // matters: a fleet config that says the responder is disarmed should
        // win over a flag somebody left in a unit file.
        if f.test_pattern.enable == Some(false) {
            self.test_pattern = None;
        } else if f.test_pattern.enable == Some(true) || self.test_pattern.is_some() {
            let tp = self
                .test_pattern
                .get_or_insert_with(|| TestPattern::from_cli(cli));
            over(
                &mut tp.ifaces,
                f.test_pattern.ifaces.as_deref().map(parse_ifaces),
            );
        }

        if f.forward.enable == Some(true) || self.forward.is_some() {
            let fwd = self
                .forward
                .get_or_insert_with(|| Forward::from_cli(cli, env));
            over(&mut fwd.host, f.forward.host.clone());
            over(&mut fwd.port, f.forward.port);
            over(&mut fwd.api_key, f.forward.api_key.clone().map(Secret::new));
            over(&mut fwd.database, f.forward.database.clone());

            let b = &mut fwd.batching;
            over(&mut b.size, f.forward.batch_size);
            over(&mut b.flush_interval, f.forward.flush_interval);
            over(&mut b.queue_size, f.forward.queue_size);
            over(&mut b.cache_max_mb, f.forward.cache_max_mb);
            over(&mut b.queue_flush_pct, f.forward.queue_flush_pct);
            // Re-resolved rather than assigned: an empty `cache_path = ""` in
            // the file has to fall back to the default, not name the working
            // directory.
            if let Some(p) = f.forward.cache_path.as_deref() {
                (b.cache_path, b.cache_origin) = cache_path(Some(p).filter(|p| !p.is_empty()), env);
            }
        }
        Ok(())
    }

    /// Label/value pairs for `--check-config`. Env render through
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
        // Named even when off: `--check-config` is where someone looks to
        // find out what a box is about to do, and this is the only setting
        // that makes it transmit.
        r.push((
            "test pattern",
            match &self.test_pattern {
                None => "disabled".into(),
                Some(tp) => format!(
                    "ARMED, transmits on {}{}",
                    if tp.ifaces.is_empty() {
                        "every interface".to_string()
                    } else {
                        tp.ifaces.join(", ")
                    },
                    if self.can_fd {
                        ""
                    } else {
                        " (classic sweep only; can_fd is off)"
                    }
                ),
            },
        ));
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
                let b = &f.batching;
                r.push((
                    "batching",
                    format!(
                        "{} frames or {}s, queue {} (spill at {}%)",
                        b.size, b.flush_interval, b.queue_size, b.queue_flush_pct
                    ),
                ));
                r.push((
                    "disk cache",
                    match b.cache_origin {
                        Some(origin) => format!(
                            "{} (max {} MB), {origin}",
                            b.cache_path.display(),
                            b.cache_max_mb
                        ),
                        None => format!("{} (max {} MB)", b.cache_path.display(), b.cache_max_mb),
                    },
                ));
                if let Some(legacy) = &b.legacy_cache_path {
                    r.push(("adopt on start", legacy.display().to_string()));
                }
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
        Settings::resolve(&cli(args), parsed.as_ref(), &Env::default())
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
        let env = Env {
            forward_api_key: Some(Secret::new("from-env")),
            ..Env::default()
        };
        let r = Settings::resolve(&cli(&["--forward-enable"]), None, &env).unwrap();
        assert_eq!(r.settings.forward.unwrap().api_key.expose(), "from-env");

        // An explicit value wins: an EnvironmentFile must not quietly replace
        // what an operator wrote in the config.
        let f = FileConfig::parse("[forward]\nenable = true\napi_key = \"from-file\"\n").unwrap();
        let r = Settings::resolve(&cli(&[]), Some(&f), &env).unwrap();
        assert_eq!(r.settings.forward.unwrap().api_key.expose(), "from-file");
    }

    /// The settings that used to live under `[postgres]`, where this server
    /// could never have read them.
    #[test]
    fn the_forward_section_configures_the_batcher() {
        let r = resolve(
            &["--pg-batch-size", "500"],
            Some(
                "[forward]\nenable = true\nbatch_size = 2000\nflush_interval = 2.5\n\
                 queue_size = 200000\ncache_max_mb = 4096\nqueue_flush_pct = 80\n",
            ),
        )
        .unwrap();
        let b = r.settings.forward.expect("enabled").batching;
        assert_eq!(b.size, 2000, "the file wins, as everywhere else");
        assert_eq!(b.flush_interval, 2.5);
        assert_eq!(b.queue_size, 200_000);
        assert_eq!(b.cache_max_mb, 4096);
        assert_eq!(b.queue_flush_pct, 80);

        // Untouched by the file, the flags stand.
        let r = resolve(&["--forward-enable", "--pg-queue-size", "9"], None).unwrap();
        let b = r.settings.forward.unwrap().batching;
        assert_eq!(b.queue_size, 9);
        assert_eq!(b.size, 500, "the Python's default");
    }

    /// A migrated file still carries these under `[postgres]`. They stay
    /// ignored, exactly as the Python ignored them once the sink was off — the
    /// section is retired wholesale, and `--check-config` shows what won.
    #[test]
    fn the_retired_section_does_not_configure_the_batcher() {
        let r = resolve(
            &["--forward-enable"],
            Some("[postgres]\nenable = false\nbatch_size = 2000\ncache_max_mb = 4096\n"),
        )
        .unwrap();
        let b = r.settings.forward.unwrap().batching;
        assert_eq!(b.size, 500);
        assert_eq!(b.cache_max_mb, 1000);
    }

    /// Where the cache lands, in the order the answer is looked for.
    #[test]
    fn the_cache_path_falls_back_through_the_environment() {
        let systemd = Env {
            state_dir: Some("/var/lib/wiretap-server".into()),
            home: Some("/home/pi".into()),
            ..Env::default()
        };
        let by_hand = Env {
            home: Some("/home/pi".into()),
            ..Env::default()
        };

        // A unit with StateDirectory= writes somewhere ProtectHome allows.
        assert_eq!(
            cache_path(None, &systemd),
            (
                PathBuf::from("/var/lib/wiretap-server/cache.db"),
                // Nothing to explain: this is the path the unit gives it.
                None
            )
        );
        // Without one, the Python's path, so an existing cache is still found.
        assert_eq!(
            cache_path(None, &by_hand),
            (
                PathBuf::from("/home/pi/.wiretap-server-cache.db"),
                // And here it is worth saying so: --check-config run without
                // STATE_DIRECTORY reports this, and it is not what runs.
                Some("from $HOME; the packaged unit sets STATE_DIRECTORY instead")
            )
        );
        // PG_CACHE_PATH is what deployed units set, and it beats both.
        let env = Env {
            cache_path: Some("/mnt/usb/cache.db".into()),
            ..systemd
        };
        assert_eq!(
            cache_path(None, &env),
            (PathBuf::from("/mnt/usb/cache.db"), None)
        );
        // And an explicit setting beats the environment.
        assert_eq!(
            cache_path(Some("/explicit.db"), &env),
            (PathBuf::from("/explicit.db"), None)
        );
    }

    /// Which cache gets adopted, and the order that matters: a packaged install
    /// can only ever produce the staged file, because its unit hides `$HOME`
    /// from the daemon entirely.
    #[test]
    fn a_staged_cache_is_adopted_ahead_of_one_in_a_home_directory() {
        let dir = std::env::temp_dir().join(format!("wt-adopt-{}", std::process::id()));
        let home = dir.join("home");
        let state = dir.join("state");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let env = Env {
            state_dir: Some(state.display().to_string()),
            home: Some(home.display().to_string()),
            ..Env::default()
        };
        let in_use = state.join("cache.db");

        // Nothing to adopt is the ordinary case, and it must not invent a path.
        assert_eq!(legacy_cache_path(&in_use, &env), None);

        std::fs::write(home.join(LEGACY_CACHE_FILE), b"").unwrap();
        assert_eq!(
            legacy_cache_path(&in_use, &env),
            Some(home.join(LEGACY_CACHE_FILE)),
            "a hand-run upgrade still finds the Python's"
        );

        // What debian/postinst leaves behind wins: it is there precisely
        // because the daemon could not have seen the one above.
        std::fs::write(state.join(STAGED_CACHE_FILE), b"").unwrap();
        assert_eq!(
            legacy_cache_path(&in_use, &env),
            Some(state.join(STAGED_CACHE_FILE))
        );

        // And a cache is never its own legacy: pointed at the staged file, the
        // answer moves on rather than proposing to adopt what is in use.
        assert_eq!(
            legacy_cache_path(&state.join(STAGED_CACHE_FILE), &env),
            Some(home.join(LEGACY_CACHE_FILE))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_path_is_configurable_from_either_source() {
        let env = Env {
            state_dir: Some("/var/lib/wiretap-server".into()),
            ..Env::default()
        };
        let file = FileConfig::parse("[forward]\nenable = true\ncache_path = \"/from/file.db\"\n")
            .unwrap();
        let r = Settings::resolve(
            &cli(&["--pg-cache-path", "/from/flag.db"]),
            Some(&file),
            &env,
        )
        .unwrap();
        assert_eq!(
            r.settings.forward.unwrap().batching.cache_path,
            Path::new("/from/file.db")
        );

        // An empty value is not a path: it means "wherever the default is".
        let file = FileConfig::parse("[forward]\nenable = true\ncache_path = \"\"\n").unwrap();
        let r = Settings::resolve(&cli(&[]), Some(&file), &env).unwrap();
        assert_eq!(
            r.settings.forward.unwrap().batching.cache_path,
            Path::new("/var/lib/wiretap-server/cache.db")
        );
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

    /// The responder is the only part of this server that transmits without
    /// being asked, so the default matters more than the flag does: a build
    /// that armed it by accident would put frames on a customer's live bus.
    #[test]
    fn the_test_pattern_responder_is_off_unless_asked_for() {
        assert!(resolve(&[], None).unwrap().settings.test_pattern.is_none());
        assert!(
            resolve(&["--can-fd", "--iface", "can0"], None)
                .unwrap()
                .settings
                .test_pattern
                .is_none(),
            "nothing else turns it on, --can-fd least of all"
        );

        let tp = resolve(&["--test-pattern-enable"], None)
            .unwrap()
            .settings
            .test_pattern
            .expect("the flag arms it");
        assert!(tp.ifaces.is_empty(), "and with no names, every interface");

        let tp = resolve(
            &[
                "--test-pattern-enable",
                "--test-pattern-ifaces",
                "can1, can2",
            ],
            None,
        )
        .unwrap()
        .settings
        .test_pattern
        .expect("armed");
        assert_eq!(tp.ifaces, ["can1", "can2"]);
    }

    /// The config file can arm it, aim it, and — unlike the other sections —
    /// turn it off again over a flag.
    #[test]
    fn the_config_file_has_the_last_word_on_the_responder() {
        let tp = resolve(
            &[],
            Some("[test_pattern]\nenable = true\nifaces = \"can1\"\n"),
        )
        .unwrap()
        .settings
        .test_pattern
        .expect("the file arms it with no flag at all");
        assert_eq!(tp.ifaces, ["can1"]);

        let r = resolve(
            &["--test-pattern-enable"],
            Some("[test_pattern]\nenable = false\n"),
        )
        .unwrap();
        assert!(
            r.settings.test_pattern.is_none(),
            "a file that disarms beats a flag that arms"
        );
        // The packaged config ships `enable = false` under three sections that
        // look identical, and this is the only one where it beats the flag. A
        // silently ineffective flag is the trap; the warning is the way out.
        assert!(
            r.warnings.contains(&Warning::TestPatternDisarmedByFile),
            "and says so: {:?}",
            r.warnings
        );

        let tp = resolve(
            &["--test-pattern-enable", "--test-pattern-ifaces", "can0"],
            Some("[test_pattern]\nifaces = \"can2\"\n"),
        )
        .unwrap()
        .settings
        .test_pattern
        .expect("the flag armed it and the file aimed it");
        assert_eq!(tp.ifaces, ["can2"], "the file overrides the flag");
    }

    /// `--check-config` is where an operator finds out what a box is about to
    /// do, and this is the only setting that makes it transmit. The wording is
    /// asserted because "no names means every interface" is a rule nobody would
    /// guess from an empty list.
    #[test]
    fn check_config_says_what_the_responder_will_transmit_on() {
        let row = |args: &[&str]| {
            let s = resolve(args, None).unwrap().settings;
            s.rows()
                .into_iter()
                .find(|(label, _)| *label == "test pattern")
                .expect("a test pattern row, armed or not")
                .1
        };

        assert_eq!(row(&["-i", "can0"]), "disabled");
        assert_eq!(
            row(&["-i", "can0,can1", "--test-pattern-enable", "--can-fd"]),
            "ARMED, transmits on every interface"
        );
        assert_eq!(
            row(&[
                "-i",
                "can0,can1",
                "--test-pattern-enable",
                "--test-pattern-ifaces",
                "can1",
            ]),
            "ARMED, transmits on can1 (classic sweep only; can_fd is off)"
        );
    }

    #[test]
    fn nothing_that_prints_settings_can_echo_a_secret() {
        let f = FileConfig::parse(
            "[forward]\nenable = true\napi_key = \"super-secret\"\n\
             [ingest]\nenable = true\ntoken = \"also-secret\"\n",
        )
        .unwrap();
        let r = Settings::resolve(&cli(&[]), Some(&f), &Env::default()).unwrap();

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
