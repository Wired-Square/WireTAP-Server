//! Catalogue assignment: which catalogue blob each capture daemon's device
//! frames with, and the devices each daemon last named in its `HELLO`. All of
//! it lives in `wiretap_meta` in the default database, beside the API keys.
//!
//! A blob is stored and served exactly as it was assigned, CRLF and BOM
//! included: its name is the Git blob SHA-1 of those bytes.

use chrono::{DateTime, Utc};
use tokio_postgres::Row;
use wiretap_gateway::{
    AssignCatalog, CatalogFinding, Daemon, DaemonDevice, DaemonList, Provenance, RefusedCatalog,
    StoredCatalog, UnassignParams,
};
use wiretap_model::{blob_sha1, blob_sha1_hex};
use wiretap_protocol::ingest::{self, valid_daemon_id, Assignment, CatalogStatus, Device, Refusal};

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
CREATE TABLE IF NOT EXISTS wiretap_meta.daemon_active (
  daemon_id    text,
  interface    text,
  source       text        NOT NULL,
  blob_sha     bytea,
  refused_sha  bytea,
  refusal      text,
  since        timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (daemon_id, interface)
);
"#;

#[derive(Debug, PartialEq)]
pub enum AssignError {
    /// The request itself: no retry will take it.
    Rejected(Vec<CatalogFinding>),
    /// `expected` is not what is assigned now, which is this SHA.
    Conflict(Option<String>),
    Database(String),
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

    /// Replace what `daemon_id` last reported for the interfaces its `HELLO`
    /// named. `since` moves only when the catalogue itself changes.
    pub async fn record_status(
        &self,
        daemon_id: &str,
        devices: &[Device],
        status: &CatalogStatus,
    ) -> Result<(), String> {
        let rows = active_rows(devices, status);
        let interfaces: Vec<&str> = rows.iter().map(|r| r.interface).collect();
        let sources: Vec<&str> = rows.iter().map(|r| r.source).collect();
        let shas: Vec<Option<&[u8]>> = rows
            .iter()
            .map(|r| r.blob_sha.as_ref().map(|s| &s[..]))
            .collect();
        let refused: Vec<Option<&[u8]>> = rows
            .iter()
            .map(|r| r.refused.as_ref().map(|(s, _)| &s[..]))
            .collect();
        let refusals: Vec<Option<&str>> =
            rows.iter().map(|r| r.refused.map(|(_, why)| why)).collect();
        let named: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
        let db =
            |e: tokio_postgres::Error| format!("recording {daemon_id}'s catalogues failed: {e}");
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(db)?;
        tx.execute(
            "INSERT INTO wiretap_meta.daemon_active \
             (daemon_id, interface, source, blob_sha, refused_sha, refusal) \
             SELECT $1, i, s, b, r, why \
             FROM UNNEST($2::text[], $3::text[], $4::bytea[], $5::bytea[], $6::text[]) \
               AS a(i, s, b, r, why) \
             ON CONFLICT (daemon_id, interface) DO UPDATE SET \
               since = CASE WHEN (daemon_active.source, daemon_active.blob_sha) \
                 IS NOT DISTINCT FROM (EXCLUDED.source, EXCLUDED.blob_sha) \
                 THEN daemon_active.since ELSE now() END, \
               source = EXCLUDED.source, blob_sha = EXCLUDED.blob_sha, \
               refused_sha = EXCLUDED.refused_sha, refusal = EXCLUDED.refusal",
            &[
                &daemon_id,
                &interfaces,
                &sources,
                &shas,
                &refused,
                &refusals,
            ],
        )
        .await
        .map_err(db)?;
        tx.execute(
            "DELETE FROM wiretap_meta.daemon_active \
             WHERE daemon_id = $1 AND interface = ANY($2) AND NOT interface = ANY($3)",
            &[&daemon_id, &named, &interfaces],
        )
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)
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

