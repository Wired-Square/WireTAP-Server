//! The settings a UI writes: `ui.toml`, beneath the config file and the flags,
//! so whatever an administrator wrote down has the final say.
//!
//! Only this daemon writes it, through the control socket, and only with what
//! a UI may change. Nothing here can make the box transmit: a device is CAN and
//! passive, and the schema has nowhere to say otherwise.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use wiretap_model::config::{DeviceSection, FileConfig};
use wslib_secret::Secret;

use crate::cli::Cli;
use crate::pipeline;
use crate::settings::{Env, Resolved, Settings, SettingsError, Warning};

/// Every scalar a UI may set: its section and key, the same in `ui.toml` and
/// the config file, and the [`Cli`] field whose flag also sets it.
macro_rules! scalar_settings {
    ($apply:ident) => {
        $apply!(forward, enable, forward_enable);
        $apply!(forward, host, forward_host);
        $apply!(forward, port, forward_port);
        $apply!(forward, database, forward_database);
        $apply!(forward, batch_size, pg_batch_size);
        $apply!(forward, flush_interval, pg_flush_interval);
        $apply!(forward, queue_size, pg_queue_size);
        $apply!(forward, cache_max_mb, pg_cache_max_mb);
        $apply!(forward, queue_flush_pct, pg_queue_flush_pct);
        $apply!(ingest, enable, ingest_enable);
        $apply!(ingest, port, ingest_port);
        $apply!(logging, level, log_level);
    };
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// `None` leaves the device set to the defaults; empty captures nothing
    /// locally.
    pub devices: Option<Vec<UiDevice>>,
    pub forward: UiForward,
    pub ingest: UiIngest,
    pub logging: UiLogging,
}

