//! API key store. Keys live in `wiretap_meta.api_keys` in the default
//! database (sha256-hashed; the plaintext is shown once at creation).
//! A bootstrap admin key, from the environment or its file, is honoured unless
//! a rotation retired it, and a new one always is, so the stack can never lock
//! itself out.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::db::Databases;
use crate::secrets;

const META_SCHEMA: &str = r#"
CREATE SCHEMA IF NOT EXISTS wiretap_meta;
CREATE TABLE IF NOT EXISTS wiretap_meta.api_keys (
  id            BIGSERIAL PRIMARY KEY,
  name          TEXT        NOT NULL,
  key_hash      TEXT        NOT NULL UNIQUE,
  role          TEXT        NOT NULL CHECK (role IN ('read', 'ingest', 'admin')),
  database_pin  TEXT,
  created_at    timestamptz NOT NULL DEFAULT now(),
  last_used_at  timestamptz,
  disabled_at   timestamptz
);
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM information_schema.columns
             WHERE table_schema = 'wiretap_meta' AND table_name = 'api_keys'
               AND column_name = 'revoked_at') THEN
    ALTER TABLE wiretap_meta.api_keys RENAME COLUMN revoked_at TO disabled_at;
  END IF;
EXCEPTION WHEN undefined_column THEN NULL; -- another gateway renamed it first
END $$;
CREATE TABLE IF NOT EXISTS wiretap_meta.bootstrap_keys (
  key_hash    TEXT        PRIMARY KEY,
  created_at  timestamptz NOT NULL DEFAULT now(),
  retired_at  timestamptz
);
CREATE UNIQUE INDEX IF NOT EXISTS bootstrap_keys_one_current
  ON wiretap_meta.bootstrap_keys ((true)) WHERE retired_at IS NULL;
"#;

/// Serialises the meta schema and the bootstrap key's history across gateways
/// sharing a database.
const META_LOCK: i64 = 0x5754_4150_0003;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Read,
    Ingest,
    Admin,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "read" => Some(Role::Read),
            "ingest" => Some(Role::Ingest),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Ingest => "ingest",
            Role::Admin => "admin",
        }
    }

    /// admin ⊇ read; admin ⊇ ingest (an admin key can do anything).
    pub fn allows_read(self) -> bool {
        matches!(self, Role::Read | Role::Admin)
    }

    pub fn allows_ingest(self) -> bool {
        matches!(self, Role::Ingest | Role::Admin)
    }

    pub fn allows_admin(self) -> bool {
        matches!(self, Role::Admin)
    }
}

#[derive(Debug, Clone)]
pub struct KeyInfo {
    pub id: i64,
    pub name: String,
    pub role: Role,
    pub database_pin: Option<String>,
    /// Set when an ApplianceOS console asserted the caller instead of a key.
    pub console: Option<ConsoleUser>,
}

#[derive(Debug, Clone)]
pub struct ConsoleUser {
    pub name: String,
    /// When the console last saw this user prove their password.
    pub reauthenticated_at: Option<DateTime<Utc>>,
}

pub enum KeyRotation {
    Rotated(String),
    NotFound,
    Disabled,
}

#[derive(Debug, Serialize)]
pub struct KeySummary {
    pub id: i64,
    pub name: String,
    pub role: Role,
    pub database_pin: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub disabled: bool,
}

fn hash_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

pub fn new_key() -> String {
    format!("wt_{}", secrets::random_hex())
}

/// What `wiretap_meta.bootstrap_keys` makes of the configured admin key.
#[derive(Debug, PartialEq)]
enum Standing {
    Current,
    Retired {
        at: DateTime<Utc>,
        current: Option<String>,
    },
    /// A deliberate break-glass reset: it becomes the current one.
    Unknown,
}

/// `rows` holds the configured key's row, if any, and the current one.
fn standing(configured: &str, rows: &[(String, Option<DateTime<Utc>>)]) -> Standing {
    match rows.iter().find(|(hash, _)| hash == configured) {
        None => Standing::Unknown,
        Some((_, None)) => Standing::Current,
        Some((_, Some(at))) => Standing::Retired {
            at: *at,
            current: rows
                .iter()
                .find(|(_, retired)| retired.is_none())
                .map(|(hash, _)| hash.clone()),
        },
    }
}

async fn meta_transaction(
    client: &mut tokio_postgres::Client,
) -> Result<tokio_postgres::Transaction<'_>, tokio_postgres::Error> {
    let tx = client.transaction().await?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&META_LOCK])
        .await?;
    Ok(tx)
}