    /// Assign a catalogue to a daemon's interface, and close that daemon's
    /// live sessions that carry the interface so they reconnect to it.
    pub async fn assign(
        &self,
        request: &AssignCatalog,
        assigned_by: &str,
    ) -> Result<wiretap_gateway::Assignment, AssignError> {
        let AssignCatalog {
            daemon_id,
            interface,
            content,
            ..
        } = request;
        check_key(daemon_id, interface)?;
        let (sha, name) = checked_blob(content, &request.provenance)?;
        let provenance = serde_json::to_string(&request.provenance).expect("plain JSON");
        let db = |e: tokio_postgres::Error| AssignError::Database(format!("assign failed: {e}"));
        let mut client = self.client().await.map_err(AssignError::Database)?;
        let tx = client.transaction().await.map_err(db)?;
        let current = lock_assignment(&tx, daemon_id, interface)
            .await
            .map_err(db)?;
        guard(request.expected.as_deref(), current.as_deref())?;
        tx.execute(
            "INSERT INTO wiretap_meta.catalog_blobs (blob_sha, content, provenance) \
             VALUES ($1, $2, $3::text::jsonb) ON CONFLICT (blob_sha) DO NOTHING",
            &[&sha.as_slice(), content, &provenance],
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
                &[daemon_id, interface, &sha.as_slice(), &assigned_by],
            )
            .await
            .map_err(db)?
            > 0;
        if changed {
            record_history(&tx, daemon_id, interface, Some(&sha), assigned_by)
                .await
                .map_err(db)?;
        }
        let row = tx
            .query_one(
                "SELECT a.assigned_at, a.assigned_by, b.provenance::text \
                 FROM wiretap_meta.catalog_assignments a \
                 JOIN wiretap_meta.catalog_blobs b USING (blob_sha) \
                 WHERE a.daemon_id = $1 AND a.interface = $2",
                &[daemon_id, interface],
            )
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        if changed {
            self.sessions.reassign(daemon_id, interface).await;
        }
        Ok(wiretap_gateway::Assignment {
            blob_sha: blob_sha1_hex(&sha),
            name: Some(name),
            assigned_at_us: row.get::<_, DateTime<Utc>>(0).timestamp_micros(),
            assigned_by: row.get(1),
            provenance: provenance_of(row.get(2)),
        })
    }

    /// Whether there was an assignment to clear.
    pub async fn clear(
        &self,
        params: &UnassignParams,
        cleared_by: &str,
    ) -> Result<(), AssignError> {
        let UnassignParams {
            daemon_id,
            interface,
            expected,
        } = params;
        let db = |e: tokio_postgres::Error| AssignError::Database(format!("clear failed: {e}"));
        let mut client = self.client().await.map_err(AssignError::Database)?;
        let tx = client.transaction().await.map_err(db)?;
        let current = lock_assignment(&tx, daemon_id, interface)
            .await
            .map_err(db)?;
        guard(expected.as_deref(), current.as_deref())?;
        let cleared = tx
            .execute(
                "DELETE FROM wiretap_meta.catalog_assignments \
                 WHERE daemon_id = $1 AND interface = $2",
                &[daemon_id, interface],
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
        Ok(())
    }

    /// Every daemon that has named a device or been assigned a catalogue.
    pub async fn daemons(&self) -> Result<DaemonList, String> {
        let rows = self
            .client()
            .await?
            .query(
                "WITH named AS ( \
                   SELECT daemon_id, interface FROM wiretap_meta.daemon_devices \
                   UNION SELECT daemon_id, interface FROM wiretap_meta.catalog_assignments) \
                 SELECT n.daemon_id, n.interface, d.bus, d.database, d.last_seen, \
                   a.blob_sha, a.assigned_at, a.assigned_by, ab.content, ab.provenance::text, \
                   x.source, x.blob_sha, x.since, xb.content, x.refused_sha, x.refusal \
                 FROM named n \
                 LEFT JOIN wiretap_meta.daemon_devices d USING (daemon_id, interface) \
                 LEFT JOIN wiretap_meta.catalog_assignments a USING (daemon_id, interface) \
                 LEFT JOIN wiretap_meta.catalog_blobs ab ON ab.blob_sha = a.blob_sha \
                 LEFT JOIN wiretap_meta.daemon_active x USING (daemon_id, interface) \
                 LEFT JOIN wiretap_meta.catalog_blobs xb ON xb.blob_sha = x.blob_sha \
                 ORDER BY n.daemon_id, d.bus NULLS LAST, n.interface",
                &[],
            )
            .await
            .map_err(|e| format!("reading the daemons failed: {e}"))?;
        let mut daemons: Vec<Daemon> = Vec::new();
        for row in &rows {
            let daemon_id: String = row.get(0);
            let device = daemon_device(row);
            match daemons.last_mut() {
                Some(d) if d.daemon_id == daemon_id => d.devices.push(device),
                _ => daemons.push(Daemon {
                    daemon_id,
                    devices: vec![device],
                }),
            }
        }
        Ok(DaemonList { daemons })
    }

    pub async fn stored(&self, sha: &[u8; 20]) -> Result<Option<StoredCatalog>, String> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT content, provenance::text, created_at \
                 FROM wiretap_meta.catalog_blobs WHERE blob_sha = $1",
                &[&sha.as_slice()],
            )
            .await
            .map_err(|e| format!("reading catalogue {} failed: {e}", blob_sha1_hex(sha)))?;
        Ok(row.map(|r| StoredCatalog {
            blob_sha: blob_sha1_hex(sha),
            content: r.get(0),
            provenance: provenance_of(r.get(1)),
            created_at_us: r.get::<_, DateTime<Utc>>(2).timestamp_micros(),
        }))
    }
}

