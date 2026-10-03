//! `wiretap-server`'s configuration, carried in the chassis's backup and put
//! back to the package's on a factory reset. Its state directory is not
//! touched: catalogue assignments come back from the gateway at every
//! `HELLO_ACK`, and the disk cache is frames in flight, not what the box is.

use std::collections::BTreeSet;
use std::fs::{self, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use appliance_backup::{BackupError, Bundle, Sections, Snapshot};
use appliance_server::files::{replace_private, replace_readable};
use appliance_store::Store;
use appliance_store::sqlx::{Sqlite, Transaction};
use appliance_types::backup::Chosen;
use serde::{Deserialize, Serialize};

const SECTION: &str = "wiretap-server";
const RESTART: &str = "wiretap-server reads its configuration when it starts: restart it to use \
                       what is there now (systemctl restart wiretap-server).";

#[derive(Debug, Serialize, Deserialize)]
struct CarriedFile {
    path: PathBuf,
    contents: String,
    /// Whether group or others could read it, which decides `0644` or `0600`
    /// on the way back: the daemon reads its configuration as `wiretap`.
    readable: bool,
}

impl CarriedFile {
    fn write(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            make_dir(dir)?;
        }
        if self.readable {
            replace_readable(&self.path, self.contents.as_bytes())
        } else {
            replace_private(&self.path, self.contents.as_bytes())
        }
    }
}

#[derive(Clone)]
pub struct DaemonFiles {
    config_dir: PathBuf,
    /// The package's own `wiretap-server.toml`, which a reset puts back.
    reference: PathBuf,
}

impl DaemonFiles {
    pub fn installed() -> Self {
        Self::new(
            "/etc/wiretap-server",
            "/usr/share/wiretap-server/wiretap-server.toml",
        )
    }

    fn new(config_dir: impl Into<PathBuf>, reference: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: config_dir.into(),
            reference: reference.into(),
        }
    }

    fn read(&self) -> io::Result<Vec<CarriedFile>> {
        let mut files = Vec::new();
        walk(&self.config_dir, &mut files)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    /// A path out of a bundle, which on the first-run route is anyone's: only
    /// a plain path under the configuration directory.
    fn carries(&self, path: &Path) -> bool {
        path.strip_prefix(&self.config_dir).is_ok_and(|rest| {
            rest.components().next().is_some()
                && rest.components().all(|c| matches!(c, Component::Normal(_)))
        })
    }

    fn carried(&self, from: &Bundle) -> Result<Vec<CarriedFile>, BackupError> {
        let files: Vec<CarriedFile> = from.take(SECTION)?.unwrap_or_default();
        match files.iter().find(|f| !self.carries(&f.path)) {
            Some(stray) => Err(BackupError::Malformed(format!(
                "the {SECTION:?} section names {}, which is not wiretap-server's",
                stray.path.display()
            ))),
            None => Ok(files),
        }
    }

    /// The gateway token and the CAN settings go, and the package's
    /// configuration comes back; anything else in the directory stays.
    fn reset(&self) -> Vec<String> {
        let can_confs = fs::read_dir(self.config_dir.join("can.d"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "conf"));
        let mut said: Vec<String> = std::iter::once(self.config_dir.join("env"))
            .chain(can_confs)
            .filter_map(|path| match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    Some(format!("{} was not removed: {e}", path.display()))
                }
                _ => None,
            })
            .collect();
        let toml = self.config_dir.join("wiretap-server.toml");
        if let Err(e) = fs::read(&self.reference).and_then(|b| replace_readable(&toml, &b)) {
            said.push(format!(
                "{} was not put back from {}: {e}",
                toml.display(),
                self.reference.display()
            ));
        }
        said
    }
}

fn walk(dir: &Path, into: &mut Vec<CarriedFile>) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        entries => entries?,
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            walk(&entry.path(), into)?;
        } else if kind.is_file() {
            read_file(&entry.path(), into)?;
        }
    }
    Ok(())
}

