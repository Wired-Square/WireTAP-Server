//! Catalogue assignment: which catalogue blob each capture daemon's device
//! frames with, and the devices each daemon last named in its `HELLO`. All of
//! it lives in `wiretap_meta` in the default database, beside the API keys.
//!
//! A blob is stored and served exactly as it was assigned, CRLF and BOM
//! included: its name is the Git blob SHA-1 of those bytes.

use serde_json::Value;
use wiretap_model::{blob_sha1, blob_sha1_hex};
use wiretap_protocol::ingest::{valid_daemon_id, Assignment, Device};

use crate::db::Databases;
use crate::ingest::Sessions;

const CATALOG_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS wiretap_meta.catalog_blobs (
  blob_sha    bytea       PRIMARY KEY,
  content     text        NOT NULL,
  provenance  jsonb       NOT NULL DEFAULT '{}',
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS wiretap_meta.catalog_assignments (
  daemon_id    text,
  interface    text,
  blob_sha     bytea       NOT NULL REFERENCES wiretap_meta.catalog_blobs,
  assigned_at  timestamptz NOT NULL DEFAULT now(),
  assigned_by  text,
  PRIMARY KEY (daemon_id, interface)
);
CREATE TABLE IF NOT EXISTS wiretap_meta.catalog_assignment_history (
  id           bigserial   PRIMARY KEY,
  daemon_id    text        NOT NULL,
  interface    text        NOT NULL,
  blob_sha     bytea,
  from_ts      timestamptz NOT NULL DEFAULT now(),
  assigned_by  text
);
CREATE TABLE IF NOT EXISTS wiretap_meta.daemon_devices (
  daemon_id  text,
  interface  text,
  bus        smallint    NOT NULL,
  database   text        NOT NULL,
  last_seen  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (daemon_id, interface)
);
"#;

#[derive(Debug, PartialEq, Eq)]
pub enum AssignError {
    /// The request itself: no retry will take it.
    Invalid(String),
    Database(String),
}

impl std::fmt::Display for AssignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(why) | Self::Database(why) => f.write_str(why),
        }
    }
}

#[derive(Clone)]
pub struct Catalogs {
    dbs: Databases,
    sessions: Sessions,
}

impl Catalogs {
    pub fn new(dbs: Databases, sessions: Sessions) -> Self {
        Self { dbs, sessions }
    }

    /// Call once at startup, after [`crate::keys::KeyStore::bootstrap`] has
    /// made the schema.
    pub async fn bootstrap(&self) -> Result<(), String> {
        self.client()
            .await?
            .batch_execute(CATALOG_SCHEMA)
            .await
            .map_err(|e| format!("catalogue schema failed: {e}"))
    }

    async fn client(&self) -> Result<tokio_postgres::Client, String> {
        self.dbs.connect_raw(self.dbs.default_database()).await
    }

    pub async fn record_hello(
        &self,
        daemon_id: &str,
        database: &str,
        devices: &[Device],
    ) -> Result<(), String> {
        let interfaces: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
        let buses: Vec<i16> = devices.iter().map(|d| i16::from(d.bus)).collect();
        self.client()
            .await?
            .execute(
                "INSERT INTO wiretap_meta.daemon_devices (daemon_id, interface, bus, database) \
                 SELECT $1, i, b, $4 FROM UNNEST($2::text[], $3::smallint[]) AS d(i, b) \
                 ON CONFLICT (daemon_id, interface) DO UPDATE \
                 SET bus = EXCLUDED.bus, database = EXCLUDED.database, last_seen = now()",
                &[&daemon_id, &interfaces, &buses, &database],
            )
            .await
            .map_err(|e| format!("recording {daemon_id}'s devices failed: {e}"))?;
        Ok(())
    }

    pub async fn assignments_for(
        &self,
        daemon_id: &str,
        devices: &[Device],
    ) -> Result<Vec<Assignment>, String> {
        let interfaces: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
        let rows = self
            .client()
            .await?
            .query(
                "SELECT interface, blob_sha FROM wiretap_meta.catalog_assignments \
                 WHERE daemon_id = $1 AND interface = ANY($2)",
                &[&daemon_id, &interfaces],
            )
            .await
            .map_err(|e| format!("reading {daemon_id}'s assignments failed: {e}"))?;
        let assigned: Vec<(String, Vec<u8>)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
        Ok(through_device_map(&assigned, devices))
    }