/// One row of [`Catalogs::daemons`]' query.
fn daemon_device(row: &Row) -> DaemonDevice {
    let micros = |i: usize| {
        row.get::<_, Option<DateTime<Utc>>>(i)
            .map(|t| t.timestamp_micros())
    };
    let text = |i: usize| row.get::<_, Option<String>>(i);
    let sha = |i: usize| row.get::<_, Option<Vec<u8>>>(i).map(hex::encode);
    let assignment = sha(5).map(|blob_sha| wiretap_gateway::Assignment {
        blob_sha,
        name: text(8).as_deref().and_then(catalogue_name),
        assigned_at_us: micros(6).unwrap_or_default(),
        assigned_by: text(7),
        provenance: text(9).map(provenance_of).unwrap_or_default(),
    });
    let refused = sha(14)
        .zip(text(15))
        .map(|(blob_sha, reason)| RefusedCatalog { blob_sha, reason });
    let active = text(10).map(|source| wiretap_gateway::ActiveCatalog {
        source,
        blob_sha: sha(11),
        name: text(13).as_deref().and_then(catalogue_name),
        since_us: micros(12).unwrap_or_default(),
        refused,
    });
    DaemonDevice {
        interface: row.get(1),
        bus: row
            .get::<_, Option<i16>>(2)
            .and_then(|b| u8::try_from(b).ok()),
        database: text(3),
        last_seen_us: micros(4),
        assignment,
        active,
    }
}

fn catalogue_name(content: &str) -> Option<String> {
    wiretap_catalog::rtu_rules(content).ok().map(|r| r.name)
}

/// The default for stored JSON whose fields do not fit `Provenance`.
fn provenance_of(json: String) -> Provenance {
    serde_json::from_str(&json).unwrap_or_default()
}

/// Hold `(daemon_id, interface)` for the rest of `tx`, and read the SHA
/// assigned there. An advisory lock, as there is no row to lock until the
/// first assignment.
async fn lock_assignment(
    tx: &tokio_postgres::Transaction<'_>,
    daemon_id: &str,
    interface: &str,
) -> Result<Option<String>, tokio_postgres::Error> {
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtextextended($1 || '/' || $2, 0))",
        &[&daemon_id, &interface],
    )
    .await?;
    let row = tx
        .query_opt(
            "SELECT blob_sha FROM wiretap_meta.catalog_assignments \
             WHERE daemon_id = $1 AND interface = $2",
            &[&daemon_id, &interface],
        )
        .await?;
    Ok(row.map(|r| hex::encode(r.get::<_, Vec<u8>>(0))))
}

/// `expected` against the SHA assigned now: `""` expects none, and absent
/// expects nothing.
fn guard(expected: Option<&str>, current: Option<&str>) -> Result<(), AssignError> {
    match expected {
        Some(e) if !e.eq_ignore_ascii_case(current.unwrap_or("")) => {
            Err(AssignError::Conflict(current.map(str::to_owned)))
        }
        _ => Ok(()),
    }
}

