//! Which catalogue each framed serial line frames with: the gateway's
//! assignment, else the `/etc` `catalog`, else none.
//!
//! Each `[forward]` session reads its lines' assignments from its `HELLO_ACK`,
//! pulls a blob it lacks into `<state dir>/catalogs/<sha>.toml`, and records
//! the assignment in `<state dir>/assignments.json`, so a restart with the
//! gateway down frames as before. A line's taps follow its [`Rules`] on a
//! watch channel.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tracing::warn;
use wiretap_model::{blob_sha1, blob_sha1_hex};

use crate::settings::{Forward, LineCatalogue, Settings};

const ASSIGNMENTS: &str = "assignments.json";

/// What a line frames with, and where that came from.
#[derive(Debug, Clone, PartialEq)]
pub enum Rules {
    Gateway {
        sha: String,
        catalogue: LineCatalogue,
    },
    Etc(LineCatalogue),
    None,
}

impl Rules {
    pub fn catalogue(&self) -> Option<&LineCatalogue> {
        match self {
            Self::Gateway { catalogue, .. } | Self::Etc(catalogue) => Some(catalogue),
            Self::None => None,
        }
    }

    /// `gateway ce013625`, the `/etc` path, or `none`.
    pub fn source(&self) -> String {
        match self {
            Self::Gateway { sha, .. } => format!("gateway {}", &sha[..8]),
            Self::Etc(c) => c.path.clone(),
            Self::None => "none".into(),
        }
    }
}

impl fmt::Display for Rules {
    /// The source, then the catalogue's name and vendor codes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.catalogue() {
            Some(c) => write!(f, "{}, {c}", self.source()),
            None => f.write_str("none"),
        }
    }
}

/// A line's assignment, as one session's `HELLO_ACK` left it.
pub enum Update {
    Assigned {
        sha: String,
        catalogue: LineCatalogue,
    },
    Cleared,
}

struct Line {
    /// Where its framed stream lands: the session forwarding there owns it.
    database: String,
    etc: Option<LineCatalogue>,
    rules: watch::Sender<Rules>,
}

fn fallback(etc: &Option<LineCatalogue>) -> Rules {
    etc.clone().map_or(Rules::None, Rules::Etc)
}

#[derive(Default)]
pub struct Catalogues {
    /// `None` with nothing to forward to, so nothing is ever assigned.
    dir: Option<PathBuf>,
    lines: BTreeMap<String, Line>,
    /// Interface → hex SHA-1, as `assignments.json` holds it.
    assigned: Mutex<BTreeMap<String, String>>,
}

impl Catalogues {
    /// Every framed serial line, starting from what the state directory
    /// remembers.
    pub fn open(settings: &Settings) -> Arc<Self> {
        let lines = settings
            .serial_devices()
            .filter(|(_, s)| s.framing.is_some());
        Self::new(
            settings.forward.as_ref().map(Forward::state_dir),
            lines.map(|(d, s)| (d.interface.clone(), d.database.clone(), s.catalogue.clone())),
        )
    }

    /// `lines` as (interface, framed database, `/etc` catalogue).
    pub fn new(
        dir: Option<PathBuf>,
        lines: impl IntoIterator<Item = (String, String, Option<LineCatalogue>)>,
    ) -> Arc<Self> {
        let mut catalogues = Self {
            assigned: Mutex::new(dir.as_deref().map(remembered).unwrap_or_default()),
            dir,
            ..Self::default()
        };
        for (interface, database, etc) in lines {
            let rules = watch::Sender::new(catalogues.remembered_rules(&interface, &etc));
            let line = Line {
                database,
                etc,
                rules,
            };
            catalogues.lines.insert(interface, line);
        }
        Arc::new(catalogues)
    }

    fn remembered_rules(&self, interface: &str, etc: &Option<LineCatalogue>) -> Rules {
        let assigned = self.assigned.lock().expect("unpoisoned");
        let Some(sha) = assigned.get(interface) else {
            return fallback(etc);
        };
        match self.cached(sha) {
            Ok(catalogue) => Rules::Gateway {
                sha: sha.clone(),
                catalogue,
            },
            Err(why) => {
                let rules = fallback(etc);
                warn!(
                    "{interface}: the gateway's catalogue {sha} is not usable ({why}); \
                     framing with {} until the gateway is reached",
                    rules.source()
                );
                rules
            }
        }
    }

    pub fn subscribe(&self, interface: &str) -> Option<watch::Receiver<Rules>> {
        self.lines.get(interface).map(|l| l.rules.subscribe())
    }

    /// What `interface` frames with now.
    pub fn effective(&self, interface: &str) -> Option<Rules> {
        self.lines.get(interface).map(|l| l.rules.borrow().clone())
    }

    /// The lines whose assignments a session forwarding to `database` keeps.
    pub fn owned_by<'a>(&'a self, database: &'a str) -> impl Iterator<Item = &'a str> {
        self.lines
            .iter()
            .filter(move |(_, l)| l.database == database)
            .map(|(i, _)| i.as_str())
    }