/// Retires the current bootstrap key, and `outgoing` if it has no row yet.
async fn make_current(
    tx: &tokio_postgres::Transaction<'_>,
    outgoing: Option<&str>,
    hash: &str,
) -> Result<(), tokio_postgres::Error> {
    tx.execute(
        "UPDATE wiretap_meta.bootstrap_keys SET retired_at = now() WHERE retired_at IS NULL",
        &[],
    )
    .await?;
    if let Some(outgoing) = outgoing {
        tx.execute(
            "INSERT INTO wiretap_meta.bootstrap_keys (key_hash, retired_at) VALUES ($1, now()) \
             ON CONFLICT DO NOTHING",
            &[&outgoing],
        )
        .await?;
    }
    tx.execute(
        "INSERT INTO wiretap_meta.bootstrap_keys (key_hash) VALUES ($1)",
        &[&hash],
    )
    .await?;
    Ok(())
}

/// Hashes of the admin key the gateway was started with, and the one it honours.
struct Bootstrap {
    configured: Option<String>,
    current: Option<String>,
}

#[derive(Clone)]
pub struct KeyStore {
    dbs: Databases,
    bootstrap: Arc<std::sync::RwLock<Bootstrap>>,
    /// key_hash -> KeyInfo for enabled keys.
    cache: Arc<RwLock<HashMap<String, KeyInfo>>>,
}

