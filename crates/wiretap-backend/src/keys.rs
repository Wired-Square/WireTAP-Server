//! API key store. Keys live in `wiretap_meta.api_keys` in the default
//! database (sha256-hashed; the plaintext is shown once at creation).
//! A bootstrap admin key, from the environment or its file, is always honoured
//! so the stack can never lock itself out.

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
"#;

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
    pub console_user: Option<String>,
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

#[derive(Clone)]
pub struct KeyStore {
    dbs: Databases,
    bootstrap_hash: Arc<std::sync::RwLock<Option<String>>>,
    /// key_hash -> KeyInfo for enabled keys.
    cache: Arc<RwLock<HashMap<String, KeyInfo>>>,
}

impl KeyStore {
    pub fn new(dbs: Databases, bootstrap_admin_key: Option<&str>) -> Self {
        Self {
            dbs,
            bootstrap_hash: Arc::new(std::sync::RwLock::new(bootstrap_admin_key.map(hash_key))),
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// The old bootstrap key stops working at once.
    pub fn set_bootstrap_key(&self, key: &str) {
        *self.bootstrap_hash.write().unwrap() = Some(hash_key(key));
    }

    /// A new bootstrap key, persisted when `file` is given.
    pub fn rotate_bootstrap_key(&self, file: Option<&Path>) -> Result<String, String> {
        let key = new_key();
        if let Some(file) = file {
            secrets::persist(file, &key)?;
        }
        self.set_bootstrap_key(&key);
        Ok(key)
    }

    /// Create the meta schema and warm the cache. Call once at startup,
    /// after the default database exists.
    pub async fn bootstrap(&self) -> Result<(), String> {
        let client = self.dbs.connect_raw(self.dbs.default_database()).await?;
        client
            .batch_execute(META_SCHEMA)
            .await
            .map_err(|e| format!("meta schema failed: {e}"))?;
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
                    console_user: None,
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
        if self.bootstrap_hash.read().unwrap().as_deref() == Some(h.as_str()) {
            return Some(KeyInfo {
                id: 0,
                name: "bootstrap-admin".into(),
                role: Role::Admin,
                database_pin: None,
                console_user: None,
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