    pub async fn blob(&self, sha: &[u8; 20]) -> Result<Option<String>, String> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT content FROM wiretap_meta.catalog_blobs WHERE blob_sha = $1",
                &[&sha.as_slice()],
            )
            .await
            .map_err(|e| format!("reading catalogue {} failed: {e}", blob_sha1_hex(sha)))?;
        Ok(row.map(|r| r.get(0)))
    }

    /// Assign `content` to a daemon's interface, and close that daemon's live
    /// sessions that carry the interface so they reconnect to it. The blob's
    /// SHA-1 comes back.
    #[allow(dead_code, reason = "the admin API calls it")]
    pub async fn assign(
        &self,
        daemon_id: &str,
        interface: &str,
        content: &str,
        provenance: &Value,
        assigned_by: Option<&str>,
    ) -> Result<[u8; 20], AssignError> {
        check_key(daemon_id, interface)?;
        let sha = checked_blob(content, provenance)?;
        let db = |e: tokio_postgres::Error| AssignError::Database(format!("assign failed: {e}"));
        let mut client = self.client().await.map_err(AssignError::Database)?;
        let tx = client.transaction().await.map_err(db)?;
        tx.execute(
            "INSERT INTO wiretap_meta.catalog_blobs (blob_sha, content, provenance) \
             VALUES ($1, $2, $3::text::jsonb) ON CONFLICT (blob_sha) DO NOTHING",
            &[&sha.as_slice(), &content, &provenance.to_string()],
        )
        .await
        .map_err(db)?;
        let changed = tx
            .execute(
                "INSERT INTO wiretap_meta.catalog_assignments \
                 (daemon_id, interface, blob_sha, assigned_by) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (daemon_id, interface) DO UPDATE \
                 SET blob_sha = EXCLUDED.blob_sha, assigned_at = now(), \
                     assigned_by = EXCLUDED.assigned_by \
                 WHERE catalog_assignments.blob_sha <> EXCLUDED.blob_sha",
                &[&daemon_id, &interface, &sha.as_slice(), &assigned_by],
            )
            .await
            .map_err(db)?
            > 0;
        if changed {
            record_history(&tx, daemon_id, interface, Some(&sha), assigned_by)
                .await
                .map_err(db)?;
        }
        tx.commit().await.map_err(db)?;
        if changed {
            self.sessions.reassign(daemon_id, interface).await;
        }
        Ok(sha)
    }

    /// Whether there was an assignment to clear.
    #[allow(dead_code, reason = "the admin API calls it")]
    pub async fn clear(
        &self,
        daemon_id: &str,
        interface: &str,
        cleared_by: Option<&str>,
    ) -> Result<bool, String> {
        let db = |e: tokio_postgres::Error| format!("clear failed: {e}");
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(db)?;
        let cleared = tx
            .execute(
                "DELETE FROM wiretap_meta.catalog_assignments \
                 WHERE daemon_id = $1 AND interface = $2",
                &[&daemon_id, &interface],
            )
            .await
            .map_err(db)?
            > 0;
        if cleared {
            record_history(&tx, daemon_id, interface, None, cleared_by)
                .await
                .map_err(db)?;
        }
        tx.commit().await.map_err(db)?;
        if cleared {
            self.sessions.reassign(daemon_id, interface).await;
        }
        Ok(cleared)
    }
}

async fn record_history(
    tx: &tokio_postgres::Transaction<'_>,
    daemon_id: &str,
    interface: &str,
    sha: Option<&[u8; 20]>,
    assigned_by: Option<&str>,
) -> Result<u64, tokio_postgres::Error> {
    tx.execute(
        "INSERT INTO wiretap_meta.catalog_assignment_history \
         (daemon_id, interface, blob_sha, assigned_by) VALUES ($1, $2, $3, $4)",
        &[
            &daemon_id,
            &interface,
            &sha.map(|s| s.as_slice()),
            &assigned_by,
        ],
    )
    .await
}

/// An assignment for each assigned interface the `HELLO` named, on the bus it
/// named it on.
fn through_device_map(assigned: &[(String, Vec<u8>)], devices: &[Device]) -> Vec<Assignment> {
    devices
        .iter()
        .filter_map(|d| {
            let (_, sha) = assigned.iter().find(|(i, _)| *i == d.name)?;
            Some(Assignment {
                bus: d.bus,
                blob_sha: sha.as_slice().try_into().ok()?,
            })
        })
        .collect()
}

fn check_key(daemon_id: &str, interface: &str) -> Result<(), AssignError> {
    if !valid_daemon_id(daemon_id) {
        return Err(AssignError::Invalid(format!(
            "{daemon_id:?} is not a daemon id"
        )));
    }
    if interface.is_empty() || interface.len() > 255 {
        return Err(AssignError::Invalid(format!(
            "an interface is 1 to 255 bytes, not {}",
            interface.len()
        )));
    }
    Ok(())
}