async fn record_history(
    tx: &tokio_postgres::Transaction<'_>,
    daemon_id: &str,
    interface: &str,
    sha: Option<&[u8; 20]>,
    assigned_by: &str,
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

#[derive(Debug, PartialEq)]
struct ActiveRow<'a> {
    interface: &'a str,
    source: &'static str,
    blob_sha: Option<[u8; 20]>,
    refused: Option<([u8; 20], &'static str)>,
}

/// A row for each reported bus the `HELLO` named, under the interface it
/// named on that bus.
fn active_rows<'a>(devices: &'a [Device], status: &CatalogStatus) -> Vec<ActiveRow<'a>> {
    status
        .entries
        .iter()
        .filter_map(|e| {
            let device = devices.iter().find(|d| d.bus == e.bus)?;
            let (source, blob_sha) = match e.active {
                ingest::ActiveCatalog::None => ("none", None),
                ingest::ActiveCatalog::Local(sha) => ("local", Some(sha)),
                ingest::ActiveCatalog::Assigned(sha) => ("assigned", Some(sha)),
            };
            let refused = e.refused.map(|(sha, why)| {
                let why = match why {
                    Refusal::HashMismatch => "hash_mismatch",
                    Refusal::DidNotParse => "did_not_parse",
                    Refusal::FetchFailed => "fetch_failed",
                    Refusal::Other(_) => "other",
                };
                (sha, why)
            });
            Some(ActiveRow {
                interface: &device.name,
                source,
                blob_sha,
                refused,
            })
        })
        .collect()
}

fn rejected(field: &str, message: String) -> AssignError {
    AssignError::Rejected(vec![CatalogFinding {
        field: field.into(),
        message,
    }])
}

fn check_key(daemon_id: &str, interface: &str) -> Result<(), AssignError> {
    if !valid_daemon_id(daemon_id) {
        let why = format!("{daemon_id:?} is not a daemon id");
        return Err(rejected("daemon_id", why));
    }
    if interface.is_empty() || interface.len() > 255 {
        let why = format!("an interface is 1 to 255 bytes, not {}", interface.len());
        return Err(rejected("interface", why));
    }
    Ok(())
}