impl KeyStore {
    pub fn new(dbs: Databases, bootstrap_admin_key: Option<&str>) -> Self {
        let configured = bootstrap_admin_key.map(hash_key);
        Self {
            dbs,
            bootstrap: Arc::new(std::sync::RwLock::new(Bootstrap {
                current: configured.clone(),
                configured,
            })),
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// The configured admin key was rotated away, so the deployment should be
    /// given the current one.
    pub fn stale_config(&self) -> bool {
        let bootstrap = self.bootstrap.read().unwrap();
        bootstrap.configured.is_some() && bootstrap.configured != bootstrap.current
    }

    pub fn bootstrap_configured(&self) -> bool {
        self.bootstrap.read().unwrap().configured.is_some()
    }

    /// A new bootstrap key, recorded as current so it outlives a restart, and
    /// saved in `file` when given. The old one stops working at once. `None`
    /// when no admin key is configured: a start ignores the record then, so a
    /// new key would not outlive a restart.
    pub async fn rotate_bootstrap_key(
        &self,
        file: Option<&Path>,
    ) -> Result<Option<String>, String> {
        let failed = |e| format!("admin key rotation failed: {e}");
        let outgoing = {
            let bootstrap = self.bootstrap.read().unwrap();
            if bootstrap.configured.is_none() {
                return Ok(None);
            }
            bootstrap.current.clone()
        };
        let key = new_key();
        let hash = hash_key(&key);
        let mut client = self.dbs.connect_raw(self.dbs.default_database()).await?;
        let tx = meta_transaction(&mut client).await.map_err(failed)?;
        make_current(&tx, outgoing.as_deref(), &hash)
            .await
            .map_err(failed)?;
        if let Some(file) = file {
            secrets::persist(file, &key)?;
        }
        tx.commit().await.map_err(failed)?;
        let mut bootstrap = self.bootstrap.write().unwrap();
        if file.is_some() {
            bootstrap.configured = Some(hash.clone());
        }
        bootstrap.current = Some(hash);
        Ok(Some(key))
    }

    /// Create the meta schema, settle which admin key is honoured, and warm
    /// the cache. Call once at startup, after the default database exists.
    pub async fn bootstrap(&self) -> Result<(), String> {
        let failed = |e| format!("meta schema failed: {e}");
        let mut client = self.dbs.connect_raw(self.dbs.default_database()).await?;
        let tx = meta_transaction(&mut client).await.map_err(failed)?;
        tx.batch_execute(META_SCHEMA).await.map_err(failed)?;
        let configured = self.bootstrap.read().unwrap().configured.clone();
        let mut honoured = configured.clone();
        if let Some(configured) = &configured {
            let rows: Vec<_> = tx
                .query(
                    "SELECT key_hash, retired_at FROM wiretap_meta.bootstrap_keys \
                     WHERE key_hash = $1 OR retired_at IS NULL",
                    &[configured],
                )
                .await
                .map_err(failed)?
                .iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect();
            match standing(configured, &rows) {
                Standing::Current => {}
                Standing::Retired { at, current } => {
                    tracing::warn!(
                        "the configured admin key was rotated away on {}: set WIRETAP_ADMIN_KEY \
                         (or the key file) to the current key",
                        at.format("%Y-%m-%d %H:%M UTC")
                    );
                    honoured = current;
                }
                Standing::Unknown => {
                    make_current(&tx, None, configured).await.map_err(failed)?;
                    tracing::info!("the configured admin key is new: it replaces the previous one");
                }
            }
        }
        tx.commit().await.map_err(failed)?;
        self.bootstrap.write().unwrap().current = honoured;
        self.reload().await
    }

    pub async fn reload(&self) -> Result<(), String> {
        let client = self.dbs.connect_raw(self.dbs.default_database()).await?;
        let rows = client
            .query(
                "SELECT id, name, key_hash, role, database_pin \
                 FROM wiretap_meta.api_keys WHERE disabled_at IS NULL",
                &[],
            )
            .await
            .map_err(|e| format!("key load failed: {e}"))?;
        let mut map = HashMap::new();
        for row in &rows {
            let role = Role::parse(row.get::<_, &str>("role")).unwrap_or(Role::Read);
            map.insert(
                row.get::<_, String>("key_hash"),
                KeyInfo {
                    id: row.get("id"),
                    name: row.get("name"),
                    role,
                    database_pin: row.get("database_pin"),
                    console: None,
                },
            );
        }
        *self.cache.write().await = map;
        Ok(())
    }

    /// Validate a plaintext key. Comparison is hash-to-hash, so timing leaks
    /// nothing about the stored keys.
    pub async fn validate(&self, key: &str) -> Option<KeyInfo> {
        let h = hash_key(key);
        if self.bootstrap.read().unwrap().current.as_deref() == Some(h.as_str()) {
            return Some(KeyInfo {
                id: 0,
                name: "bootstrap-admin".into(),
                role: Role::Admin,
                database_pin: None,
                console: None,
            });
        }
        let info = self.cache.read().await.get(&h).cloned()?;
        self.touch_last_used(info.id);
        Some(info)
    }

    /// Fire-and-forget last_used update (best effort, never blocks auth).
    fn touch_last_used(&self, id: i64) {
        let dbs = self.dbs.clone();
        tokio::spawn(async move {
            if let Ok(client) = dbs.connect_raw(dbs.default_database()).await {
                let _ = client
                    .execute(
                        "UPDATE wiretap_meta.api_keys SET last_used_at = now() \
                         WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - INTERVAL '1 minute')",
                        &[&id],
                    )
                    .await;
            }
        });
    }

    /// Create a key; returns the plaintext (shown once, never stored).
    pub async fn create(
        &self,
        name: &str,
        role: Role,
        database_pin: Option<&str>,
    ) -> Result<(i64, String), String> {
        let plaintext = new_key();
        let client = self.dbs.connect_raw(self.dbs.default_database()).await?;
        let row = client
            .query_one(
                "INSERT INTO wiretap_meta.api_keys (name, key_hash, role, database_pin) \
                 VALUES ($1, $2, $3, $4) RETURNING id",
                &[&name, &hash_key(&plaintext), &role.as_str(), &database_pin],
            )
            .await
            .map_err(|e| format!("key create failed: {e}"))?;
        self.reload().await?;
        Ok((row.get(0), plaintext))
    }

    /// False when there is no such key, or it is already that way.
    pub async fn set_disabled(&self, id: i64, disabled: bool) -> Result<bool, String> {
        let client = self.dbs.connect_raw(self.dbs.default_database()).await?;
        let n = client
            .execute(
                "UPDATE wiretap_meta.api_keys SET disabled_at = CASE WHEN $2 THEN now() END \
                 WHERE id = $1 AND (disabled_at IS NULL) = $2",
                &[&id, &disabled],
            )
            .await
            .map_err(|e| format!("key update failed: {e}"))?;
        self.reload().await?;
        Ok(n > 0)
    }

    /// A new plaintext for the same key row, returned once; the old one stops
    /// working at once.
    pub async fn rotate(&self, id: i64) -> Result<KeyRotation, String> {
        let plaintext = new_key();
        let client = self.dbs.connect_raw(self.dbs.default_database()).await?;
        let row = client
            .query_opt(
                "WITH found AS (SELECT disabled_at IS NOT NULL AS disabled \
                   FROM wiretap_meta.api_keys WHERE id = $1), \
                 rotated AS (UPDATE wiretap_meta.api_keys SET key_hash = $2 \
                   WHERE id = $1 AND disabled_at IS NULL RETURNING id) \
                 SELECT disabled FROM found",
                &[&id, &hash_key(&plaintext)],
            )
            .await
            .map_err(|e| format!("key rotate failed: {e}"))?;
        self.reload().await?;
        Ok(match row.map(|r| r.get::<_, bool>(0)) {
            None => KeyRotation::NotFound,
            Some(true) => KeyRotation::Disabled,
            Some(false) => KeyRotation::Rotated(plaintext),
        })
    }

    /// `pin` is `Some(None)` to clear the pin, and `None` to leave it.
    pub async fn edit(
        &self,
        id: i64,
        name: Option<&str>,
        pin: Option<Option<&str>>,
    ) -> Result<bool, String> {
        let client = self.dbs.connect_raw(self.dbs.default_database()).await?;
        let n = client
            .execute(
                "UPDATE wiretap_meta.api_keys SET name = COALESCE($2, name), \
                 database_pin = CASE WHEN $3 THEN $4 ELSE database_pin END \
                 WHERE id = $1",
                &[&id, &name, &pin.is_some(), &pin.flatten()],
            )
            .await
            .map_err(|e| format!("key edit failed: {e}"))?;
        self.reload().await?;
        Ok(n > 0)
    }

    /// Permanently delete a key row.
    pub async fn delete(&self, id: i64) -> Result<bool, String> {
        let client = self.dbs.connect_raw(self.dbs.default_database()).await?;
        let n = client
            .execute("DELETE FROM wiretap_meta.api_keys WHERE id = $1", &[&id])
            .await
            .map_err(|e| format!("key delete failed: {e}"))?;
        self.reload().await?;
        Ok(n > 0)
    }

    pub async fn list(&self) -> Result<Vec<KeySummary>, String> {
        let client = self.dbs.connect_raw(self.dbs.default_database()).await?;
        let rows = client
            .query(
                "SELECT id, name, role, database_pin, created_at, last_used_at, disabled_at \
                 FROM wiretap_meta.api_keys ORDER BY id",
                &[],
            )
            .await
            .map_err(|e| format!("key list failed: {e}"))?;
        Ok(rows
            .iter()
            .map(|r| KeySummary {
                id: r.get("id"),
                name: r.get("name"),
                role: Role::parse(r.get::<_, &str>("role")).unwrap_or(Role::Read),
                database_pin: r.get("database_pin"),
                created_at: r.get("created_at"),
                last_used_at: r.get("last_used_at"),
                disabled: r.get::<_, Option<DateTime<Utc>>>("disabled_at").is_some(),
            })
            .collect())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::tests::{live_databases, run};

    /// Over a database of its own, so no other test's keys are in the store.
    pub(crate) async fn own_key_database(prefix: &str) -> Databases {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("{prefix}_{stamp}");
        run("postgres", &format!("CREATE DATABASE {name}")).await;
        let mut config = live_databases(false).config().clone();
        config.default_database = name;
        Databases::new(Arc::new(config))
    }

    #[test]
    fn the_configured_admin_key_is_current_retired_or_a_reset() {
        let at = Utc::now();
        let row = |hash: &str, retired| (hash.to_string(), retired);
        assert_eq!(standing("c", &[row("c", None)]), Standing::Current);
        assert_eq!(
            standing("r", &[row("r", Some(at)), row("c", None)]),
            Standing::Retired {
                at,
                current: Some("c".into())
            }
        );
        assert_eq!(
            standing("r", &[row("r", Some(at))]),
            Standing::Retired { at, current: None }
        );
        assert_eq!(standing("n", &[row("c", None)]), Standing::Unknown);
        assert_eq!(standing("n", &[]), Standing::Unknown);
    }

    #[tokio::test]
    async fn with_no_admin_key_configured_there_is_none_to_rotate() {
        let store = KeyStore::new(crate::db::tests::unreachable_databases(false), None);
        assert!(!store.bootstrap_configured());
        assert_eq!(store.rotate_bootstrap_key(None).await, Ok(None));
    }

    async fn current_bootstrap_keys(dbs: &Databases) -> i64 {
        let client = dbs.connect_raw(dbs.default_database()).await.unwrap();
        client
            .query_one(
                "SELECT count(*) FROM wiretap_meta.bootstrap_keys WHERE retired_at IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get(0)
    }

    async fn restarted(dbs: &Databases, configured: Option<&str>) -> KeyStore {
        let store = KeyStore::new(dbs.clone(), configured);
        store.bootstrap().await.unwrap();
        store
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn an_admin_key_rotated_from_the_environment_outlives_a_restart_with_the_old_env() {
        let dbs = own_key_database("p10_admin_restart").await;
        let first = restarted(&dbs, Some("old")).await;
        let rotated = first.rotate_bootstrap_key(None).await.unwrap().unwrap();
        assert!(first.stale_config());

        let old_env = restarted(&dbs, Some("old")).await;
        assert!(old_env.validate("old").await.is_none());
        assert!(old_env.validate(&rotated).await.is_some());
        assert!(old_env.stale_config());

        let fresh_env = restarted(&dbs, Some("fresh")).await;
        assert!(fresh_env.validate("fresh").await.is_some());
        assert!(fresh_env.validate(&rotated).await.is_none());
        assert!(fresh_env.validate("old").await.is_none());
        assert!(!fresh_env.stale_config());
        assert_eq!(current_bootstrap_keys(&dbs).await, 1);

        let no_env = restarted(&dbs, None).await;
        assert!(no_env.validate("fresh").await.is_none());
        assert!(!no_env.stale_config());
        assert!(restarted(&dbs, Some("fresh"))
            .await
            .validate("fresh")
            .await
            .is_some());
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn two_gateways_starting_with_a_new_admin_key_record_one_current() {
        let dbs = own_key_database("p10_admin_race").await;
        for key in ["old", "new"] {
            let (a, b) = tokio::join!(restarted(&dbs, Some(key)), restarted(&dbs, Some(key)));
            assert_eq!(current_bootstrap_keys(&dbs).await, 1);
            assert!(a.validate(key).await.is_some() && b.validate(key).await.is_some());
        }
        assert!(restarted(&dbs, Some("old"))
            .await
            .validate("old")
            .await
            .is_none());
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn an_admin_key_rotated_into_its_file_is_the_one_a_restart_reads() {
        let dbs = own_key_database("p10_admin_keyfile").await;
        let dir = std::env::temp_dir().join(format!("wiretap-keyfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("admin-key");
        std::fs::write(&file, "from-file").unwrap();

        let first = restarted(&dbs, Some("from-file")).await;
        let rotated = first
            .rotate_bootstrap_key(Some(&file))
            .await
            .unwrap()
            .unwrap();
        assert!(!first.stale_config());
        let saved = secrets::read(&file).unwrap();
        assert_eq!(saved.expose(), rotated);

        let again = restarted(&dbs, Some(saved.expose())).await;
        assert!(again.validate(&rotated).await.is_some());
        assert!(again.validate("from-file").await.is_none());
        assert!(!again.stale_config());
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_store_from_before_the_rename_keeps_its_rows_and_which_are_disabled() {
        let dbs = own_key_database("p10_keys_old").await;
        run(
            dbs.default_database(),
            &(META_SCHEMA
                .split("DO $$")
                .next()
                .unwrap()
                .replace("disabled_at", "revoked_at")
                + "INSERT INTO wiretap_meta.api_keys (name, key_hash, role, revoked_at) VALUES \
                   ('live', 'h1', 'read', NULL), ('gone', 'h2', 'admin', now());"),
        )
        .await;

        let store = KeyStore::new(dbs.clone(), None);
        store.bootstrap().await.unwrap();
        store.bootstrap().await.unwrap();
        let listed: Vec<_> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|k| (k.name, k.disabled))
            .collect();
        assert_eq!(listed, [("live".into(), false), ("gone".into(), true)]);
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_fresh_store_disables_and_enables_a_key() {
        let store = KeyStore::new(own_key_database("p10_keys_new").await, None);
        store.bootstrap().await.unwrap();
        store.bootstrap().await.unwrap();
        let (id, key) = store.create("k", Role::Read, None).await.unwrap();

        assert!(store.set_disabled(id, true).await.unwrap());
        assert!(!store.set_disabled(id, true).await.unwrap());
        assert!(store.validate(&key).await.is_none());
        assert!(store.list().await.unwrap()[0].disabled);
        assert!(store.set_disabled(id, false).await.unwrap());
        assert!(store.validate(&key).await.is_some());
    }
}
