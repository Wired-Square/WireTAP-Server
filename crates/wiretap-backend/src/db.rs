//! Connection-pool registry: one deadpool pool per capture database, created
//! lazily. Database names are strictly validated before they reach a DSN or
//! SQL, and existence is checked against pg_database.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use tokio::sync::Mutex;
use tokio_postgres::NoTls;

use crate::config::Config;
use crate::schema;

/// Where a capture database stands relative to [`schema::SCHEMA_VERSION`].
///
/// Only `Current` serves *capture data*. The gate is `Databases::pool`, so the
/// key store — which lives in the default database but goes through
/// `connect_raw` — is deliberately outside it; it must work for the gateway to
/// authenticate anything at all.
///
/// Refusing the rest is what makes a migration safe under a live capture. A
/// capture server refused at HELLO reads it as a sink failure, and `Batcher::fail`
/// treats any sink failure as an outage: cache to disk, reconnect, drain. That
/// is the same path a gateway restart takes, and it is drilled.
#[derive(Clone, Debug)]
pub enum DbSchemaState {
    Current { version: i32 },
    Pending { version: i32 },
    Migrating { since: Instant },
    Failed { version: i32, error: String },
}

impl DbSchemaState {
    fn current() -> Self {
        Self::Current {
            version: schema::SCHEMA_VERSION,
        }
    }

    /// The word the API and the UI use for this state.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Current { .. } => "current",
            Self::Pending { .. } => "pending",
            Self::Migrating { .. } => "migrating",
            Self::Failed { .. } => "failed",
        }
    }

    pub fn version(&self) -> Option<i32> {
        match self {
            Self::Current { version }
            | Self::Pending { version }
            | Self::Failed { version, .. } => Some(*version),
            Self::Migrating { .. } => None,
        }
    }

    pub fn busy_secs(&self) -> Option<u64> {
        match self {
            Self::Migrating { since } => Some(since.elapsed().as_secs()),
            _ => None,
        }
    }
}