fn read_file(path: &Path, into: &mut Vec<CarriedFile>) -> io::Result<()> {
    let Ok(contents) = String::from_utf8(fs::read(path)?) else {
        tracing::warn!(path = %path.display(), "not text, so left out of the backup");
        return Ok(());
    };
    let readable = fs::metadata(path)?.permissions().mode() & 0o044 != 0;
    into.push(CarriedFile {
        path: path.to_owned(),
        contents,
        readable,
    });
    Ok(())
}

/// `0755` whatever the unit's `UMask=0077` says, so `wiretap` can reach what
/// is restored under it.
fn make_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        make_dir(parent)?;
    }
    fs::create_dir(dir)?;
    fs::set_permissions(dir, Permissions::from_mode(0o755))
}

impl Sections for DaemonFiles {
    async fn capture(&self, _store: &Store, into: &mut Snapshot) -> Result<(), BackupError> {
        let files = self.read().map_err(|e| {
            BackupError::Internal(format!("wiretap-server's files could not be read: {e}"))
        })?;
        into.put(SECTION, &files)
    }

    /// Checks the paths before anything commits; the files are written in
    /// `after_restore`, which a rollback cannot undo.
    async fn restore(
        &self,
        _tx: &mut Transaction<'_, Sqlite>,
        from: &Bundle,
        chosen: &BTreeSet<String>,
    ) -> Result<Vec<String>, BackupError> {
        if chosen.contains(SECTION) {
            self.carried(from)?;
        }
        Ok(Vec::new())
    }

    async fn after_restore(&self, _store: &Store, from: &Bundle, applied: &Chosen) -> Vec<String> {
        if !applied.app.contains(SECTION) {
            return Vec::new();
        }
        let mut said: Vec<String> = match self.carried(from) {
            Ok(files) => files
                .iter()
                .filter_map(|f| {
                    f.write()
                        .err()
                        .map(|e| format!("{} was not restored: {e}", f.path.display()))
                })
                .collect(),
            Err(e) => return vec![e.to_string()],
        };
        said.push(RESTART.to_string());
        said
    }

    async fn erase(&self, _tx: &mut Transaction<'_, Sqlite>) -> Result<(), BackupError> {
        Ok(())
    }

