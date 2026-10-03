use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::Duration;

use appliance_server::{About, ProductAbout};
use appliance_store::Store;
use appliance_types::about::Fact;
use tokio::process::Command;

/// The About route waits for this, so the probe is bounded.
const PATIENCE: Duration = Duration::from_secs(5);

/// The capture daemon's version on the About screen, as the installed binary
/// reports it.
pub struct DaemonAbout {
    binary: PathBuf,
}

impl DaemonAbout {
    pub fn installed() -> Self {
        Self::at("/usr/bin/wiretap-server")
    }

    fn at(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    async fn version(&self) -> String {
        let asked = Command::new(&self.binary)
            .arg("--version")
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(PATIENCE, asked).await {
            Ok(Ok(out)) if out.status.success() => {
                let said = String::from_utf8_lossy(&out.stdout);
                let said = said.trim();
                said.strip_prefix("wiretap-server ")
                    .unwrap_or(said)
                    .to_string()
            }
            Ok(Ok(out)) => format!("not read: --version {}", out.status),
            Ok(Err(e)) if e.kind() == ErrorKind::NotFound => "not installed".to_string(),
            Ok(Err(e)) => format!("not read: {e}"),
            Err(_) => format!("not read: --version did not answer in {PATIENCE:?}"),
        }
    }
}

impl About for DaemonAbout {
    async fn about(&self, _store: &Store) -> ProductAbout {
        ProductAbout::new(
            vec![Fact::new("wiretap-server", self.version().await)],
            Vec::new(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use appliance_server::testing::{Harness, TempDir, body_json};
    use appliance_store::Store;
    use serde_json::json;

    use super::DaemonAbout;

    async fn facts(about: DaemonAbout) -> serde_json::Value {
        let app = Harness::builder()
            .store(Store::open_in_memory(None).await.expect("store"))
            .about(about)
            .build()
            .await;
        let admin = app.admin("admin").await;
        body_json(app.get("/api/about").cookie(&admin).send().await).await["facts"].clone()
    }

    #[tokio::test]
    async fn about_names_the_installed_daemons_version() {
        let dir = TempDir::new("wiretap-appliance-about");
        let binary = dir.join("wiretap-server");
        std::fs::write(
            &binary,
            "#!/bin/sh\necho 'wiretap-server 0.1.9 (abc1234)'\n",
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            facts(DaemonAbout::at(binary)).await,
            json!([{ "label": "wiretap-server", "value": "0.1.9 (abc1234)" }])
        );
    }

    #[tokio::test]
    async fn about_says_so_when_the_daemon_is_not_installed() {
        let dir = TempDir::new("wiretap-appliance-about-missing");
        assert_eq!(
            facts(DaemonAbout::at(dir.join("wiretap-server"))).await,
            json!([{ "label": "wiretap-server", "value": "not installed" }])
        );
    }
}