/// The blob's SHA-1 and name, once `provenance.blob_sha` (when given) agrees
/// with it, the catalogue validates, and the daemon's own reader would take
/// it: `rtu_rules`, and a name. That is the check the daemon applies to a
/// catalogue in `/etc`, so a catalogue the gateway takes is one every daemon
/// frames with.
fn checked_blob(content: &str, provenance: &Provenance) -> Result<([u8; 20], String), AssignError> {
    let sha = blob_sha1(content.as_bytes());
    if let Some(claimed) = &provenance.blob_sha {
        let hex = blob_sha1_hex(&sha);
        if !claimed.eq_ignore_ascii_case(&hex) {
            let why = format!("{claimed}, but the content hashes to {hex}");
            return Err(rejected("provenance.blob_sha", why));
        }
    }
    let findings = wiretap_catalog::validate::validate(content);
    if !findings.is_empty() {
        let findings = findings.into_iter().map(|f| CatalogFinding {
            field: f.field,
            message: f.message,
        });
        return Err(AssignError::Rejected(findings.collect()));
    }
    let rules = wiretap_catalog::rtu_rules(content).map_err(|e| {
        rejected(
            "catalogue",
            format!("the capture daemon would refuse it: {e}"),
        )
    })?;
    if rules.name.is_empty() {
        return Err(rejected(
            "meta.name",
            "Catalog name must not be empty".into(),
        ));
    }
    Ok((sha, rules.name))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_catalogue_status_is_stored_under_the_interfaces_its_hello_named() {
        let (assigned, refused, local) = (blob_sha1(b"a"), blob_sha1(b"r"), blob_sha1(b"l"));
        let entry = |bus, active, refused| ingest::CatalogStatusEntry {
            bus,
            active,
            refused,
        };
        let status = CatalogStatus {
            entries: vec![
                entry(
                    4,
                    ingest::ActiveCatalog::Assigned(assigned),
                    Some((refused, Refusal::DidNotParse)),
                ),
                entry(0, ingest::ActiveCatalog::None, None),
                entry(5, ingest::ActiveCatalog::Local(local), None),
                entry(9, ingest::ActiveCatalog::None, None),
            ],
        };
        let devices = [
            device(0, "can0"),
            device(4, "/dev/ttyUSB0"),
            device(5, "/dev/ttyUSB1"),
        ];
        let row = |interface, source, blob_sha, refused| ActiveRow {
            interface,
            source,
            blob_sha,
            refused,
        };
        assert_eq!(
            active_rows(&devices, &status),
            [
                row(
                    "/dev/ttyUSB0",
                    "assigned",
                    Some(assigned),
                    Some((refused, "did_not_parse"))
                ),
                row("can0", "none", None, None),
                row("/dev/ttyUSB1", "local", Some(local), None),
            ],
            "bus 9 was not in the HELLO"
        );
    }

    fn findings(content: &str) -> Vec<(String, String)> {
        let Err(AssignError::Rejected(found)) = checked_blob(content, &Provenance::default())
        else {
            panic!("{content:?} was taken");
        };
        found.into_iter().map(|f| (f.field, f.message)).collect()
    }

    #[test]
    fn a_blob_is_named_by_its_git_sha() {
        let (sha, name) = checked_blob(CATALOGUE, &Provenance::default()).unwrap();
        assert_eq!(
            (sha, name.as_str()),
            (blob_sha1(CATALOGUE.as_bytes()), "bench")
        );
        let claimed = Provenance {
            blob_sha: Some(blob_sha1_hex(&sha).to_uppercase()),
            ..Provenance::default()
        };
        assert_eq!(checked_blob(CATALOGUE, &claimed).unwrap().0, sha);
    }

    #[test]
    fn a_claimed_sha_the_content_does_not_hash_to_is_refused() {
        let wrong = Provenance {
            blob_sha: Some(blob_sha1_hex(&blob_sha1(b"other"))),
            ..Provenance::default()
        };
        let Err(AssignError::Rejected(found)) = checked_blob(CATALOGUE, &wrong) else {
            panic!("taken");
        };
        assert_eq!(found[0].field, "provenance.blob_sha");
        assert!(found[0].message.contains("hashes to"), "{found:?}");
    }

    #[test]
    fn a_crlf_catalogue_is_hashed_as_it_came() {
        let crlf = CATALOGUE.replace('\n', "\r\n");
        let (sha, _) = checked_blob(&crlf, &Provenance::default()).unwrap();
        assert_eq!(sha, blob_sha1(crlf.as_bytes()));
        assert_ne!(sha, blob_sha1(CATALOGUE.as_bytes()));
    }

    #[test]
    fn a_catalogue_that_does_not_validate_is_refused_with_its_findings() {
        let unreadable_rule = "[meta]\nname = \"x\"\n[meta.modbus.function_code.0x60]\n\
                               lengths = [{ when = { offset = 4, value = 3 } }]\n";
        assert_eq!(
            findings(unreadable_rule),
            [(
                "meta.modbus.function_code.0x60.lengths[0]".to_string(),
                "A length rule needs a len of { fixed } or { count_at, overhead }, \
                 and optionally a when of { offset, value }"
                    .to_string()
            )]
        );
        assert_eq!(findings("[meta\n")[0].0, "toml");
    }

    #[test]
    fn a_catalogue_that_validates_but_the_daemon_would_refuse_names_why() {
        assert_eq!(
            findings("[meta]\nname = \"\"\n"),
            [(
                "meta.name".to_string(),
                "Catalog name must not be empty".to_string()
            )]
        );
    }

    #[test]
    fn expected_guards_against_a_concurrent_change() {
        let now = blob_sha1_hex(&blob_sha1(b"now"));
        assert_eq!(guard(None, Some(&now)), Ok(()));
        assert_eq!(guard(Some(&now.to_uppercase()), Some(&now)), Ok(()));
        assert_eq!(guard(Some(""), None), Ok(()), "only if unassigned");
        assert_eq!(
            guard(Some(""), Some(&now)),
            Err(AssignError::Conflict(Some(now.clone())))
        );
        assert_eq!(guard(Some(&now), None), Err(AssignError::Conflict(None)));
    }

    #[test]
    fn a_daemon_id_or_interface_the_hello_could_not_carry_is_refused() {
        assert!(check_key("bench", "/dev/ttyUSB0").is_ok());
        assert!(check_key("Bench", "can0").is_err());
        assert!(check_key("bench", "").is_err());
        assert!(check_key("bench", &"x".repeat(256)).is_err());
    }
}