    async fn after_reset(&self, _store: &Store, database_only: bool) -> Vec<String> {
        if database_only {
            return Vec::new();
        }
        let mut said = self.reset();
        said.push(RESTART.to_string());
        said
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use appliance_server::testing::{Harness, TempDir, body_json, body_text};
    use appliance_store::Store;
    use axum::http::StatusCode;
    use serde_json::json;

    use super::DaemonFiles;

    const PASSPHRASE: &str = "a passphrase long enough to be one";

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    async fn appliance(files: DaemonFiles) -> Harness {
        let builder = Harness::builder().store(Store::open_in_memory(None).await.expect("store"));
        let host = appliance_host::Host::unavailable("no D-Bus in a test", builder.bootstrap());
        let backups = appliance_backup_host::Backups::new(
            crate::NAME,
            "0.1.0-test",
            builder.dir(),
            host,
            files,
        );
        builder.api(backups.routes()).build().await
    }

    #[tokio::test]
    async fn a_restore_brings_back_the_daemons_configuration() {
        let dir = TempDir::new("wiretap-appliance-backup");
        let etc = dir.join("etc");
        let toml = etc.join("wiretap-server.toml");
        let env = etc.join("env");
        let can = etc.join("can.d/can0.conf");
        fs::create_dir_all(can.parent().unwrap()).unwrap();
        fs::write(&toml, "[server]\niface = \"can0\"\n").unwrap();
        fs::set_permissions(&toml, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&env, "WIRETAP_FORWARD_TOKEN=secret\n").unwrap();
        fs::set_permissions(&env, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&can, "BITRATE=500000\n").unwrap();

        let app = appliance(DaemonFiles::new(&etc, dir.join("reference.toml"))).await;
        let admin = app.admin("admin").await;
        let taken = app
            .post("/api/backup")
            .cookie(&admin)
            .json(json!({ "passphrase": PASSPHRASE, "password": Harness::PASSWORD }))
            .send()
            .await;
        assert_eq!(taken.status(), StatusCode::OK);
        let bundle = body_text(taken).await;
        let header: serde_json::Value = serde_json::from_str(&bundle).unwrap();
        assert_eq!(header["header"]["contains"]["app"]["wiretap-server"], 3);

        fs::remove_dir_all(&etc).unwrap();

        let restored = app
            .post("/api/backup/restore")
            .cookie(&admin)
            .json(json!({
                "passphrase": PASSPHRASE,
                "bundle": bundle,
                "password": Harness::PASSWORD,
                "sections": { "app": ["wiretap-server"] },
            }))
            .send()
            .await;
        assert_eq!(restored.status(), StatusCode::OK);
        let report = body_json(restored).await;
        assert_eq!(report["warnings"], json!([super::RESTART]), "{report}");

        assert_eq!(
            fs::read_to_string(&toml).unwrap(),
            "[server]\niface = \"can0\"\n"
        );
        assert_eq!(mode(&toml), 0o644);
        assert_eq!(mode(&env), 0o600, "a private file stays private");
        assert_eq!(mode(&etc.join("can.d")), 0o755);
        assert_eq!(fs::read_to_string(&can).unwrap(), "BITRATE=500000\n");
    }

    #[tokio::test]
    async fn a_factory_reset_takes_the_gateway_token_and_puts_the_packaged_configuration_back() {
        let dir = TempDir::new("wiretap-appliance-reset");
        let etc = dir.join("etc");
        let reference = dir.join("reference.toml");
        fs::create_dir_all(etc.join("can.d")).unwrap();
        fs::write(&reference, "[server]\n").unwrap();
        fs::write(
            etc.join("wiretap-server.toml"),
            "[forward]\nenable = true\n",
        )
        .unwrap();
        fs::write(etc.join("env"), "WIRETAP_FORWARD_TOKEN=secret\n").unwrap();
        fs::write(etc.join("can.d/can0.conf"), "BITRATE=500000\n").unwrap();
        fs::write(etc.join("can.d/README"), "kept\n").unwrap();
        fs::write(etc.join("line.catalog.toml"), "kept\n").unwrap();

        let app = appliance(DaemonFiles::new(&etc, &reference)).await;
        let reset = |database_only: bool| {
            app.post("/api/factory-reset").json(json!({
                "password": Harness::PASSWORD,
                "database_only": database_only,
            }))
        };

        let admin = app.admin("admin").await;
        let kept = reset(true).cookie(&admin).send().await;
        assert_eq!(kept.status(), StatusCode::OK);
        assert!(
            etc.join("env").exists(),
            "a database-only reset touches no file"
        );

        let admin = app.admin("admin").await;
        let reset = reset(false).cookie(&admin).send().await;
        assert_eq!(reset.status(), StatusCode::OK);
        let report = body_json(reset).await;
        assert!(
            report["warnings"]
                .as_array()
                .unwrap()
                .contains(&json!(super::RESTART)),
            "{report}"
        );

        assert!(!etc.join("env").exists());
        assert!(!etc.join("can.d/can0.conf").exists());
        assert_eq!(
            fs::read_to_string(etc.join("wiretap-server.toml")).unwrap(),
            "[server]\n"
        );
        assert_eq!(mode(&etc.join("wiretap-server.toml")), 0o644);
        assert!(etc.join("can.d/README").exists());
        assert!(etc.join("line.catalog.toml").exists());
    }

    #[test]
    fn only_the_daemons_configuration_is_restored() {
        let files = DaemonFiles::installed();
        for ours in [
            "/etc/wiretap-server/wiretap-server.toml",
            "/etc/wiretap-server/can.d/can0.conf",
        ] {
            assert!(files.carries(Path::new(ours)), "{ours}");
        }
        for theirs in [
            "/etc/wiretap-server",
            "/etc/wiretap-server/../shadow",
            "/etc/wiretap-server/can.d/../../passwd",
            "/etc/wiretap-server-other/x",
            "/var/lib/wiretap-server/assignments.json",
            "etc/wiretap-server/wiretap-server.toml",
        ] {
            assert!(!files.carries(Path::new(theirs)), "{theirs}");
        }
    }
}