/// A CAN interface, always passive.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiDevice {
    pub interface: String,
    pub fd: Option<bool>,
    pub database: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiForward {
    pub enable: Option<bool>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub api_key: Option<Secret>,
    pub database: Option<String>,
    pub batch_size: Option<usize>,
    pub flush_interval: Option<f64>,
    pub queue_size: Option<usize>,
    pub cache_max_mb: Option<u64>,
    pub queue_flush_pct: Option<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiIngest {
    pub enable: Option<bool>,
    pub port: Option<u16>,
    pub token: Option<Secret>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiLogging {
    pub level: Option<String>,
}

impl UiDevice {
    /// As a `[[device]]` table. Refuses a name no network interface can have,
    /// which is also every serial device path.
    pub fn table(&self) -> Result<DeviceSection, String> {
        let name = &self.interface;
        // IFNAMSIZ, less the terminating nul.
        if name.is_empty()
            || name.len() > 15
            || name.contains(['/', ':'])
            || name.contains(char::is_whitespace)
        {
            return Err(format!("{name:?} is not a CAN interface name"));
        }
        Ok(DeviceSection {
            kind: Some("can".into()),
            interface: Some(name.clone()),
            mode: Some("passive".into()),
            fd: self.fd,
            database: self.database.clone(),
            ..DeviceSection::default()
        })
    }
}

impl UiConfig {
    /// `cli` with this layer's values under every flag that was not given.
    pub fn beneath(&self, cli: &Cli) -> Cli {
        let mut c = cli.clone();
        macro_rules! fill {
            ($section:ident, $key:ident, $flag:ident) => {
                if let Some(v) = &self.$section.$key {
                    if !cli.given(stringify!($flag)) {
                        c.$flag = v.clone();
                    }
                }
            };
        }
        scalar_settings!(fill);
        c
    }
}

/// Shown only as set or unset.
const SECRETS: [&str; 2] = ["forward.api_key", "ingest.token"];

/// Every key a UI may set, as `GET` and `PUT /v1/settings` name them.
pub fn keys() -> Vec<&'static str> {
    let mut keys = vec!["devices"];
    keys.extend(SECRETS);
    macro_rules! key {
        ($section:ident, $key:ident, $flag:ident) => {
            keys.push(concat!(stringify!($section), ".", stringify!($key)));
        };
    }
    scalar_settings!(key);
    keys
}

/// Where `ui.toml` lives: `--ui-config`, else the state directory.
pub fn path(cli: &Cli, env: &Env) -> Option<PathBuf> {
    cli.ui_config
        .clone()
        .map(PathBuf::from)
        .or_else(|| env.state_dir.as_ref().map(|d| Path::new(d).join("ui.toml")))
}

/// Read `ui.toml`. Absent is empty.
pub fn load(path: &Path) -> Result<UiConfig, String> {
    match fs::read_to_string(path) {
        // Not toml's Display, which quotes the line: it may hold a secret, and
        // this reaches the log and the control socket.
        Ok(text) => toml::from_str(&text).map_err(|e| {
            let line = e.span().map_or(String::new(), |s| {
                let n = text.bytes().take(s.start).filter(|&b| b == b'\n').count();
                format!(" line {}", n + 1)
            });
            format!("{}{line}: {}", path.display(), e.message())
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(UiConfig::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

const HEADER: &str = "# Written by wiretap-server for its UI. Set a value in the config file\n\
                      # instead to pin it: anything set there wins over this file.\n";

/// Replace `ui.toml` whole: a temporary file beside it, synced, renamed over
/// it, and the directory synced so the rename outlives a power cut.
pub fn write(path: &Path, ui: &UiConfig) -> io::Result<()> {
    let text = toml::to_string(ui).map_err(io::Error::other)?;
    let tmp = path.with_extension("toml.tmp");
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(HEADER.as_bytes())?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    File::open(dir.unwrap_or(Path::new(".")))?.sync_all()
}

/// `current` with `put`'s keys set, or cleared where they are `null`.
pub fn merge(current: &UiConfig, put: &Map<String, Value>) -> Result<UiConfig, String> {
    let mut doc = serde_json::to_value(current).map_err(|e| e.to_string())?;
    for (key, value) in put {
        let slot = doc
            .pointer_mut(&format!("/{}", key.replace('.', "/")))
            .ok_or_else(|| format!("{key} is not a UI setting"))?;
        *slot = value.clone();
    }
    serde_json::from_value(doc).map_err(|e| e.to_string())
}

/// What a start runs on. A UI layer that cannot be read, or that would stop
/// the start, is left out with a warning: the UI's only way to repair it is the
/// control socket, which a daemon that will not start never opens.
pub fn resolve_at_start(
    cli: &Cli,
    file: Option<&FileConfig>,
    path: Option<&Path>,
    env: &Env,
) -> Result<(Resolved, UiConfig), SettingsError> {
    let ui = match path.map(load).transpose() {
        Ok(ui) => ui.unwrap_or_default(),
        Err(why) => {
            let mut r = Settings::resolve(cli, file, None, env)?;
            r.warnings.push(Warning::UiIgnored(why));
            return Ok((r, UiConfig::default()));
        }
    };
    let with = Settings::resolve(cli, file, Some(&ui), env);
    let why = match &with {
        Ok(r) => pipeline::check(&r.settings).err().map(|e| e.to_string()),
        Err(e) => Some(e.to_string()),
    };
    let Some(why) = why else {
        return Ok((with?, ui));
    };
    let mut without = Settings::resolve(cli, file, None, env)?;
    if pipeline::check(&without.settings).is_err() {
        if let Ok(with) = with {
            return Ok((with, ui));
        }
    }
    let path = path.map_or(String::new(), |p| format!("{}: ", p.display()));
    without
        .warnings
        .push(Warning::UiIgnored(format!("{path}{why}")));
    Ok((without, UiConfig::default()))
}

/// Which layer a setting's value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    Ui,
    /// The admin's config file.
    File,
    /// An environment variable, by name.
    Env(&'static str),
    /// A flag, by its [`Cli`] field.
    Flag(&'static str),
}

impl Source {
    /// Set somewhere a UI cannot change.
    pub fn pinned(self) -> bool {
        matches!(self, Self::File | Self::Env(_) | Self::Flag(_))
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Ui => "ui",
            Self::File => "file",
            Self::Env(_) => "env",
            Self::Flag(_) => "flag",
        }
    }

    /// Where a pinned value is pinned: `config`, `env X` or `flag --x`.
    pub fn place(self, config: Option<&str>) -> Option<String> {
        match self {
            Self::Default | Self::Ui => None,
            Self::File => config.map(str::to_owned),
            Self::Env(var) => Some(format!("env {var}")),
            Self::Flag(field) => Some(format!("flag --{}", field.replace('_', "-"))),
        }
    }
}

/// The highest layer that set it, in the order resolving applies them.
fn source(file: bool, flag: Option<&'static str>, ui: bool) -> Source {
    match (file, flag, ui) {
        (true, _, _) => Source::File,
        (_, Some(f), _) => Source::Flag(f),
        (_, _, true) => Source::Ui,
        _ => Source::Default,
    }
}

/// One setting a UI may change, as it stands.
#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    pub key: &'static str,
    /// Whether a secret is set, never the secret.
    pub value: Value,
    pub secret: bool,
    pub source: Source,
}

impl Setting {
    pub fn to_json(&self, config: Option<&str>) -> Value {
        let mut v = json!({
            "key": self.key,
            "source": self.source.name(),
            "pinned": self.source.pinned(),
        });
        v[if self.secret { "set" } else { "value" }] = self.value.clone();
        if let Some(place) = self.source.place(config) {
            v["where"] = place.into();
        }
        v
    }

    /// A line of `--check-config`.
    pub fn row(&self, config: Option<&str>) -> String {
        let value = match (&self.value, self.secret) {
            (Value::Bool(set), true) => (if *set { "set" } else { "unset" }).to_owned(),
            (Value::String(s), _) => format!("{s:?}"),
            (v, _) => v.to_string(),
        };
        let from = match (self.source, self.source.place(config)) {
            (Source::File, Some(path)) => format!("file {path}"),
            (_, Some(place)) => place,
            (source, None) => source.name().to_owned(),
        };
        format!("{:<24}{value} ({from})\n", self.key)
    }
}

/// Which layer each setting a UI may change comes from, given the layers.
pub fn sources(
    cli: &Cli,
    file: Option<&FileConfig>,
    ui: &UiConfig,
    env: &Env,
) -> Vec<(&'static str, Source)> {
    let flag = |field: &'static str| cli.given(field).then_some(field);
    let file_devices = file.is_some_and(|f| f.server.iface.is_some() || !f.device.is_empty());
    let mut out = vec![(
        "devices",
        source(file_devices, flag("iface"), ui.devices.is_some()),
    )];

    macro_rules! scalar {
        ($section:ident, $key:ident, $flag:ident) => {
            out.push((
                concat!(stringify!($section), ".", stringify!($key)),
                source(
                    file.is_some_and(|f| f.$section.$key.is_some()),
                    flag(stringify!($flag)),
                    ui.$section.$key.is_some(),
                ),
            ));
        };
    }
    scalar_settings!(scalar);
    // A flag turns a listener on over a file's `enable = false`.
    for (key, in_file, field) in [
        (
            "forward.enable",
            file.and_then(|f| f.forward.enable),
            "forward_enable",
        ),
        (
            "ingest.enable",
            file.and_then(|f| f.ingest.enable),
            "ingest_enable",
        ),
    ] {
        if in_file == Some(false) && cli.given(field) {
            if let Some((_, s)) = out.iter_mut().find(|(k, _)| *k == key) {
                *s = Source::Flag(field);
            }
        }
    }

    // Secrets: an empty value is a gap, and the environment fills one above
    // the UI.
    let secret = |field: &'static str, var, layers: [Option<&str>; 4]| {
        let [in_file, in_flag, from_env, from_ui] =
            layers.map(|l| l.is_some_and(|l| !l.is_empty()));
        match () {
            () if in_file => Source::File,
            () if in_flag => Source::Flag(field),
            () if from_env => Source::Env(var),
            () if from_ui => Source::Ui,
            () => Source::Default,
        }
    };
    out.push((
        "forward.api_key",
        secret(
            "forward_api_key",
            "WIRETAP_FORWARD_TOKEN",
            [
                file.and_then(|f| f.forward.api_key.as_deref()),
                cli.forward_api_key.as_deref(),
                env.forward_api_key.as_ref().map(Secret::expose),
                ui.forward.api_key.as_ref().map(Secret::expose),
            ],
        ),
    ));
    out.push((
        "ingest.token",
        secret(
            "ingest_token",
            "WIRETAP_INGEST_TOKEN",
            [
                file.and_then(|f| f.ingest.token.as_deref()),
                cli.ingest_token.as_deref(),
                env.ingest_token.as_ref().map(Secret::expose),
                ui.ingest.token.as_ref().map(Secret::expose),
            ],
        ),
    ));
    out
}

/// Every setting a UI may change: its value in `settings`, resolved from the
/// layers given, and the layer it came from.
pub fn listing(
    cli: &Cli,
    file: Option<&FileConfig>,
    ui: &UiConfig,
    env: &Env,
    settings: &Settings,
) -> Vec<Setting> {
    let layered = ui.beneath(cli);
    // What resolving decided first, so a lookup finds it ahead of what the
    // layers said.
    let mut values: Vec<(&str, Value)> = vec![
        (
            "devices",
            settings
                .devices
                .iter()
                .map(|d| {
                    json!({
                        "interface": d.interface,
                        "kind": if d.is_can() { "can" } else { "serial" },
                        "mode": d.mode.as_str(),
                        "fd": d.fd(),
                        "database": d.database,
                        "bus": d.bus.0,
                    })
                })
                .collect(),
        ),
        ("forward.enable", settings.forward.is_some().into()),
        ("ingest.enable", settings.ingest.is_some().into()),
        ("logging.level", settings.log_level.as_str().into()),
    ];
    // What the file says, else the flag, else the UI, else the default, as
    // resolving applies them, whether or not the section is on.
    macro_rules! scalar {
        ($section:ident, $key:ident, $flag:ident) => {
            values.push((
                concat!(stringify!($section), ".", stringify!($key)),
                json!(file
                    .and_then(|f| f.$section.$key.clone())
                    .unwrap_or_else(|| layered.$flag.clone())),
            ));
        };
    }
    scalar_settings!(scalar);

    sources(cli, file, ui, env)
        .into_iter()
        .map(|(key, source)| {
            let secret = SECRETS.contains(&key);
            let value = if secret {
                (source != Source::Default).into()
            } else {
                let (_, v) = values
                    .iter()
                    .find(|(k, _)| *k == key)
                    .expect("every key has a value");
                v.clone()
            };
            Setting {
                key,
                value,
                secret,
                source,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogues::tests::TempDir;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_args(["wiretap-server"].iter().chain(args))
    }

    fn ui(toml: &str) -> UiConfig {
        toml::from_str(toml).expect("test ui.toml parses")
    }

    /// Compared as text, since a `Secret` has no `==`.
    fn as_toml(ui: &UiConfig) -> String {
        toml::to_string(ui).unwrap()
    }

    fn is_empty(ui: &UiConfig) -> bool {
        as_toml(ui) == as_toml(&UiConfig::default())
    }

    fn env() -> Env {
        Env {
            hostname: Some("bench".into()),
            ..Env::default()
        }
    }

    fn source_of(sources: &[(&str, Source)], key: &str) -> Source {
        sources.iter().find(|(k, _)| *k == key).unwrap().1
    }

    #[test]
    fn each_setting_says_which_layer_it_came_from_and_where_it_is_pinned() {
        let file = FileConfig::parse("[forward]\nhost = \"file\"\n").unwrap();
        let env = Env {
            ingest_token: Some(Secret::new("from-env")),
            ..env()
        };
        let layers = ui("[forward]\nport = 1\ndatabase = \"site\"\n[logging]\nlevel = \"DEBUG\"\n");
        let s = sources(
            &cli(&["--forward-database", "flag"]),
            Some(&file),
            &layers,
            &env,
        );

        assert_eq!(source_of(&s, "forward.host"), Source::File);
        assert_eq!(
            source_of(&s, "forward.database"),
            Source::Flag("forward_database")
        );
        assert_eq!(source_of(&s, "forward.port"), Source::Ui);
        assert_eq!(source_of(&s, "ingest.port"), Source::Default);
        assert_eq!(
            source_of(&s, "ingest.token"),
            Source::Env("WIRETAP_INGEST_TOKEN")
        );
        assert_eq!(source_of(&s, "devices"), Source::Default);

        let config = Some("/etc/wiretap-server/wiretap-server.toml");
        assert_eq!(Source::File.place(config).as_deref(), config);
        assert_eq!(
            Source::Flag("forward_database").place(config).as_deref(),
            Some("flag --forward-database")
        );
        assert_eq!(
            Source::Env("WIRETAP_INGEST_TOKEN").place(config).as_deref(),
            Some("env WIRETAP_INGEST_TOKEN")
        );
        assert!(Source::File.pinned() && Source::Flag("x").pinned() && Source::Env("x").pinned());
        assert!(!Source::Ui.pinned() && !Source::Default.pinned());
        assert_eq!(keys().len(), s.len(), "one source per key");
    }

    #[test]
    fn a_flag_over_a_files_enable_false_is_the_flags() {
        let file = FileConfig::parse("[ingest]\nenable = false\n").unwrap();
        let s = sources(
            &cli(&["--ingest-enable"]),
            Some(&file),
            &UiConfig::default(),
            &env(),
        );
        assert_eq!(
            source_of(&s, "ingest.enable"),
            Source::Flag("ingest_enable")
        );
        let s = sources(&cli(&[]), Some(&file), &UiConfig::default(), &env());
        assert_eq!(source_of(&s, "ingest.enable"), Source::File);
    }

    #[test]
    fn the_listing_shows_values_but_never_a_secret() {
        let c = cli(&["--forward-api-key", "flag-secret"]);
        let layers =
            ui("[forward]\nenable = true\nhost = \"gw\"\n[ingest]\ntoken = \"ui-secret\"\n");
        let r = Settings::resolve(&c, None, Some(&layers), &env()).unwrap();
        let listing = listing(&c, None, &layers, &env(), &r.settings);
        let shown: String = listing
            .iter()
            .map(|s| s.to_json(None).to_string() + &s.row(None))
            .collect();
        assert!(
            !shown.contains("flag-secret") && !shown.contains("ui-secret"),
            "{shown}"
        );

        let get = |key| listing.iter().find(|s| s.key == key).unwrap().to_json(None);
        assert_eq!(get("forward.host")["value"], "gw");
        assert_eq!(get("forward.host")["source"], "ui");
        assert_eq!(get("forward.api_key")["set"], true);
        assert_eq!(get("forward.api_key")["where"], "flag --forward-api-key");
        assert_eq!(get("ingest.token")["set"], true);
        assert_eq!(get("ingest.token")["pinned"], false);
        assert_eq!(get("ingest.enable")["value"], false, "configured, not on");
        assert_eq!(get("devices")["value"][0]["interface"], "can0");
    }

    #[test]
    fn ui_toml_round_trips_through_an_atomic_write() {
        let dir = TempDir::new("ui-write");
        let path = dir.0.join("ui.toml");
        assert!(is_empty(&load(&path).unwrap()), "absent is empty");

        let written =
            ui("[[devices]]\ninterface = \"can1\"\n[forward]\nenable = true\napi_key = \"k\"\n");
        write(&path, &written).unwrap();
        assert_eq!(as_toml(&load(&path).unwrap()), as_toml(&written));
        assert!(!dir.0.join("ui.toml.tmp").exists());
        let mode = fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
    }

    #[test]
    fn a_ui_toml_with_anything_but_ui_settings_does_not_load() {
        let dir = TempDir::new("ui-strict");
        let path = dir.0.join("ui.toml");
        for bad in [
            "[[devices]]\ninterface = \"can0\"\nmode = \"active\"\n",
            "[test_pattern]\nenable = true\n",
            "[forward]\ncache_path = \"/elsewhere\"\n",
            "[forward\n",
        ] {
            fs::write(&path, bad).unwrap();
            assert!(load(&path).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_ui_toml_that_does_not_load_is_not_quoted() {
        let dir = TempDir::new("ui-unquoted");
        let path = dir.0.join("ui.toml");
        fs::write(&path, "[forward]\napi_key = \"hunter2\nhost = 1\n").unwrap();
        let why = load(&path).unwrap_err();
        assert!(!why.contains("hunter2"), "{why}");
        assert!(why.contains("line 2"), "{why}");

        fs::write(&path, "[forward]\nunknown = 1\n").unwrap();
        let why = load(&path).unwrap_err();
        assert!(why.contains("line 2: unknown field"), "{why}");

        fs::write(&path, "[forward]\napi_key = 31337\n").unwrap();
        let why = load(&path).unwrap_err();
        assert!(!why.contains("31337"), "{why}");
        let put = json!({"ingest.token": 31337});
        let why = merge(&UiConfig::default(), put.as_object().unwrap()).unwrap_err();
        assert!(!why.contains("31337"), "{why}");
    }

    #[test]
    fn a_change_sets_and_clears_keys() {
        let current = ui("[forward]\nhost = \"old\"\nport = 1\n");
        let put = |v: Value| merge(&current, v.as_object().unwrap());
        let next =
            put(json!({"forward.host": "new", "forward.port": null, "devices": []})).unwrap();
        assert_eq!(next.forward.host.as_deref(), Some("new"));
        assert_eq!(next.forward.port, None, "null clears");
        assert_eq!(next.devices, Some(vec![]));
        assert!(
            put(json!({"forward.port": "eighty"})).is_err(),
            "a wrong type"
        );
        assert!(put(json!({"devices": [{"interface": "can0", "mode": "active"}]})).is_err());
    }

    fn ignored(r: &Resolved) -> bool {
        r.warnings
            .iter()
            .any(|w| matches!(w, Warning::UiIgnored(_)))
    }

    #[test]
    fn a_ui_toml_that_cannot_be_read_or_resolved_is_left_out() {
        let dir = TempDir::new("ui-start");
        let path = dir.0.join("ui.toml");
        let c = cli(&["-i", "", "--ingest-enable", "--forward-enable"]);
        let start = || resolve_at_start(&c, None, Some(&path), &env()).unwrap();

        fs::write(&path, "[forward\n").unwrap();
        let (r, applied) = start();
        assert!(is_empty(&applied), "{applied:?}");
        assert!(ignored(&r), "{:?}", r.warnings);

        fs::write(&path, "[forward]\ndatabase = \"Not A Name\"\n").unwrap();
        let (r, applied) = start();
        assert!(is_empty(&applied), "{applied:?}");
        assert_eq!(r.settings.forward.as_ref().unwrap().database, "");
        assert!(ignored(&r));

        fs::write(&path, "[logging]\nlevel = \"DEBUG\"\n").unwrap();
        let (r, applied) = start();
        assert_eq!(applied.logging.level.as_deref(), Some("DEBUG"));
        assert_eq!(r.settings.log_level, crate::settings::LogLevel::Debug);
        assert!(!ignored(&r));
    }

    /// Off Linux the defaults' `can0` cannot start either, so there is no
    /// start without the UI to fall back to.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_ui_toml_that_would_stop_the_start_is_left_out() {
        let dir = TempDir::new("ui-check");
        let path = dir.0.join("ui.toml");
        fs::write(&path, "[ingest]\nenable = true\n").unwrap();
        let (r, applied) = resolve_at_start(&cli(&[]), None, Some(&path), &env()).unwrap();
        assert!(is_empty(&applied), "{applied:?}");
        assert!(r.settings.ingest.is_none());
        assert!(r
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::UiIgnored(why) if why.contains("ingest"))));
    }
}