pub fn valid_db_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[derive(Clone)]
pub struct Databases {
    config: Arc<Config>,
    pools: Arc<Mutex<HashMap<String, Pool>>>,
    /// Serialises CREATE DATABASE *and* migration races between concurrent
    /// first connections — two callers must not both run `ALTER TABLE … RENAME`.
    create_lock: Arc<Mutex<()>>,
    schema_state: Arc<Mutex<HashMap<String, DbSchemaState>>>,
    /// Databases whose hourly rollup is being materialised, and since when.
    /// Deliberately *not* a `DbSchemaState`: a rebuild does not change what
    /// version a database is at, and the reads it would otherwise block are
    /// exactly the ones it exists to make fast.
    rollup_rebuild: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Databases {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            pools: Arc::new(Mutex::new(HashMap::new())),
            create_lock: Arc::new(Mutex::new(())),
            schema_state: Arc::new(Mutex::new(HashMap::new())),
            rollup_rebuild: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn schema_states(&self) -> HashMap<String, DbSchemaState> {
        self.schema_state.lock().await.clone()
    }

    /// Seconds each in-progress rollup rebuild has been running.
    pub async fn rollup_rebuilds(&self) -> HashMap<String, u64> {
        self.rollup_rebuild
            .lock()
            .await
            .iter()
            .map(|(k, since)| (k.clone(), since.elapsed().as_secs()))
            .collect()
    }

    async fn set_state(&self, name: &str, state: DbSchemaState) {
        // Dropping the pool is what makes the gate bite on a *live* capture.
        // `pool()` is entered once per session, not per batch, so an ingest
        // client that connected before a migration would otherwise keep COPYing
        // through it. Dropping the pool forces the next batch back through
        // `require_current`, and discards prepared statements whose plans the
        // rename has just invalidated.
        if !matches!(state, DbSchemaState::Current { .. }) {
            self.pools.lock().await.remove(name);
        }
        self.schema_state
            .lock()
            .await
            .insert(name.to_string(), state);
    }

    /// Names only, for a log line.
    fn names_of(behind: &[(String, i32)]) -> String {
        behind
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Every capture database on the cluster, in name order.
    pub async fn list_names(&self) -> Result<Vec<String>, String> {
        let client = self.connect_raw("postgres").await?;
        let rows = client
            .query(
                "SELECT datname FROM pg_database \
                 WHERE NOT datistemplate AND datname <> 'postgres' ORDER BY datname",
                &[],
            )
            .await
            .map_err(|e| format!("database list failed: {e}"))?;
        Ok(rows
            .iter()
            .map(|r| r.get::<_, String>(0))
            .filter(|n| valid_db_name(n))
            .collect())
    }

    /// Bring one database to the current schema, probing it first.
    /// Idempotent, and safe to call on a database that is already current.
    pub async fn migrate_one(&self, name: &str) -> Result<(), String> {
        let at = self.probe_version(name).await?;
        self.migrate_from(name, at).await
    }

    /// The migration proper, given a version the caller has already established.
    /// Split from [`Self::migrate_one`] so the sweep's probe pass is not thrown
    /// away and asked again.
    async fn migrate_from(&self, name: &str, at: Option<i32>) -> Result<(), String> {
        // A database already at the current version is *not* marked `Migrating`:
        // that would evict its pool and refuse its reads on every sweep. But the
        // schema is still re-applied, because `apply_capture_schema` is
        // IF NOT EXISTS throughout and is what repairs a database whose first run
        // died partway. The version row is stamped last in that file, so "v1"
        // means it finished, and re-running it costs a few milliseconds.
        let migrating = at != Some(schema::SCHEMA_VERSION);
        let _guard = self.create_lock.lock().await;
        let since = Instant::now();
        if migrating {
            self.set_state(name, DbSchemaState::Migrating { since })
                .await;
        }

        let result = async {
            let client = self.connect_raw(name).await?;
            schema::migrate(&client, at).await?;
            // The rollup MUST be backfilled before the maintenance policy runs,
            // and this is not a performance nicety — it is correctness.
            //
            // The migration recreates the aggregate empty. Its policy then
            // materialises only a recent window and advances the watermark past
            // it, and a real-time aggregate serves buckets *below* the watermark
            // from the materialisation table alone — it does not recompute the
            // gaps, it omits them. Measured: a 5 760-frame archive reported 121
            // after one policy run. On an idle archive the policy writes nothing
            // and the fault never appears, which is why it survives testing and
            // waits for a live capture.
            if migrating {
                schema::refresh_rollup(&client).await?;
            }
            Ok::<(), String>(())
        }
        .await;

        match &result {
            Ok(()) => {
                if migrating && at.is_some() {
                    tracing::warn!(
                        database = name,
                        from = at,
                        to = schema::SCHEMA_VERSION,
                        elapsed_ms = since.elapsed().as_millis() as u64,
                        "schema migrated and the hourly rollup rebuilt"
                    );
                }
                self.set_state(name, DbSchemaState::current()).await;
            }
            Err(e) => {
                tracing::error!(database = name, "schema migration failed: {e}");
                self.set_state(
                    name,
                    DbSchemaState::Failed {
                        version: at.unwrap_or(0),
                        error: e.clone(),
                    },
                )
                .await;
            }
        }
        result
    }

    /// Sweep every capture database. Spawned after the listeners bind, so a slow
    /// migration delays ingest for the database being migrated and nothing else.
    ///
    /// Two passes on purpose: probe them all first so the admin UI shows the
    /// whole queue at once — `v0 → v1` against the ones still waiting — instead
    /// of revealing it one database at a time as the sweep reaches each.
    pub async fn migrate_all(&self) {
        let names = match self.list_names().await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("schema sweep could not list databases: {e}");
                return;
            }
        };
        // `ours` is every capture database, `behind` only those needing work.
        // Both get the schema re-applied: an archive migrated before the version
        // table existed has no `schema_version` to stamp until this runs, and
        // re-applying is IF NOT EXISTS throughout and costs milliseconds.
        let mut ours = Vec::new();
        let mut behind = Vec::new();
        for name in &names {
            match self.probe_version(name).await {
                // Carries no capture schema, so it is not ours. A cluster can
                // hold databases nothing to do with this gateway, and creating
                // hypertables in them would be vandalism, not migration.
                Ok(None) => continue,
                Ok(Some(v)) => {
                    if v == schema::SCHEMA_VERSION {
                        self.set_state(name, DbSchemaState::Current { version: v })
                            .await;
                    } else {
                        self.set_state(name, DbSchemaState::Pending { version: v })
                            .await;
                        behind.push((name.clone(), v));
                    }
                    ours.push((name.clone(), v));
                }
                Err(e) => {
                    self.set_state(
                        name,
                        DbSchemaState::Failed {
                            version: 0,
                            error: e,
                        },
                    )
                    .await;
                }
            }
        }
        if !behind.is_empty() && !self.config.auto_migrate {
            tracing::warn!(
                databases = Self::names_of(&behind),
                "behind schema v{} and WIRETAP_AUTO_MIGRATE is off; they will refuse \
                 reads and writes until schema/migrations/*.sql is applied by hand",
                schema::SCHEMA_VERSION
            );
            return;
        }
        if !behind.is_empty() {
            tracing::warn!(
                databases = Self::names_of(&behind),
                "migrating to schema v{}; these refuse reads and writes until done",
                schema::SCHEMA_VERSION
            );
        }
        // Every capture database, not only the ones behind — each carries the
        // version the probe above already found, so nothing is asked twice.
        for (name, at) in &ours {
            let _ = self.migrate_from(name, Some(*at)).await;
        }

        // Then repair any rollup that is empty despite the database holding
        // frames — for archives that arrived another way: restored from a backup,
        // migrated by an older build, or left by a refresh that failed.
        //
        // It is not cosmetic. Such an archive reads correctly only while it is
        // idle: the maintenance policy has nothing to write, so the watermark
        // stays put and every query falls through to the raw table. The first
        // frame ingested lets the policy advance the watermark over the gap, and
        // from then on the rollup omits everything older.
        //
        // Skipping the ones just migrated, which backfilled on their way through:
        // probing those again is a connection and a catalogue query to be told
        // what this loop already knows.
        let migrated: std::collections::HashSet<&str> =
            behind.iter().map(|(n, _)| n.as_str()).collect();
        for (name, _) in ours.iter().filter(|(n, _)| !migrated.contains(n.as_str())) {
            self.repair_rollup_if_incomplete(name).await;
        }
    }