/// The blob's SHA-1, once `provenance.blob_sha` (when given) agrees with it
/// and the daemon's own reader would take the catalogue: `rtu_rules`, and a
/// name. That is the check the daemon applies to a catalogue in `/etc`, so a
/// catalogue the gateway takes is one every daemon frames with.
fn checked_blob(content: &str, provenance: &Value) -> Result<[u8; 20], AssignError> {
    let sha = blob_sha1(content.as_bytes());
    match provenance.get("blob_sha") {
        None => {}
        Some(Value::String(claimed)) if claimed.eq_ignore_ascii_case(&blob_sha1_hex(&sha)) => {}
        Some(Value::String(claimed)) => {
            return Err(AssignError::Invalid(format!(
                "provenance.blob_sha is {claimed}, but the content hashes to {}",
                blob_sha1_hex(&sha)
            )))
        }
        Some(_) => {
            return Err(AssignError::Invalid(
                "provenance.blob_sha must be a hex string".into(),
            ))
        }
    }
    let rules = wiretap_catalog::rtu_rules(content)
        .map_err(|e| AssignError::Invalid(format!("catalogue: {e}")))?;
    if rules.name.is_empty() {
        return Err(AssignError::Invalid(
            "catalogue: meta.name: Catalog name must not be empty".into(),
        ));
    }
    Ok(sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CATALOGUE: &str = "[meta]\nname = \"bench\"\n";

    fn device(bus: u8, name: &str) -> Device {
        Device {
            bus,
            name: name.into(),
        }
    }

    #[test]
    fn an_assignment_goes_out_on_the_bus_this_hello_named() {
        let sha = blob_sha1(b"x");
        let assigned = vec![
            ("/dev/ttyUSB0".to_string(), sha.to_vec()),
            ("can9".to_string(), blob_sha1(b"y").to_vec()),
        ];
        let devices = [device(0, "can0"), device(4, "/dev/ttyUSB0")];
        assert_eq!(
            through_device_map(&assigned, &devices),
            [Assignment {
                bus: 4,
                blob_sha: sha
            }],
            "renumbered from whatever bus it had before, and only what was named"
        );
    }

    #[test]
    fn a_blob_is_named_by_its_git_sha() {
        let sha = checked_blob(CATALOGUE, &json!({})).unwrap();
        assert_eq!(sha, blob_sha1(CATALOGUE.as_bytes()));
        let claimed = json!({ "blob_sha": blob_sha1_hex(&sha).to_uppercase() });
        assert_eq!(checked_blob(CATALOGUE, &claimed), Ok(sha));
    }

    #[test]
    fn a_claimed_sha_the_content_does_not_hash_to_is_refused() {
        let wrong = json!({ "blob_sha": blob_sha1_hex(&blob_sha1(b"other")) });
        let Err(AssignError::Invalid(why)) = checked_blob(CATALOGUE, &wrong) else {
            panic!("taken");
        };
        assert!(why.contains("hashes to"), "{why}");
        assert!(matches!(
            checked_blob(CATALOGUE, &json!({ "blob_sha": 5 })),
            Err(AssignError::Invalid(_))
        ));
    }

    #[test]
    fn a_crlf_catalogue_is_hashed_as_it_came() {
        let crlf = CATALOGUE.replace('\n', "\r\n");
        assert_eq!(
            checked_blob(&crlf, &json!({})),
            Ok(blob_sha1(crlf.as_bytes()))
        );
        assert_ne!(blob_sha1(crlf.as_bytes()), blob_sha1(CATALOGUE.as_bytes()));
    }

    #[test]
    fn a_catalogue_the_daemon_would_refuse_is_refused() {
        for (text, why) in [
            ("[meta\n", "not valid TOML"),
            ("[meta]\nname = \"\"\n", "must not be empty"),
            (
                "[meta]\nname = \"x\"\n[meta.modbus.function_code.0x60]\n\
                 lengths = [{ when = { offset = 4, value = 3 } }]\n",
                "function_code.0x60.lengths[0]",
            ),
        ] {
            let Err(AssignError::Invalid(got)) = checked_blob(text, &json!({})) else {
                panic!("{text:?} was taken");
            };
            assert!(got.contains(why), "{text:?}: {got}");
        }
    }

    #[test]
    fn a_daemon_id_or_interface_the_hello_could_not_carry_is_refused() {
        assert!(check_key("bench", "/dev/ttyUSB0").is_ok());
        assert!(check_key("Bench", "can0").is_err());
        assert!(check_key("bench", "").is_err());
        assert!(check_key("bench", &"x".repeat(256)).is_err());
    }
}
