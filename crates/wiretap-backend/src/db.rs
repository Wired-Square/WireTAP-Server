//! Connection-pool registry: one deadpool pool per capture database, created
//! lazily. Database names are strictly validated before they reach a DSN or
//! SQL, and existence is checked against pg_database.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use tokio::sync::Mutex;
use tokio_postgres::NoTls;

use crate::config::Config;
use crate::ingest::writer::{self, CopyError, FrameRow, Written};
use crate::schema;

/// Where a capture database stands relative to [`schema::SCHEMA_VERSION`].
///
/// Only `Current` serves reads of *capture data*. The gate is `Databases::pool`,
/// so the key store — which lives in the default database but goes through
/// `connect_raw` — is deliberately outside it; it must work for the gateway to
/// authenticate anything at all.
///
/// Ingest for a database not `Current` is not refused but buffered
/// into `capture_frame_pending`, and drained into `capture_frame` before the
/// database is next `Current`: see [`Databases::write_rows`].
#[derive(Clone, Debug)]
pub enum DbSchemaState {
    Current {
        version: i32,
    },
    Pending {
        version: i32,
    },
    Migrating {
        since: Instant,
    },
    Failed {
        version: i32,
        error: String,
        since: Instant,
    },
}

/// How long a failed database waits before a request may try it again. A NAS
/// can start the gateway before PostgreSQL accepts connections, and the sweep
/// then fails every database it reaches.
const FAILED_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Bounds the startup handshake as well as the TCP connect, which is all
/// tokio-postgres's own `connect_timeout` covers.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

impl DbSchemaState {
    fn current() -> Self {
        Self::Current {
            version: schema::SCHEMA_VERSION,
        }
    }

    fn migrating() -> Self {
        Self::Migrating {
            since: Instant::now(),
        }
    }

    fn failed(version: i32, error: String) -> Self {
        Self::Failed {
            version,
            error,
            since: Instant::now(),
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

pub use wiretap_protocol::ingest::valid_database_name;

/// Why a capture database cannot be served: never, or not yet.
#[derive(Debug)]
pub enum DbError {
    /// An invalid name, or a database that does not exist and will not be made.
    Refused(String),
    /// An outage, or a schema check or migration in progress.
    Unavailable(String),
}

impl From<String> for DbError {
    fn from(message: String) -> Self {
        Self::Unavailable(message)
    }
}

impl From<DbError> for String {
    fn from(e: DbError) -> Self {
        e.to_string()
    }
}

/// Why `start_migration` did not start one.
#[derive(Debug)]
pub enum MigrateRefusal {
    NotFound(String),
    /// Already current, already migrating, or not a capture database.
    Conflict(String),
    Unavailable(String),
}

impl From<String> for MigrateRefusal {
    fn from(message: String) -> Self {
        Self::Unavailable(message)
    }
}

/// Why a batch was not stored.
#[derive(Debug)]
pub enum WriteError {
    Db(DbError),
    Copy(CopyError),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => e.fmt(f),
            Self::Copy(e) => e.fmt(f),
        }
    }
}

/// Where a batch for one database goes.
enum Route {
    Live(Pool),
    Buffered(Pool),
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(m) | Self::Unavailable(m) => f.write_str(m),
        }
    }
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
    /// Pools for buffering ingest, dropped once a database is `Current`: a pool
    /// keeps its idle connections for good.
    buffer_pools: Arc<Mutex<HashMap<String, Pool>>>,
    /// Rows this process has buffered per database since its last drain.
    buffered: Arc<Mutex<HashMap<String, u64>>>,
    connect_timeout: Duration,
}