    fn blob_path(&self, sha: &str) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        Some(dir.join("catalogs").join(format!("{sha}.toml")))
    }

    /// The cached blob `sha`, checked and parsed as it was stored.
    pub fn cached(&self, sha: &str) -> Result<LineCatalogue, String> {
        let path = self.blob_path(sha).ok_or("no state directory")?;
        let blob = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        checked(sha, &blob, &path)
    }

    /// Check `blob` against `sha`, parse it, and cache it.
    pub fn store(&self, sha: &str, blob: &[u8]) -> Result<LineCatalogue, String> {
        let path = self.blob_path(sha).ok_or("no state directory")?;
        let catalogue = checked(sha, blob, &path)?;
        write_atomically(&path, blob)?;
        Ok(catalogue)
    }

    /// Take one session's assignments, remember them, and hand any change
    /// to the lines' taps.
    pub fn apply(&self, updates: Vec<(String, Update)>) {
        let mut assigned = self.assigned.lock().expect("unpoisoned");
        let before = assigned.clone();
        for (interface, update) in updates {
            let Some(line) = self.lines.get(&interface) else {
                continue;
            };
            let rules = match update {
                Update::Assigned { sha, catalogue } => {
                    assigned.insert(interface, sha.clone());
                    Rules::Gateway { sha, catalogue }
                }
                Update::Cleared => {
                    assigned.remove(&interface);
                    fallback(&line.etc)
                }
            };
            line.rules.send_if_modified(|current| {
                let changed = *current != rules;
                *current = rules;
                changed
            });
        }
        if *assigned == before {
            return;
        }
        let Some(dir) = &self.dir else { return };
        let json = serde_json::to_vec_pretty(&*assigned).expect("a map of strings");
        if let Err(e) = write_atomically(&dir.join(ASSIGNMENTS), &json) {
            warn!("cannot remember the gateway's catalogue assignments: {e}");
        }
    }
}

fn remembered(dir: &Path) -> BTreeMap<String, String> {
    let path = dir.join(ASSIGNMENTS);
    let Ok(json) = std::fs::read(&path) else {
        return BTreeMap::new();
    };
    serde_json::from_slice(&json).unwrap_or_else(|e| {
        warn!("ignoring {}: {e}", path.display());
        BTreeMap::new()
    })
}

/// The blob's own bytes, as the `/etc` path reads a file: no line ending or
/// encoding is touched, since the SHA-1 is over exactly these.
fn checked(sha: &str, blob: &[u8], path: &Path) -> Result<LineCatalogue, String> {
    let actual = blob_sha1_hex(&blob_sha1(blob));
    if actual != sha {
        return Err(format!("its content hashes to {actual}"));
    }
    let text = std::str::from_utf8(blob).map_err(|e| format!("not UTF-8: {e}"))?;
    LineCatalogue::parse(&path.display().to_string(), text)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let partial = path.with_extension("partial");
    std::fs::write(&partial, bytes).map_err(fail)?;
    std::fs::rename(&partial, path).map_err(fail)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const CATALOGUE: &str =
        "[meta]\nname = \"gateway\"\n[meta.modbus.function_code.0x60]\n\
         lengths = [{ len = { fixed = 11 } }]\n";

    pub(crate) fn sha_of(blob: &[u8]) -> String {
        blob_sha1_hex(&blob_sha1(blob))
    }

    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("wiretap-catalogues-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("a writable temp directory");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn etc() -> LineCatalogue {
        LineCatalogue::parse(
            "/etc/wiretap-server/line.catalog.toml",
            "[meta]\nname = \"etc\"\n",
        )
        .unwrap()
    }

    fn line(dir: &TempDir, etc: Option<LineCatalogue>) -> Arc<Catalogues> {
        let line = ("/dev/ttyUSB0".to_string(), "site".to_string(), etc);
        Catalogues::new(Some(dir.0.clone()), [line])
    }

    fn assign(c: &Catalogues, blob: &str) {
        let sha = sha_of(blob.as_bytes());
        let catalogue = c.store(&sha, blob.as_bytes()).unwrap();
        c.apply(vec![(
            "/dev/ttyUSB0".into(),
            Update::Assigned { sha, catalogue },
        )]);
    }

    fn source(c: &Catalogues) -> String {
        c.effective("/dev/ttyUSB0").unwrap().source()
    }

    #[test]
    fn the_gateway_comes_before_etc_and_etc_before_none() {
        let dir = TempDir::new("precedence");
        let with_etc = line(&dir, Some(etc()));
        assert_eq!(source(&with_etc), "/etc/wiretap-server/line.catalog.toml");
        assign(&with_etc, CATALOGUE);
        let short = &sha_of(CATALOGUE.as_bytes())[..8];
        assert_eq!(source(&with_etc), format!("gateway {short}"));
        with_etc.apply(vec![("/dev/ttyUSB0".into(), Update::Cleared)]);
        assert_eq!(source(&with_etc), "/etc/wiretap-server/line.catalog.toml");

        let bare = line(&TempDir::new("precedence-bare"), None);
        assert_eq!(bare.effective("/dev/ttyUSB0"), Some(Rules::None));
    }

    #[test]
    fn a_restart_with_the_gateway_down_frames_from_the_cache() {
        let dir = TempDir::new("offline");
        assign(&line(&dir, Some(etc())), CATALOGUE);

        let restarted = line(&dir, Some(etc()));
        let Some(Rules::Gateway { sha, catalogue }) = restarted.effective("/dev/ttyUSB0") else {
            panic!("not the gateway's");
        };
        assert_eq!(sha, sha_of(CATALOGUE.as_bytes()));
        assert_eq!(catalogue.name, "gateway");
    }

    #[test]
    fn a_cached_blob_that_no_longer_hashes_right_is_not_framed_with() {
        let dir = TempDir::new("tampered");
        assign(&line(&dir, Some(etc())), CATALOGUE);
        let sha = sha_of(CATALOGUE.as_bytes());
        let cached = dir.0.join("catalogs").join(format!("{sha}.toml"));
        std::fs::write(&cached, CATALOGUE.replace("gateway", "tampered")).unwrap();

        let restarted = line(&dir, Some(etc()));
        assert_eq!(source(&restarted), "/etc/wiretap-server/line.catalog.toml");
        assert!(restarted.cached(&sha).unwrap_err().contains("hashes to"));
    }
}