    async fn repair_rollup_if_incomplete(&self, name: &str) {
        let client = match self.connect_raw(name).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(database = name, "rollup check could not connect: {e}");
                return;
            }
        };
        match schema::rollup_status(&client).await {
            Ok(schema::RollupState::Incomplete) => {
                tracing::warn!(
                    database = name,
                    "rollup does not cover this archive from its first frame; \
                     materialising it now — left alone, queries omit the uncovered span"
                );
                // Through the pooled entry point, not `schema::refresh_rollup`
                // directly: that records the rebuild so the admin UI shows it and
                // an operator cannot start a second one on top of it, which the
                // database refuses with a concurrent-refresh error.
                let _ = self.refresh_rollup(name).await;
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(database = name, "could not check the rollup: {e}"),
        }
    }

    async fn probe_version(&self, name: &str) -> Result<Option<i32>, String> {
        let client = self.connect_raw(name).await?;
        schema::detect_version(&client).await
    }

    /// Materialise the hourly rollup over its whole range, recording it so the
    /// admin UI can show it and a second one cannot start on top.
    ///
    /// Also run automatically — by a migration, and by the startup sweep when it
    /// finds an archive the rollup does not cover. This entry point is the
    /// operator's button, for a rollup that has merely fallen behind.
    ///
    /// Does not block reads or writes. Note that an aggregate which is *partly*
    /// materialised under-reports rather than merely running slowly — buckets
    /// below the watermark come from the materialisation table alone — so this
    /// always refreshes the whole range, never a window.
    pub async fn refresh_rollup(&self, name: &str) -> Result<(), String> {
        if !valid_db_name(name) {
            return Err(format!("invalid database name '{name}'"));
        }
        let started = Instant::now();
        self.rollup_rebuild
            .lock()
            .await
            .insert(name.to_string(), started);
        let result = async {
            let client = self.connect_raw(name).await?;
            schema::refresh_rollup(&client).await
        }
        .await;
        self.rollup_rebuild.lock().await.remove(name);
        match &result {
            Ok(()) => tracing::info!(
                database = name,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "hourly rollup materialised"
            ),
            Err(e) => tracing::error!(database = name, "rollup refresh failed: {e}"),
        }
        result
    }

    pub fn default_database(&self) -> &str {
        &self.config.default_database
    }

    fn build_pool(&self, database: &str) -> Result<Pool, String> {
        let pg_config: tokio_postgres::Config = self
            .config
            .pg_dsn(database)
            .parse()
            .map_err(|e| format!("bad DSN: {e}"))?;
        let mgr = Manager::from_config(
            pg_config,
            NoTls,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        Pool::builder(mgr)
            .max_size(8)
            .build()
            .map_err(|e| format!("pool build failed: {e}"))
    }

    /// One-off (non-pooled) connection, used for CREATE DATABASE and bootstrap.
    pub async fn connect_raw(&self, database: &str) -> Result<tokio_postgres::Client, String> {
        let (client, connection) = tokio_postgres::connect(&self.config.pg_dsn(database), NoTls)
            .await
            .map_err(|e| format!("connect to '{database}' failed: {e}"))?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::debug!("postgres connection closed: {e}");
            }
        });
        Ok(client)
    }

    pub async fn database_exists(&self, name: &str) -> Result<bool, String> {
        let client = self.connect_raw("postgres").await?;
        let row = client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)",
                &[&name],
            )
            .await
            .map_err(|e| format!("pg_database lookup failed: {e}"))?;
        Ok(row.get(0))
    }

    /// Create a capture database (idempotent) and bring it to the current
    /// schema. Safe on a database that already exists and is behind.
    pub async fn create_database(&self, name: &str) -> Result<(), String> {
        if !valid_db_name(name) {
            return Err(format!("invalid database name '{name}'"));
        }
        {
            // Scoped: migrate_one takes this same lock, and holding it across
            // that call would deadlock.
            let _guard = self.create_lock.lock().await;
            if !self.database_exists(name).await? {
                let client = self.connect_raw("postgres").await?;
                // CREATE DATABASE is non-transactional; name is validated above.
                client
                    .batch_execute(&format!("CREATE DATABASE {name}"))
                    .await
                    .map_err(|e| format!("CREATE DATABASE {name} failed: {e}"))?;
                tracing::info!("created database '{name}'");
            }
        }
        self.migrate_one(name).await
    }

    /// Drop a capture database (admin). Refuses the default database (it holds
    /// the API-key store). Removes the pool first, then DROP ... WITH (FORCE)
    /// to terminate any lingering query connections. Callers must ensure it is
    /// not actively being ingested.
    pub async fn delete_database(&self, name: &str) -> Result<(), String> {
        if !valid_db_name(name) {
            return Err(format!("invalid database name '{name}'"));
        }
        if name == self.config.default_database {
            return Err("cannot delete the default database".into());
        }
        self.pools.lock().await.remove(name);
        self.schema_state.lock().await.remove(name);
        self.rollup_rebuild.lock().await.remove(name);
        if !self.database_exists(name).await? {
            return Ok(());
        }
        let client = self.connect_raw("postgres").await?;
        client
            .batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .await
            .map_err(|e| format!("DROP DATABASE {name} failed: {e}"))?;
        tracing::info!("dropped database '{name}'");
        Ok(())
    }

    /// Pool for an existing capture database. Errors if it doesn't exist —
    /// callers wanting auto-create go through `ensure_database` first.
    pub async fn pool(&self, name: &str) -> Result<Pool, String> {
        if !valid_db_name(name) {
            return Err(format!("invalid database name '{name}'"));
        }
        {
            let pools = self.pools.lock().await;
            if let Some(pool) = pools.get(name) {
                return Ok(pool.clone());
            }
        }
        // Existence before readiness: a typo should say "does not exist" rather
        // than fail deep inside a migration attempt against a missing database.
        if !self.database_exists(name).await? {
            return Err(format!("database '{name}' does not exist"));
        }
        self.require_current(name).await?;
        let pool = self.build_pool(name)?;
        self.pools
            .lock()
            .await
            .insert(name.to_string(), pool.clone());
        Ok(pool)
    }

    /// Refuse anything not at the current schema. A database never seen before —
    /// restored from a backup, or created between sweeps — is migrated here
    /// rather than left permanently unusable, unless automatic migration is off.
    async fn require_current(&self, name: &str) -> Result<(), String> {
        let known = self.schema_state.lock().await.get(name).cloned();
        match known {
            Some(DbSchemaState::Current { .. }) => Ok(()),
            Some(state) => Err(format!(
                "database '{name}' is not ready: schema {}",
                state.label()
            )),
            None if self.config.auto_migrate => {
                // Owned by the process, not by this request: axum drops a
                // handler future when the client disconnects, and a migration
                // dropped mid-flight would leave the state at `Migrating`
                // forever. Start it and answer "not ready" — every caller
                // already handles that, and a capture server caches and retries.
                let this = self.clone();
                let owned = name.to_string();
                tokio::spawn(async move { this.migrate_one(&owned).await });
                Err(format!("database '{name}' is being checked; retry shortly"))
            }
            None => Err(format!(
                "database '{name}' has not been checked and WIRETAP_AUTO_MIGRATE is off"
            )),
        }
    }

    /// Resolve a database for ingest/import: existing, or auto-created when
    /// the config allows. Returns the pool.
    pub async fn ensure_database(&self, name: &str, allow_create: bool) -> Result<Pool, String> {
        if !valid_db_name(name) {
            return Err(format!("invalid database name '{name}'"));
        }
        if !self.database_exists(name).await? {
            if !(allow_create && self.config.auto_create_databases) {
                return Err(format!(
                    "database '{name}' does not exist (auto-create disabled)"
                ));
            }
            self.create_database(name).await?;
        }
        self.pool(name).await
    }
}

#[cfg(test)]
mod tests {
    use super::valid_db_name;

    #[test]
    fn db_name_validation() {
        assert!(valid_db_name("wiretap"));
        assert!(valid_db_name("vehicle_1"));
        assert!(!valid_db_name(""));
        assert!(!valid_db_name("1leading_digit"));
        assert!(!valid_db_name("Has-Caps"));
        assert!(!valid_db_name("name;drop table"));
        assert!(!valid_db_name(&"x".repeat(64)));
    }
}