impl Databases {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            pools: Arc::new(Mutex::new(HashMap::new())),
            create_lock: Arc::new(Mutex::new(())),
            schema_state: Arc::new(Mutex::new(HashMap::new())),
            rollup_rebuild: Arc::new(Mutex::new(HashMap::new())),
            buffer_pools: Arc::new(Mutex::new(HashMap::new())),
            buffered: Arc::new(Mutex::new(HashMap::new())),
            connect_timeout: CONNECT_TIMEOUT,
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
        // Dropping the pool is what makes the gate bite on a *live* capture: the
        // next batch is routed afresh by `write_rows`, and prepared statements
        // whose plans a migration has just invalidated are discarded.
        //
        // Under the pools lock, which `pool_if_current` holds from its check to
        // its insert, so no pool is cached for a state being left.
        let mut pools = self.pools.lock().await;
        if matches!(state, DbSchemaState::Current { .. }) {
            self.buffer_pools.lock().await.remove(name);
        } else {
            pools.remove(name);
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
            .filter(|n| valid_database_name(n))
            .collect())
    }

    /// Bring one database to the current schema, probing it first.
    /// Idempotent, and safe to call on a database that is already current.
    pub async fn migrate_one(&self, name: &str) -> Result<(), String> {
        self.migrate_from(name, None).await
    }

    /// The migration proper, given the version the caller found. Split from
    /// [`Self::migrate_one`] so the sweep's probe pass is not thrown away and
    /// asked again for a database already current. One behind is asked again
    /// under the lock: a run queued behind another may find it finished.
    async fn migrate_from(&self, name: &str, at: Option<i32>) -> Result<(), String> {
        let _guard = self.create_lock.lock().await;
        let at = match at {
            Some(schema::SCHEMA_VERSION) => at,
            _ => match self.probe_version(name).await {
                Ok(at) => at,
                Err(e) => {
                    self.set_state(name, DbSchemaState::failed(at.unwrap_or(0), e.clone()))
                        .await;
                    return Err(e);
                }
            },
        };
        // A database already at the current version is *not* marked `Migrating`:
        // that would evict its pool and refuse its reads on every sweep. But the
        // schema is still re-applied, because `apply_capture_schema` is
        // IF NOT EXISTS throughout and is what repairs a database whose first run
        // died partway. The version row is stamped last in that file, so a
        // current version means it finished, and re-running it costs a few
        // milliseconds.
        let migrating = at != Some(schema::SCHEMA_VERSION);
        let since = Instant::now();
        if migrating {
            self.set_state(name, DbSchemaState::migrating()).await;
        }

        let result = async {
            let mut client = self.connect_raw(name).await?;
            schema::migrate(&client, at).await?;
            if migrating {
                schema::report_progress(&client, "drain", None).await;
            }
            // Into the hypertable before anything reads it, and in every run:
            // a gateway that died between a migration and its drain left the
            // buffer behind.
            let drained = writer::drain_pending(&mut client).await?;
            // A rollup the migration left uncovered MUST be backfilled before
            // the maintenance policy runs, and before this database serves a
            // read — this is not a performance nicety, it is correctness.
            //
            // 0001 recreates the aggregate empty. Its policy then materialises
            // only a recent window and advances the watermark past it, and a
            // real-time aggregate serves buckets *below* the watermark from the
            // materialisation table alone — it does not recompute the gaps, it
            // omits them. Measured: a 5 760-frame archive reported 121 after
            // one policy run. On an idle archive the policy writes nothing and
            // the fault never appears, which is why it survives testing and
            // waits for a live capture.
            //
            // Asked, not assumed: a migration that leaves the aggregate alone
            // (0002 does) must not pay for one that did not.
            if migrating
                && matches!(
                    schema::rollup_status(&client).await?,
                    schema::RollupState::Incomplete
                )
            {
                tracing::warn!(
                    database = name,
                    "the migration left the rollup uncovered; rebuilding it"
                );
                schema::refresh_rollup(&client).await?;
            } else if let Some(span) = &drained {
                schema::refresh_rollup_span(&client, span).await?;
            }
            if migrating {
                schema::report_progress(&client, "done", None).await;
            }
            Ok::<_, String>(drained)
        }
        .await;

        match &result {
            Ok(drained) => {
                if migrating && at.is_some() {
                    tracing::warn!(
                        database = name,
                        from = at,
                        to = schema::SCHEMA_VERSION,
                        elapsed_ms = since.elapsed().as_millis() as u64,
                        "schema migrated"
                    );
                }
                self.note_drained(name, drained.as_ref()).await;
                self.set_state(name, DbSchemaState::current()).await;
            }
            Err(e) => {
                tracing::error!(database = name, "schema migration failed: {e}");
                if migrating {
                    if let Ok(client) = self.connect_raw(name).await {
                        schema::report_progress(&client, "failed", Some(e)).await;
                    }
                }
                self.set_state(name, DbSchemaState::failed(at.unwrap_or(0), e.clone()))
                    .await;
            }
        }
        result.map(|_| ())
    }

    async fn note_drained(&self, name: &str, drained: Option<&writer::Drained>) {
        if let Some(drained) = drained {
            tracing::info!(
                database = name,
                rows = drained.rows,
                "drained the ingest buffered while it was behind"
            );
        }
        self.buffered.lock().await.remove(name);
    }

    /// Rows buffered per database since its last drain.
    pub async fn buffered_rows(&self) -> HashMap<String, u64> {
        self.buffered.lock().await.clone()
    }

    /// Store one batch. A `Current` database takes it into `capture_frame`; one
    /// behind or migrating, into its buffer, which the gateway drains once the
    /// database is current. Reads stay refused meanwhile.
    pub async fn write_rows(&self, name: &str, rows: &[FrameRow]) -> Result<(), WriteError> {
        match self.route(name).await.map_err(WriteError::Db)? {
            Route::Live(pool) => writer::copy_rows(&pool, rows)
                .await
                .map_err(WriteError::Copy),
            Route::Buffered(pool) => match writer::buffer_rows(&pool, rows)
                .await
                .map_err(WriteError::Copy)?
            {
                Written::Buffered => {
                    *self.buffered.lock().await.entry(name.into()).or_default() +=
                        rows.len() as u64;
                    Ok(())
                }
                // Migrated by hand meanwhile: drain it and serve it.
                Written::Live => {
                    let this = self.clone();
                    let owned = name.to_string();
                    tokio::spawn(async move { this.recheck_if_behind(&owned).await });
                    Ok(())
                }
            },
        }
    }

    async fn owes_migration(&self, name: &str, failed_too: bool) -> bool {
        match self.schema_state.lock().await.get(name) {
            Some(DbSchemaState::Pending { .. } | DbSchemaState::Migrating { .. }) => true,
            Some(DbSchemaState::Failed { .. }) => failed_too,
            _ => false,
        }
    }

    /// A failed database is buffered too, but with automatic migration on it
    /// goes through `pool` first, which is what retries it.
    async fn route(&self, name: &str) -> Result<Route, DbError> {
        if !self.owes_migration(name, !self.config.auto_migrate).await {
            match self.pool(name).await {
                Ok(pool) => return Ok(Route::Live(pool)),
                Err(e) if !self.owes_migration(name, true).await => return Err(e),
                Err(_) => {}
            }
        }
        let mut pools = self.buffer_pools.lock().await;
        let pool = match pools.get(name) {
            Some(pool) => pool.clone(),
            None => {
                let pool = self.build_pool(name)?;
                pools.insert(name.to_string(), pool.clone());
                pool
            }
        };
        Ok(Route::Buffered(pool))
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
        // Both get the schema re-applied — `migrate_from` says why that is
        // cheap and worth it.
        let mut ours = Vec::new();
        let mut behind = Vec::new();
        for name in &names {
            match self.probe_version(name).await {
                // Carries no capture schema, so it is not ours. A cluster can
                // hold databases nothing to do with this gateway, and creating
                // hypertables in them would be vandalism, not migration.
                Ok(None) => continue,
                Ok(Some(v)) => {
                    // Current with a buffer a crash left: not served until the
                    // loop below has drained it.
                    if v == schema::SCHEMA_VERSION && !self.buffer_left(name).await {
                        self.set_state(name, DbSchemaState::Current { version: v })
                            .await;
                    } else if v == schema::SCHEMA_VERSION {
                        self.set_state(name, DbSchemaState::Pending { version: v })
                            .await;
                    } else {
                        self.set_state(name, DbSchemaState::Pending { version: v })
                            .await;
                        behind.push((name.clone(), v));
                    }
                    ours.push((name.clone(), v));
                }
                Err(e) => {
                    self.set_state(name, DbSchemaState::failed(0, e)).await;
                }
            }
        }
        if !behind.is_empty() && !self.config.auto_migrate {
            tracing::warn!(
                databases = Self::names_of(&behind),
                "behind schema v{} and WIRETAP_AUTO_MIGRATE is off; they refuse reads, \
                 and buffer ingest, until migrated from the admin UI's Databases page, \
                 POST /v1/databases/{{db}}/migrate, or by hand",
                schema::SCHEMA_VERSION
            );
            ours.retain(|(_, v)| *v == schema::SCHEMA_VERSION);
        } else if !behind.is_empty() {
            tracing::warn!(
                databases = Self::names_of(&behind),
                "migrating to schema v{}; these refuse reads and buffer ingest until done",
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
        // Skipping the ones just migrated, which were checked on their way
        // through: probing those again is a connection and a catalogue query to
        // be told what this loop already knows.
        let migrated: std::collections::HashSet<&str> =
            behind.iter().map(|(n, _)| n.as_str()).collect();
        for (name, _) in ours.iter().filter(|(n, _)| !migrated.contains(n.as_str())) {
            self.repair_rollup_if_incomplete(name).await;
        }
    }

    /// Migrate one database in the background, through the path the sweep
    /// takes, whether or not automatic migration is on.
    pub async fn start_migration(&self, name: &str) -> Result<(), MigrateRefusal> {
        if !valid_database_name(name) || !self.database_exists(name).await? {
            return Err(MigrateRefusal::NotFound(format!(
                "database '{name}' does not exist"
            )));
        }
        let refuse_settled = |states: &HashMap<String, DbSchemaState>| match states.get(name) {
            Some(state @ (DbSchemaState::Current { .. } | DbSchemaState::Migrating { .. })) => Err(
                MigrateRefusal::Conflict(format!("database '{name}' is {}", state.label())),
            ),
            _ => Ok(()),
        };
        refuse_settled(&*self.schema_state.lock().await)?;
        // Already current on disk, migrated by hand: the run below is then the
        // cheap one that drains its buffer and serves it.
        let at = self.probe_version(name).await?;
        if at.is_none() {
            return Err(MigrateRefusal::Conflict(format!(
                "database '{name}' holds no capture schema"
            )));
        }
        let mut states = self.schema_state.lock().await;
        refuse_settled(&states)?;
        states.insert(name.to_string(), DbSchemaState::migrating());
        drop(states);
        // Owned by the process, for the reason `require_current` gives.
        let this = self.clone();
        let owned = name.to_string();
        tokio::spawn(async move { this.migrate_from(&owned, at).await });
        Ok(())
    }

    /// A database left behind may since have been migrated by hand. Asked
    /// again on each request while it is, and never migrated from here; found
    /// current, its buffer is drained before it serves.
    async fn recheck_if_behind(&self, name: &str) {
        // A failure is asked again too, once the interval has passed: with
        // automatic migration on, `require_current` retries it instead.
        let behind = |state: Option<&DbSchemaState>| match state {
            Some(DbSchemaState::Pending { .. }) => true,
            Some(DbSchemaState::Failed { since, .. }) => {
                !self.config.auto_migrate && since.elapsed() >= FAILED_RETRY_INTERVAL
            }
            None => !self.config.auto_migrate,
            _ => false,
        };
        if !behind(self.schema_state.lock().await.get(name)) {
            return;
        }
        let Ok(Some(version)) = self.probe_version(name).await else {
            return;
        };
        let state = if version == schema::SCHEMA_VERSION {
            match self.drain(name).await {
                Ok(drained) => self.note_drained(name, drained.as_ref()).await,
                Err(e) => {
                    tracing::warn!(database = name, "could not drain buffered ingest: {e}");
                    return;
                }
            }
            DbSchemaState::Current { version }
        } else {
            DbSchemaState::Pending { version }
        };
        let mut states = self.schema_state.lock().await;
        if behind(states.get(name)) {
            states.insert(name.to_string(), state);
        }
    }

    async fn drain(&self, name: &str) -> Result<Option<writer::Drained>, String> {
        let mut client = self.connect_raw(name).await?;
        let drained = writer::drain_pending(&mut client).await?;
        if let Some(span) = &drained {
            schema::refresh_rollup_span(&client, span).await?;
        }
        Ok(drained)
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

    async fn buffer_left(&self, name: &str) -> bool {
        match self.connect_raw(name).await {
            Ok(client) => writer::has_pending(&client).await.unwrap_or(true),
            Err(_) => true,
        }
    }

    async fn probe_version(&self, name: &str) -> Result<Option<i32>, String> {
        let client = self.connect_raw(name).await?;
        schema::detect_version(&client).await
    }

    /// Materialise the hourly rollup over its whole range, recording it so the
    /// admin UI can show it and a second one cannot start on top.
    ///
    /// Also run automatically — by a migration that leaves the rollup
    /// uncovered, and by the startup sweep when it finds an archive the rollup
    /// does not cover. This entry point is the operator's button, for a rollup
    /// that has merely fallen behind.
    ///
    /// Does not block reads or writes. Note that an aggregate which is *partly*
    /// materialised under-reports rather than merely running slowly — buckets
    /// below the watermark come from the materialisation table alone — so this
    /// always refreshes the whole range, never a window.
    pub async fn refresh_rollup(&self, name: &str) -> Result<(), String> {
        if !valid_database_name(name) {
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

    pub fn auto_migrate(&self) -> bool {
        self.config.auto_migrate
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
            .create_timeout(Some(self.connect_timeout))
            .runtime(Runtime::Tokio1)
            .build()
            .map_err(|e| format!("pool build failed: {e}"))
    }

    /// One-off (non-pooled) connection, used for CREATE DATABASE and bootstrap.
    pub async fn connect_raw(&self, database: &str) -> Result<tokio_postgres::Client, String> {
        let dsn = self.config.pg_dsn(database);
        let connecting = tokio_postgres::connect(&dsn, NoTls);
        let (client, connection) = tokio::time::timeout(self.connect_timeout, connecting)
            .await
            .map_err(|_| format!("connect to '{database}' timed out"))?
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

    /// Create a capture database (idempotent) and give it the current schema.
    /// One that already exists and is behind is left to wait for its
    /// migration unless automatic migration is on: initialising is not
    /// migrating.
    pub async fn create_database(&self, name: &str) -> Result<(), String> {
        if !valid_database_name(name) {
            return Err(format!("invalid database name '{name}'"));
        }
        let created = {
            // Scoped: migrate_one takes this same lock, and holding it across
            // that call would deadlock.
            let _guard = self.create_lock.lock().await;
            let exists = self.database_exists(name).await?;
            if !exists {
                let client = self.connect_raw("postgres").await?;
                // CREATE DATABASE is non-transactional; name is validated above.
                client
                    .batch_execute(&format!("CREATE DATABASE {name}"))
                    .await
                    .map_err(|e| format!("CREATE DATABASE {name} failed: {e}"))?;
                tracing::info!("created database '{name}'");
            }
            !exists
        };
        if created || self.config.auto_migrate {
            return self.migrate_one(name).await;
        }
        match self.probe_version(name).await? {
            Some(version) if version != schema::SCHEMA_VERSION => {
                self.set_state(name, DbSchemaState::Pending { version })
                    .await;
                Ok(())
            }
            at => self.migrate_from(name, at).await,
        }
    }

    /// Drop a capture database (admin). Refuses the default database (it holds
    /// the API-key store). Removes the pool first, then DROP ... WITH (FORCE)
    /// to terminate any lingering query connections. Callers must ensure it is
    /// not actively being ingested.
    pub async fn delete_database(&self, name: &str) -> Result<(), String> {
        if !valid_database_name(name) {
            return Err(format!("invalid database name '{name}'"));
        }
        if name == self.config.default_database {
            return Err("cannot delete the default database".into());
        }
        self.pools.lock().await.remove(name);
        self.buffer_pools.lock().await.remove(name);
        self.buffered.lock().await.remove(name);
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
    pub async fn pool(&self, name: &str) -> Result<Pool, DbError> {
        if !valid_database_name(name) {
            return Err(DbError::Refused(format!("invalid database name '{name}'")));
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
            return Err(DbError::Refused(format!(
                "database '{name}' does not exist"
            )));
        }
        self.recheck_if_behind(name).await;
        self.pool_if_current(name).await
    }

    async fn pool_if_current(&self, name: &str) -> Result<Pool, DbError> {
        let mut pools = self.pools.lock().await;
        self.require_current(name).await?;
        let pool = self.build_pool(name)?;
        pools.insert(name.to_string(), pool.clone());
        Ok(pool)
    }

    /// Refuse anything not at the current schema. A database never seen before —
    /// restored from a backup, or created between sweeps — is migrated here
    /// rather than left permanently unusable, unless automatic migration is off.
    /// So is one that failed, once [`FAILED_RETRY_INTERVAL`] has passed.
    async fn require_current(&self, name: &str) -> Result<(), String> {
        let mut states = self.schema_state.lock().await;
        let check = match states.get(name) {
            Some(DbSchemaState::Current { .. }) => return Ok(()),
            Some(DbSchemaState::Failed { since, .. })
                if self.config.auto_migrate && since.elapsed() >= FAILED_RETRY_INTERVAL =>
            {
                // Claimed under the lock, so concurrent requests see it busy
                // rather than each starting a retry of their own.
                states.insert(name.to_string(), DbSchemaState::migrating());
                true
            }
            Some(DbSchemaState::Pending { version }) => {
                return Err(format!(
                    "database '{name}' is at schema v{version} and waiting to be migrated \
                     to v{}: Migrate now on the admin UI's Databases page, or \
                     POST /v1/databases/{name}/migrate",
                    schema::SCHEMA_VERSION
                ));
            }
            Some(state) => {
                return Err(format!(
                    "database '{name}' is not ready: schema {}",
                    state.label()
                ));
            }
            None => self.config.auto_migrate,
        };
        drop(states);
        if check {
            // Owned by the process, not by this request: axum drops a
            // handler future when the client disconnects, and a migration
            // dropped mid-flight would leave the state at `Migrating`
            // forever. Start it and answer "not ready" — every caller
            // already handles that, and a capture server caches and retries.
            let this = self.clone();
            let owned = name.to_string();
            tokio::spawn(async move { this.migrate_one(&owned).await });
            Err(format!("database '{name}' is being checked; retry shortly"))
        } else {
            Err(format!(
                "database '{name}' has not been checked and WIRETAP_AUTO_MIGRATE is off"
            ))
        }
    }

    /// Resolve a database for ingest/import: existing, or auto-created when
    /// the config allows, and able to take a batch now.
    pub async fn ensure_database(&self, name: &str, allow_create: bool) -> Result<(), DbError> {
        if !valid_database_name(name) {
            return Err(DbError::Refused(format!("invalid database name '{name}'")));
        }
        if !self.database_exists(name).await? {
            if !(allow_create && self.config.auto_create_databases) {
                return Err(DbError::Refused(format!(
                    "database '{name}' does not exist (auto-create disabled)"
                )));
            }
            self.create_database(name).await?;
        }
        self.route(name).await.map(|_| ())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use wiretap_model::Secret;

    pub(crate) fn unreachable_databases(auto_migrate: bool) -> Databases {
        let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Databases::new(Arc::new(config_at(closed_port, auto_migrate)))
    }

    pub(crate) fn config_at(pg_port: u16, auto_migrate: bool) -> Config {
        Config {
            http_listen: String::new(),
            ingest_listen: String::new(),
            pg_host: "127.0.0.1".into(),
            pg_port,
            pg_user: "postgres".into(),
            pg_password: Secret::new("unused"),
            default_database: "wiretap".into(),
            bootstrap_admin_key: None,
            auto_create_databases: false,
            auto_migrate,
            ingest_keepalive_secs: 30.0,
            ingest_max_batch_frames: 256,
            log_buffer: 0,
        }
    }

    /// Over a throwaway TimescaleDB on `WIRETAP_TEST_PG_PORT`, the superuser's
    /// password in `WIRETAP_TEST_PG_PASSWORD`. The tests using it are ignored
    /// by default; run them with `--ignored`.
    pub(crate) fn live_databases(auto_migrate: bool) -> Databases {
        let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let mut config = config_at(env("WIRETAP_TEST_PG_PORT").parse().unwrap(), auto_migrate);
        config.pg_password = Secret::new(env("WIRETAP_TEST_PG_PASSWORD"));
        Databases::new(Arc::new(config))
    }

    /// Held to create a database, and to sweep. On a fresh cluster two first
    /// runs race to create the role the schema grants to; and a sweep re-applies
    /// the schema to every current database on the cluster, which re-stamps one
    /// caught between its creation and `then`.
    static CLUSTER: Mutex<()> = Mutex::const_new(());

    /// A new capture database, with `then` run on it.
    async fn fresh_database(prefix: &str, then: &str) -> String {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("{prefix}_{stamp}");
        let _cluster = CLUSTER.lock().await;
        live_databases(true).create_database(&name).await.unwrap();
        run(&name, then).await;
        name
    }

    pub(crate) async fn sweep(dbs: &Databases) {
        let _cluster = CLUSTER.lock().await;
        dbs.migrate_all().await;
    }

    pub(crate) async fn run(name: &str, sql: &str) {
        let client = live_databases(true).connect_raw(name).await.unwrap();
        client.batch_execute(sql).await.unwrap();
    }

    /// A capture database stamped one version behind. Each migration re-runs
    /// over its own work, so the newest one takes it to current with nothing
    /// of its own to do: what is left is the runner's.
    pub(crate) async fn behind_database(prefix: &str) -> String {
        let behind = schema::SCHEMA_VERSION - 1;
        fresh_database(
            prefix,
            &format!("UPDATE public.schema_version SET version = {behind}"),
        )
        .await
    }

    /// A database in v3's shape, with `days` of CAN frames, `per_day` to a
    /// day's chunk, the older half compressed.
    pub(crate) async fn seeded_v3_database(prefix: &str, days: u32, per_day: u32) -> String {
        fresh_database(
            prefix,
            &format!(
                "DROP MATERIALIZED VIEW public.capture_frame_hourly CASCADE;
                 DROP VIEW public.can_fd_frame_bytes, public.can_frame_bytes, public.can_frame;
                 ALTER TABLE public.capture_frame
                   DROP CONSTRAINT capture_frame_protocol_columns_check,
                   DROP COLUMN flags,
                   ADD COLUMN extended boolean NOT NULL DEFAULT false,
                   ADD COLUMN is_fd boolean NOT NULL DEFAULT false,
                   ADD COLUMN dir text NOT NULL DEFAULT 'rx';
                 INSERT INTO public.capture_frame
                   (ts, protocol, id, dlc, data_bytes, bus, extended, is_fd, dir)
                 SELECT now() - make_interval(secs => i * 86400.0 / {per_day}), 'can',
                        256 + i % 4, 8, '\\x0102030405060708', 0, i % 5 = 0, false,
                        CASE WHEN i % 3 = 0 THEN 'tx' ELSE 'rx' END
                   FROM generate_series(1, {days} * {per_day}) i;
                 SELECT compress_chunk(c) FROM show_chunks('public.capture_frame',
                   older_than => now() - INTERVAL '{} days') c;
                 UPDATE public.schema_version SET version = 3;",
                days / 2
            ),
        )
        .await
    }

    /// `n` CAN frames of `id`, a millisecond apart from `from_us`.
    pub(crate) fn frames(id: u32, from_us: i64, n: i64) -> Vec<FrameRow> {
        use wiretap_protocol::ingest::RecordKind;
        (0..n)
            .map(|i| FrameRow::new(from_us + i * 1000, RecordKind::Can, id, 0, 0, vec![1, 2]))
            .collect()
    }

    /// Rows of `id` in `capture_frame`, and how many distinct timestamps.
    pub(crate) async fn archived(name: &str, id: u32) -> (i64, i64) {
        let client = live_databases(true).connect_raw(name).await.unwrap();
        let row = client
            .query_one(
                "SELECT count(*), count(DISTINCT ts) FROM public.capture_frame WHERE id = $1",
                &[&(id as i32)],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1))
    }

    pub(crate) async fn buffer_exists(name: &str) -> bool {
        let client = live_databases(true).connect_raw(name).await.unwrap();
        writer::has_pending(&client).await.unwrap()
    }

    pub(crate) async fn until_label(dbs: &Databases, name: &str, label: &str) {
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                let now = state_of(dbs, name).await;
                assert!(
                    !matches!(now, DbSchemaState::Failed { .. }) || label == "failed",
                    "{now:?}"
                );
                if now.label() == label {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{name} never reached {label}"));
    }

    pub(crate) async fn version_on_disk(dbs: &Databases, name: &str) -> Option<i32> {
        dbs.probe_version(name).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_database_migrated_by_hand_is_served_without_a_restart() {
        let name = behind_database("handrun").await;
        let dbs = live_databases(false);
        sweep(&dbs).await;
        assert_eq!(state_of(&dbs, &name).await.label(), "pending");
        assert!(
            dbs.pool(&name).await.is_err(),
            "a behind database was served"
        );
        assert_eq!(
            state_of(&dbs, &name).await.label(),
            "pending",
            "a request migrated it with WIRETAP_AUTO_MIGRATE off"
        );
        dbs.write_rows(&name, &frames(0x7A0, 1_700_000_000_000_000, 5))
            .await
            .unwrap();

        let by_hand = dbs.connect_raw(&name).await.unwrap();
        schema::migrate(&by_hand, Some(schema::SCHEMA_VERSION - 1))
            .await
            .unwrap();

        let served = dbs.pool(&name).await;
        assert!(served.is_ok(), "{}", served.err().unwrap());
        assert_eq!(state_of(&dbs, &name).await.label(), "current");
        assert_eq!(archived(&name, 0x7A0).await, (5, 5));
        assert!(!buffer_exists(&name).await);
        dbs.delete_database(&name).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_new_database_is_initialised_but_an_existing_one_behind_waits() {
        let dbs = live_databases(false);
        let new = format!("fresh_{}", std::process::id());
        {
            let _cluster = CLUSTER.lock().await;
            dbs.create_database(&new).await.unwrap();
        }
        assert_eq!(state_of(&dbs, &new).await.label(), "current");
        assert_eq!(
            version_on_disk(&dbs, &new).await,
            Some(schema::SCHEMA_VERSION)
        );

        let behind = behind_database("waits").await;
        dbs.create_database(&behind).await.unwrap();
        assert_eq!(state_of(&dbs, &behind).await.label(), "pending");
        let refused = dbs.pool(&behind).await.err().unwrap().to_string();
        assert!(refused.contains("waiting to be migrated"), "{refused}");
        assert_eq!(
            version_on_disk(&dbs, &behind).await,
            Some(schema::SCHEMA_VERSION - 1)
        );
        for name in [&new, &behind] {
            dbs.delete_database(name).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn ingest_during_a_migration_lands_once_after_it_and_reads_stay_refused() {
        let name = seeded_v3_database("during", 12, 24).await;
        let dbs = live_databases(false);
        sweep(&dbs).await;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = tokio::spawn({
            let (dbs, name, stop) = (dbs.clone(), name.clone(), stop.clone());
            async move {
                let mut sent = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let from = 1_700_000_000_000_000 + sent * 1000;
                    // Sent again when refused, as a capture server does.
                    if dbs
                        .write_rows(&name, &frames(0x7B0, from, 10))
                        .await
                        .is_ok()
                    {
                        sent += 10;
                    }
                }
                sent
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        dbs.start_migration(&name).await.unwrap();
        assert!(
            dbs.pool(&name).await.is_err(),
            "a migrating database was served"
        );
        until_label(&dbs, &name, "current").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let sent = writer.await.unwrap();

        assert!(sent > 0);
        assert_eq!(archived(&name, 0x7B0).await, (sent, sent));
        assert!(!buffer_exists(&name).await);
        dbs.delete_database(&name).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_buffer_left_by_a_crash_is_drained_at_startup() {
        let name = behind_database("crashed").await;
        let before = live_databases(false);
        sweep(&before).await;
        before
            .write_rows(&name, &frames(0x7C0, 1_700_000_000_000_000, 7))
            .await
            .unwrap();
        let client = before.connect_raw(&name).await.unwrap();
        schema::migrate(&client, Some(schema::SCHEMA_VERSION - 1))
            .await
            .unwrap();
        assert!(buffer_exists(&name).await);

        let restarted = live_databases(false);
        sweep(&restarted).await;
        assert_eq!(state_of(&restarted, &name).await.label(), "current");
        assert_eq!(archived(&name, 0x7C0).await, (7, 7));
        assert!(!buffer_exists(&name).await);
        restarted.delete_database(&name).await.unwrap();
    }

    async fn rollup_table(name: &str) -> String {
        let client = live_databases(true).connect_raw(name).await.unwrap();
        client
            .query_one(
                "SELECT format('%I.%I', materialization_hypertable_schema, \
                                        materialization_hypertable_name) \
                   FROM timescaledb_information.continuous_aggregates \
                  WHERE view_name = 'capture_frame_hourly'",
                &[],
            )
            .await
            .unwrap()
            .get(0)
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_run_queued_behind_one_that_finished_does_not_migrate_again() {
        let name = behind_database("queued").await;
        let dbs = live_databases(false);
        sweep(&dbs).await;
        dbs.start_migration(&name).await.unwrap();
        until_label(&dbs, &name, "current").await;
        let rollup = rollup_table(&name).await;

        dbs.migrate_from(&name, Some(schema::SCHEMA_VERSION - 1))
            .await
            .unwrap();
        assert_eq!(
            rollup_table(&name).await,
            rollup,
            "the migration ran again and rebuilt the rollup"
        );
        dbs.delete_database(&name).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB 2.29; see live_databases"]
    async fn a_drain_that_meets_a_refresh_in_progress_still_refreshes_its_span() {
        let name = behind_database("collides").await;
        let dbs = live_databases(false);
        sweep(&dbs).await;
        dbs.write_rows(&name, &frames(0x7E9, 1_700_000_000_000_000, 5))
            .await
            .unwrap();
        let by_hand = dbs.connect_raw(&name).await.unwrap();
        schema::migrate(&by_hand, Some(schema::SCHEMA_VERSION - 1))
            .await
            .unwrap();

        // A refresh of the whole range in progress, as TimescaleDB records one,
        // for a second.
        let refreshing = dbs.connect_raw(&name).await.unwrap();
        refreshing
            .batch_execute(
                "INSERT INTO _timescaledb_catalog.continuous_aggs_jobs_refresh_ranges
                 SELECT mat_hypertable_id, -9223372036854775807, 9223372036854775807,
                        pg_backend_pid(), 0, now()
                   FROM _timescaledb_catalog.continuous_agg",
            )
            .await
            .unwrap();
        let finishes = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            refreshing
                .batch_execute(
                    "DELETE FROM _timescaledb_catalog.continuous_aggs_jobs_refresh_ranges \
                     WHERE job_id = 0",
                )
                .await
                .unwrap();
        });

        let served = dbs.pool(&name).await;
        assert!(served.is_ok(), "{}", served.err().unwrap());
        finishes.await.unwrap();
        let materialised: i64 = by_hand
            .query_one(
                &format!("SELECT count(*) FROM {}", rollup_table(&name).await),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(materialised > 0, "the drained span was never materialised");
        dbs.delete_database(&name).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_migrated_database_keeps_no_connection_for_its_buffer() {
        let name = behind_database("idle").await;
        let dbs = live_databases(false);
        sweep(&dbs).await;
        dbs.write_rows(&name, &frames(0x7EA, 1_700_000_000_000_000, 2))
            .await
            .unwrap();
        dbs.start_migration(&name).await.unwrap();
        until_label(&dbs, &name, "current").await;

        let postgres = dbs.connect_raw("postgres").await.unwrap();
        let held = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let n: i64 = postgres
                    .query_one(
                        "SELECT count(*) FROM pg_stat_activity \
                          WHERE datname = $1 AND backend_type = 'client backend'",
                        &[&name],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if n == 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(held.is_ok(), "a connection is still open to {name}");
        dbs.delete_database(&name).await.unwrap();
    }

    fn failed_ago(ago: Duration) -> DbSchemaState {
        DbSchemaState::Failed {
            version: 1,
            error: "connection refused".into(),
            since: Instant::now().checked_sub(ago).unwrap(),
        }
    }

    /// Behind a server that accepts connections and never says a word, like
    /// one that is up but wedged.
    async fn silent_databases() -> (Databases, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let dbs = Databases {
            connect_timeout: Duration::from_millis(200),
            ..Databases::new(Arc::new(config_at(port, true)))
        };
        (dbs, server)
    }

    pub(crate) async fn state_of(dbs: &Databases, name: &str) -> DbSchemaState {
        dbs.schema_states().await[name].clone()
    }

    #[tokio::test]
    async fn a_failed_database_is_retried_once_the_interval_has_passed() {
        let dbs = unreachable_databases(true);

        dbs.set_state("archive", failed_ago(Duration::ZERO)).await;
        let early = dbs.require_current("archive").await.unwrap_err();
        assert!(early.ends_with("schema failed"), "{early}");

        dbs.set_state("archive", failed_ago(FAILED_RETRY_INTERVAL))
            .await;
        let due = dbs.require_current("archive").await.unwrap_err();
        assert!(due.ends_with("being checked; retry shortly"), "{due}");

        let during = dbs.require_current("archive").await.unwrap_err();
        assert!(during.ends_with("schema migrating"), "{during}");

        let settled = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let DbSchemaState::Failed { since, .. } = state_of(&dbs, "archive").await {
                    return since;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the retry never settled");
        assert!(
            settled.elapsed() < FAILED_RETRY_INTERVAL,
            "a failed retry restarts the interval"
        );
    }

    #[tokio::test]
    async fn a_pool_is_not_cached_for_a_database_that_left_current_meanwhile() {
        let dbs = unreachable_databases(true);
        dbs.set_state("archive", DbSchemaState::current()).await;

        let pools = dbs.pools.lock().await;
        let caller = tokio::spawn({
            let dbs = dbs.clone();
            async move { dbs.pool_if_current("archive").await.map(|_| ()) }
        });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        dbs.schema_state
            .lock()
            .await
            .insert("archive".into(), DbSchemaState::migrating());
        drop(pools);

        let answer = caller.await.unwrap();
        assert!(
            answer.is_err(),
            "a pool was handed out for a migrating database"
        );
        assert!(
            !dbs.pools.lock().await.contains_key("archive"),
            "a pool was cached for a migrating database"
        );
    }

    #[tokio::test]
    async fn a_failed_database_stays_failed_when_automatic_migration_is_off() {
        let dbs = unreachable_databases(false);
        dbs.set_state("archive", failed_ago(FAILED_RETRY_INTERVAL * 2))
            .await;

        let answer = dbs.require_current("archive").await.unwrap_err();
        assert!(answer.ends_with("schema failed"), "{answer}");
        assert_eq!(state_of(&dbs, "archive").await.label(), "failed");
    }

    #[tokio::test]
    async fn a_raw_connection_to_a_silent_server_gives_up() {
        let (dbs, server) = silent_databases().await;
        let outcome =
            tokio::time::timeout(Duration::from_secs(2), dbs.connect_raw("archive")).await;
        assert!(
            matches!(outcome, Ok(Err(_))),
            "connect_raw was still waiting on a silent server after 2s"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_pooled_connection_to_a_silent_server_gives_up() {
        let (dbs, server) = silent_databases().await;
        let pool = dbs.build_pool("archive").unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(2), pool.get()).await;
        assert!(
            matches!(outcome, Ok(Err(_))),
            "the pool was still waiting on a silent server after 2s"
        );
        server.abort();
    }
}
