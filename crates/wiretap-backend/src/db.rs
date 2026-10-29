//! Connection-pool registry: one deadpool pool per capture database, created
//! lazily. Database names are strictly validated before they reach a DSN or
//! SQL, and existence is checked against pg_database.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deadpool_postgres::{Connect, Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use futures_util::future::BoxFuture;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};
use tokio::task::JoinHandle;
use tokio_postgres::error::SqlState;
use tokio_postgres::NoTls;
use wslib_file_atomic::PromoteError;
use wslib_secret::Secret;

use crate::archive::ImportError;
use crate::config::Config;
use crate::ingest::writer::{self, CopyError, FrameRow, Written};
use crate::schema;
use crate::secrets;

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

pub use wslib_wiretap_protocol::ingest::valid_database_name;

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
    /// Into the buffer of a database an import is replacing, for the swap to
    /// carry across; refused while it swaps.
    Held(Arc<RwLock<bool>>),
}

/// The refusal for anything that would treat the gateway's own database as a
/// capture database.
pub fn protected(name: &str) -> String {
    format!(
        "'{name}' is the gateway's own database (WIRETAP_DEFAULT_DB): it holds keys and \
         catalogues, and takes no captures"
    )
}

/// An archive being loaded into `staging`, to become its target.
struct Import {
    staging: String,
    since: Instant,
    /// A replace's: held writes take it shared, the swap exclusively, and it
    /// is true from the swap on.
    held: Option<Arc<RwLock<bool>>>,
    /// Holds the import's advisory lock until it is done with.
    _claim: tokio_postgres::Client,
}

/// Prefixes no capture database can have, so a start can drop what an import
/// left behind. An import's staging and replaced databases share the suffix
/// after them, which its advisory lock is keyed on.
const STAGING_PREFIX: &str = "__import_";
const REPLACED_PREFIX: &str = "__old_";

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(m) | Self::Unavailable(m) => f.write_str(m),
        }
    }
}

/// The password each new connection, pooled or raw, is opened with. A rotation
/// takes the write half, so no handshake straddles its `ALTER ROLE`.
#[derive(Clone)]
struct Credentials(Arc<RwLock<Secret>>);

impl Connect for Credentials {
    fn connect(
        &self,
        pg_config: &tokio_postgres::Config,
    ) -> BoxFuture<'_, Result<(tokio_postgres::Client, JoinHandle<()>), tokio_postgres::Error>>
    {
        let mut pg_config = pg_config.clone();
        Box::pin(async move {
            let password = self.0.read().await;
            pg_config.password(password.expose());
            let (client, connection) = pg_config.connect(NoTls).await?;
            drop(password);
            let task = tokio::spawn(async move {
                if let Err(e) = connection.await {
                    tracing::debug!("postgres connection closed: {e}");
                }
            });
            Ok((client, task))
        })
    }
}

enum Unreached {
    TimedOut,
    Failed(tokio_postgres::Error),
}

impl Unreached {
    fn describe(self, database: &str) -> String {
        match self {
            Self::TimedOut => format!("connect to '{database}' timed out"),
            Self::Failed(e) => format!(
                "connect to '{database}' failed: {}",
                schema::db_error_detail(&e)
            ),
        }
    }
}

#[derive(Clone)]
pub struct Databases {
    config: Arc<Config>,
    credentials: Credentials,
    pools: Arc<Mutex<HashMap<String, Pool>>>,
    /// Serialises CREATE DATABASE between concurrent first connections.
    create_lock: Arc<Mutex<()>>,
    /// One per database, held for the whole of its migration, so two runs
    /// never overlap on one database and runs on different ones never wait.
    migration_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
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
    /// Archive imports in progress, by the database each will become.
    imports: Arc<Mutex<HashMap<String, Import>>>,
    connect_timeout: Duration,
}

impl Databases {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            credentials: Credentials(Arc::new(RwLock::new(config.pg_password.clone()))),
            config,
            pools: Arc::new(Mutex::new(HashMap::new())),
            create_lock: Arc::new(Mutex::new(())),
            migration_locks: Arc::new(Mutex::new(HashMap::new())),
            schema_state: Arc::new(Mutex::new(HashMap::new())),
            rollup_rebuild: Arc::new(Mutex::new(HashMap::new())),
            buffer_pools: Arc::new(Mutex::new(HashMap::new())),
            buffered: Arc::new(Mutex::new(HashMap::new())),
            imports: Arc::new(Mutex::new(HashMap::new())),
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

    /// Every capture database on the cluster, in name order: not the gateway's
    /// own.
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
            .filter(|n| valid_database_name(n) && !self.is_gateway_database(n))
            .collect())
    }

    /// Bring one database to the current schema, probing it first.
    /// Idempotent, and safe to call on a database that is already current.
    pub async fn migrate_one(&self, name: &str) -> Result<(), String> {
        self.migrate_from(name, None).await
    }

    async fn migration_lock(&self, name: &str) -> Arc<Mutex<()>> {
        self.migration_locks
            .lock()
            .await
            .entry(name.to_string())
            .or_default()
            .clone()
    }

    /// The migration, given the version the caller found. Split from
    /// [`Self::migrate_one`] so the sweep's probe pass is not thrown away and
    /// asked again for a database already current. One behind is asked again
    /// under the lock: a run queued behind another may find it finished.
    async fn migrate_from(&self, name: &str, at: Option<i32>) -> Result<(), String> {
        let guard = self.migration_lock(name).await.lock_owned().await;
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
        self.migrate_holding(guard, name, at).await
    }

    async fn migrate_holding(
        &self,
        _lock: OwnedMutexGuard<()>,
        name: &str,
        at: Option<i32>,
    ) -> Result<(), String> {
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
            // buffer behind, or between a drain and its refresh, the span.
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
            }
            writer::refresh_drained(&client).await?;
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
                self.note_drained(name, *drained).await;
                self.start_background_workers(name).await;
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

    async fn note_drained(&self, name: &str, rows: u64) {
        if rows > 0 {
            tracing::info!(
                database = name,
                rows,
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
            Route::Held(gate) => {
                let swapping = gate.read().await;
                if *swapping {
                    return Err(WriteError::Db(DbError::Unavailable(format!(
                        "database '{name}' is being swapped for an imported archive; retry shortly"
                    ))));
                }
                let pool = self.buffer_pool(name).await.map_err(WriteError::Db)?;
                writer::buffer_rows(&pool, rows, true)
                    .await
                    .map_err(WriteError::Copy)?;
                self.note_buffered(name, rows.len()).await;
                Ok(())
            }
            Route::Buffered(pool) => match writer::buffer_rows(&pool, rows, false)
                .await
                .map_err(WriteError::Copy)?
            {
                Written::Buffered => {
                    self.note_buffered(name, rows.len()).await;
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

    async fn note_buffered(&self, name: &str, rows: usize) {
        *self.buffered.lock().await.entry(name.into()).or_default() += rows as u64;
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
        if let Some(gate) = self.held(name).await {
            return Ok(Route::Held(gate));
        }
        if !self.owes_migration(name, !self.config.auto_migrate).await {
            match self.pool(name).await {
                Ok(pool) => return Ok(Route::Live(pool)),
                Err(e) if !self.owes_migration(name, true).await => return Err(e),
                Err(_) => {}
            }
        }
        Ok(Route::Buffered(self.buffer_pool(name).await?))
    }

    async fn buffer_pool(&self, name: &str) -> Result<Pool, DbError> {
        let mut pools = self.buffer_pools.lock().await;
        if let Some(pool) = pools.get(name) {
            return Ok(pool.clone());
        }
        let pool = self.build_pool(name)?;
        pools.insert(name.to_string(), pool.clone());
        Ok(pool)
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
                    // Current with a buffer, or a drained span, a crash left:
                    // not served until the loop below has finished it.
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
        self.warn_of_old_captures().await;
    }

    /// An older install captured into the gateway's own database when a client
    /// named none. Those rows are no longer served, and are left where they are.
    async fn warn_of_old_captures(&self) {
        let name = self.default_database();
        let counted = async {
            let client = self.connect_raw(name).await?;
            let table = match schema::detect_version(&client).await? {
                None => return Ok(0),
                Some(0) => "public.can_frame",
                Some(_) => "public.capture_frame",
            };
            let row = client
                .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                .await
                .map_err(|e| schema::db_error_detail(&e))?;
            Ok::<i64, String>(row.get(0))
        };
        match counted.await {
            Ok(0) => {}
            Ok(frames) => tracing::warn!(
                database = name,
                frames,
                "the gateway's own database holds frames captured before it stopped taking \
                 captures; they are left alone and no longer served"
            ),
            Err(e) => tracing::warn!(database = name, "could not count old captures: {e}"),
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
        let Ok(lock) = self.migration_lock(name).await.try_lock_owned() else {
            return Err(MigrateRefusal::Conflict(format!(
                "database '{name}' is migrating"
            )));
        };
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
        tokio::spawn(async move { this.migrate_holding(lock, &owned, at).await });
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
                Ok(drained) => self.note_drained(name, drained).await,
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

    async fn drain(&self, name: &str) -> Result<u64, String> {
        let mut client = self.connect_raw(name).await?;
        let drained = writer::drain_pending(&mut client).await?;
        writer::refresh_drained(&client).await?;
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
            Ok(client) => writer::drain_owed(&client).await.unwrap_or(true),
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

    pub fn is_gateway_database(&self, name: &str) -> bool {
        name == self.config.default_database
    }

    /// The gateway's own database, for its keys and catalogues: created
    /// without the capture schema.
    pub async fn create_gateway_database(&self) -> Result<(), String> {
        let name = self.default_database();
        if !valid_database_name(name) {
            return Err(format!("invalid WIRETAP_DEFAULT_DB '{name}'"));
        }
        let _guard = self.create_lock.lock().await;
        if !self.database_exists(name).await? {
            let client = self.connect_raw("postgres").await?;
            client
                .batch_execute(&format!("CREATE DATABASE {name}"))
                .await
                .map_err(|e| format!("CREATE DATABASE {name} failed: {e}"))?;
            tracing::info!("created the gateway's database '{name}'");
        }
        Ok(())
    }

    fn build_pool(&self, database: &str) -> Result<Pool, String> {
        let mgr = Manager::from_connect(
            self.config.pg_config(database),
            self.credentials.clone(),
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
        self.reach(database).await.map_err(|e| e.describe(database))
    }

    async fn reach(&self, database: &str) -> Result<tokio_postgres::Client, Unreached> {
        let pg_config = self.config.pg_config(database);
        let connecting = self.credentials.connect(&pg_config);
        match tokio::time::timeout(self.connect_timeout, connecting).await {
            Err(_) => Err(Unreached::TimedOut),
            Ok(Err(e)) => Err(Unreached::Failed(e)),
            Ok(Ok((client, _))) => Ok(client),
        }
    }

    /// The previous password.
    pub async fn set_pg_password(&self, password: Secret) -> Secret {
        std::mem::replace(&mut *self.credentials.0.write().await, password)
    }

    /// Connect once, as the gateway does at start. A `.next` beside the
    /// password file is a rotation interrupted after its `ALTER ROLE` if it is
    /// the password that works, and promoted; otherwise one that never got
    /// that far, and discarded.
    pub async fn reach_postgres(&self) -> Result<(), String> {
        let file = self.config.secret_files.pg_password.as_deref();
        let Some(file) = file.filter(|f| secrets::staged(f).exists()) else {
            return self.connect_raw("postgres").await.map(drop);
        };
        let staged = secrets::staged(file);
        match self.reach("postgres").await {
            Err(Unreached::Failed(e)) if e.code() == Some(&SqlState::INVALID_PASSWORD) => {
                let password = Secret::from_file(&staged).map_err(|e| e.to_string())?;
                let previous = self.set_pg_password(password).await;
                if let Err(e) = self.connect_raw("postgres").await {
                    self.set_pg_password(previous).await;
                    return Err(e);
                }
                wslib_file_atomic::promote(file, secrets::STAGED)
                    .map_err(|e| format!("{}: {e}", staged.display()))?;
                tracing::warn!(
                    "promoted {}, left by a rotation that stopped before saving it",
                    staged.display()
                );
                Ok(())
            }
            Err(e) => Err(e.describe("postgres")),
            Ok(_) => wslib_file_atomic::discard(file, secrets::STAGED)
                .map_err(|e| format!("{}: {e}", staged.display())),
        }
    }

    /// A new password for the gateway's role, set from a verifier computed here
    /// so the plaintext never reaches the server's log. Persisted when `file`
    /// is given: staged first, so a crash after the `ALTER ROLE` leaves it for
    /// [`Self::reach_postgres`] to promote. A failed `ALTER ROLE` leaves it
    /// too, since a lost reply may hide one that took.
    pub async fn rotate_pg_password(&self, file: Option<&Path>) -> Result<Secret, String> {
        let client = self.connect_raw("postgres").await?;
        // Every new connection waits while the write half is held.
        client
            .batch_execute("SET statement_timeout = '10s'")
            .await
            .map_err(|e| schema::db_error_detail(&e))?;
        let password = Secret::new(secrets::random_hex());
        let staged = file
            .map(|file| {
                secrets::stage(file, password.expose())
                    .map_err(|e| format!("{}: {e}", secrets::staged(file).display()))
            })
            .transpose()?;
        let verifier = postgres_protocol::password::scram_sha_256(password.expose().as_bytes());
        let mut current = self.credentials.0.write().await;
        if let Err(e) = client
            .batch_execute(&format!("ALTER ROLE CURRENT_USER PASSWORD '{verifier}'"))
            .await
        {
            return Err(format!(
                "ALTER ROLE failed: {}",
                schema::db_error_detail(&e)
            ));
        }
        *current = password.clone();
        drop(current);
        if let Some((file, staged)) = file.zip(staged) {
            staged.promote().map_err(|e| match e {
                PromoteError::Unmoved { staged, error } => format!(
                    "the password was changed but not saved ({error}); it is in {}, \
                     which the next start promotes",
                    staged.path().display()
                ),
                PromoteError::Unsynced(error) => format!(
                    "the password was changed and saved in {}, but its directory was not \
                     synced ({error})",
                    file.display()
                ),
                e => format!("the password was changed but not saved ({e})"),
            })?;
        }
        Ok(password)
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
        if self.importing(name).await {
            return Err(being_imported(name));
        }
        let created = {
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

    /// Drop a capture database (admin). Removes the pool first, then
    /// DROP ... WITH (FORCE) to terminate any lingering query connections.
    /// Callers must ensure it is not actively being ingested.
    pub async fn delete_database(&self, name: &str) -> Result<(), String> {
        if !valid_database_name(name) {
            return Err(format!("invalid database name '{name}'"));
        }
        if self.is_gateway_database(name) {
            return Err(protected(name));
        }
        self.pools.lock().await.remove(name);
        self.buffer_pools.lock().await.remove(name);
        self.buffered.lock().await.remove(name);
        self.schema_state.lock().await.remove(name);
        self.rollup_rebuild.lock().await.remove(name);
        self.migration_locks.lock().await.remove(name);
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
        if self.importing(name).await {
            return Err(DbError::Unavailable(being_imported(name)));
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

    /// An unpooled connection to a database that is serving, for a long read.
    pub async fn connect_current(&self, name: &str) -> Result<tokio_postgres::Client, DbError> {
        self.pool(name).await?;
        Ok(self.connect_raw(name).await?)
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
        if self.is_gateway_database(name) {
            return Err(DbError::Refused(protected(name)));
        }
        if self.held(name).await.is_some() {
            return Ok(());
        }
        if self.importing(name).await {
            return Err(DbError::Unavailable(being_imported(name)));
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

    async fn importing(&self, name: &str) -> bool {
        self.imports.lock().await.contains_key(name)
    }

    async fn held(&self, name: &str) -> Option<Arc<RwLock<bool>>> {
        self.imports.lock().await.get(name)?.held.clone()
    }

    /// Seconds each import in progress has been running, by its target.
    pub async fn import_busy(&self) -> HashMap<String, u64> {
        let imports = self.imports.lock().await;
        imports
            .iter()
            .map(|(name, import)| (name.clone(), import.since.elapsed().as_secs()))
            .collect()
    }

    /// Claim `target` for an archive and create the staging database it is
    /// loaded into. From here a replace holds `target`'s ingest in its buffer
    /// and refuses its reads, until [`Self::finish_import`] or
    /// [`Self::abandon_import`].
    pub async fn begin_import(&self, target: &str, replace: bool) -> Result<String, ImportError> {
        if !valid_database_name(target) {
            return Err(ImportError::BadRequest(format!(
                "invalid database name '{target}'"
            )));
        }
        match (replace, self.database_exists(target).await?) {
            (false, true) => {
                return Err(ImportError::Conflict(format!(
                    "database '{target}' already exists; import under a new name, or replace it"
                )))
            }
            (true, false) => {
                return Err(ImportError::NotFound(format!(
                    "database '{target}' does not exist"
                )))
            }
            _ => {}
        }
        let suffix = &secrets::random_hex()[..16];
        let staging = format!("{STAGING_PREFIX}{suffix}");
        let claim = self.connect_raw("postgres").await?;
        claim
            .execute(
                "SELECT pg_advisory_lock(hashtextextended($1, 0))",
                &[&suffix],
            )
            .await
            .map_err(|e| schema::db_error_detail(&e))?;
        {
            let states = self.schema_state.lock().await;
            if replace && !matches!(states.get(target), Some(DbSchemaState::Current { .. })) {
                return Err(ImportError::Conflict(format!(
                    "database '{target}' is not at the current schema; migrate it first"
                )));
            }
            let mut imports = self.imports.lock().await;
            if imports.contains_key(target) {
                return Err(ImportError::Conflict(being_imported(target)));
            }
            imports.insert(
                target.to_string(),
                Import {
                    staging: staging.clone(),
                    since: Instant::now(),
                    held: replace.then(Default::default),
                    _claim: claim,
                },
            );
        }
        let created = async {
            let client = self.connect_raw("postgres").await?;
            client
                .batch_execute(&format!("CREATE DATABASE {staging}"))
                .await
                .map_err(|e| format!("CREATE DATABASE {staging} failed: {e}"))?;
            let staged = self.connect_raw(&staging).await?;
            schema::migrate(&staged, None).await?;
            // Its policies would otherwise race the load's own compression and
            // refresh; `finish_import` starts them again.
            staged
                .batch_execute("SELECT _timescaledb_functions.stop_background_workers()")
                .await
                .map_err(|e| schema::db_error_detail(&e))
        };
        if let Err(e) = created.await {
            self.abandon_import(target).await;
            return Err(ImportError::Unavailable(e));
        }
        Ok(staging)
    }

    /// Put a loaded staging database in its target's place: a rename for a new
    /// one, a swap for a replace, with the ingest held meanwhile moved across
    /// first. Abandoned if it fails before the swap; from the swap on it has
    /// happened, and what then fails is returned as `Ok`, for the caller to
    /// report.
    pub async fn finish_import(&self, target: &str) -> Result<Option<String>, ImportError> {
        let replaced = match self.swap_in(target).await {
            Ok(replaced) => replaced,
            Err(e) => {
                self.abandon_import(target).await;
                return Err(e);
            }
        };
        self.start_background_workers(target).await;
        let served = self.migrate_one(target).await;
        if let Some(old) = replaced {
            if let Err(e) = self.drop_database(&old).await {
                tracing::warn!("{e}; the next start drops it");
            }
        }
        Ok(served.err().map(|e| {
            format!(
                "'{target}' now holds the archive, but is not serving yet: {e}. It is retried; \
                 or migrate it from the Databases page"
            )
        }))
    }

    /// The name the replaced database now has, for a replace.
    async fn swap_in(&self, target: &str) -> Result<Option<String>, ImportError> {
        let (staging, held) = {
            let imports = self.imports.lock().await;
            let import = imports
                .get(target)
                .ok_or_else(|| ImportError::Unavailable(format!("no import into '{target}'")))?;
            (import.staging.clone(), import.held.clone())
        };
        let Some(gate) = held else {
            self.rename(&[(&staging, target)]).await?;
            self.imports.lock().await.remove(target);
            return Ok(None);
        };
        *gate.write().await = true;
        self.pools.lock().await.remove(target);
        self.buffer_pools.lock().await.remove(target);
        let moved = async {
            let from = self.connect_raw(target).await?;
            let to = self.connect_raw(&staging).await?;
            writer::move_pending(&from, &to).await
        };
        let moved = moved.await?;
        let old = staging.replace(STAGING_PREFIX, REPLACED_PREFIX);
        self.rename(&[(target, &old), (&staging, target)]).await?;
        // Served once the rows moved across are drained into it.
        self.set_state(target, DbSchemaState::migrating()).await;
        self.imports.lock().await.remove(target);
        tracing::info!(
            database = target,
            held = moved,
            "replaced by an imported archive"
        );
        Ok(Some(old))
    }

    /// Undo [`Self::begin_import`]: drop the staging database, and for a
    /// replace store the ingest held meanwhile in the database it was held for.
    pub async fn abandon_import(&self, target: &str) {
        let Some((staging, held)) = self
            .imports
            .lock()
            .await
            .get(target)
            .map(|i| (i.staging.clone(), i.held.clone()))
        else {
            return;
        };
        if let Some(gate) = &held {
            *gate.write().await = true;
        }
        self.imports.lock().await.remove(target);
        if held.is_some() {
            match self.drain(target).await {
                Ok(drained) => self.note_drained(target, drained).await,
                Err(e) => {
                    tracing::warn!(
                        database = target,
                        "could not drain the ingest held for an import: {e}"
                    );
                    self.set_state(
                        target,
                        DbSchemaState::Pending {
                            version: schema::SCHEMA_VERSION,
                        },
                    )
                    .await;
                }
            }
        }
        if let Err(e) = self.drop_database(&staging).await {
            tracing::warn!("{e}; the next start drops it");
        }
    }

    /// Rename databases in one transaction, so a swap happens whole or not at
    /// all. Nothing may be connected to them, the TimescaleDB scheduler
    /// included, and it can reconnect between the terminate and the rename.
    async fn rename(&self, renames: &[(&str, &str)]) -> Result<(), ImportError> {
        let names: Vec<&str> = renames.iter().map(|(from, _)| *from).collect();
        let statements: String = renames
            .iter()
            .map(|(from, to)| format!("ALTER DATABASE {from} RENAME TO {to};"))
            .collect();
        let mut attempts = 0;
        loop {
            let client = self.connect_raw("postgres").await?;
            client
                .execute(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                      WHERE datname = ANY($1) AND pid <> pg_backend_pid()",
                    &[&names],
                )
                .await
                .map_err(|e| schema::db_error_detail(&e))?;
            match client
                .batch_execute(&format!("BEGIN; {statements} COMMIT"))
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) if e.code() == Some(&SqlState::OBJECT_IN_USE) && attempts < 5 => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(e) if e.code() == Some(&SqlState::DUPLICATE_DATABASE) => {
                    return Err(ImportError::Conflict(schema::db_error_detail(&e)))
                }
                Err(e) => return Err(ImportError::Unavailable(schema::db_error_detail(&e))),
            }
        }
    }

    /// Its TimescaleDB jobs run only under its scheduler, which an import stops
    /// while it loads. Asked on every migration too, so a gateway that stopped
    /// after the swap and before this still restarts it.
    async fn start_background_workers(&self, name: &str) {
        let started = async {
            self.connect_raw(name)
                .await?
                .batch_execute("SELECT _timescaledb_functions.start_background_workers()")
                .await
                .map_err(|e| schema::db_error_detail(&e))
        };
        if let Err(e) = started.await {
            tracing::warn!(
                database = name,
                "could not start its background workers: {e}"
            );
        }
    }

    async fn drop_database(&self, name: &str) -> Result<(), String> {
        self.connect_raw("postgres")
            .await?
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .await
            .map_err(|e| {
                format!(
                    "DROP DATABASE {name} failed: {}",
                    schema::db_error_detail(&e)
                )
            })
    }

    /// Drop the staging and replaced databases an import left when the gateway
    /// stopped partway, but not one another gateway on the cluster is running:
    /// its lock is held from before it creates them, and taken here before
    /// they are dropped. A replace's held ingest is in the database it was
    /// held for, which the sweep drains.
    pub async fn drop_import_leftovers(&self) -> Result<(), String> {
        let client = self.connect_raw("postgres").await?;
        let rows = client
            .query(
                "SELECT datname FROM pg_database \
                  WHERE starts_with(datname, $1) OR starts_with(datname, $2)",
                &[&STAGING_PREFIX, &REPLACED_PREFIX],
            )
            .await
            .map_err(|e| format!("database list failed: {e}"))?;
        for name in rows.iter().map(|r| r.get::<_, String>(0)) {
            let suffix = name
                .strip_prefix(STAGING_PREFIX)
                .or_else(|| name.strip_prefix(REPLACED_PREFIX))
                .unwrap_or_default();
            let abandoned: bool = client
                .query_one(
                    "SELECT pg_try_advisory_lock(hashtextextended($1, 0))",
                    &[&suffix],
                )
                .await
                .map_err(|e| schema::db_error_detail(&e))?
                .get(0);
            if abandoned {
                self.drop_database(&name).await?;
                tracing::warn!("dropped '{name}', left by an import the gateway stopped partway");
            }
        }
        Ok(())
    }
}

fn being_imported(name: &str) -> String {
    format!("database '{name}' is being imported from an archive")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

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
            secret_files: Default::default(),
            auto_create_databases: false,
            auto_migrate,
            ingest_keepalive_secs: 30.0,
            ingest_max_batch_frames: 256,
            base_path: String::new(),
            admin_dir: "/nonexistent".into(),
            app_socket: None,
        }
    }

    impl Databases {
        pub(crate) fn config(&self) -> &Config {
            &self.config
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
    pub(crate) static CLUSTER: Mutex<()> = Mutex::const_new(());

    /// A new capture database, with `then` run on it.
    pub(crate) async fn fresh_database(prefix: &str, then: &str) -> String {
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

    /// A database in v4's shape, with `days` of frames, `per_day` to a day's
    /// chunk and CAN, Modbus (unit 1, function 3) and serial in turn, the older
    /// half compressed and the rollup materialised.
    pub(crate) async fn seeded_v4_database(prefix: &str, days: u32, per_day: u32) -> String {
        let name = fresh_database(
            prefix,
            &format!(
                "ALTER TABLE public.capture_frame
                   DROP CONSTRAINT capture_frame_protocol_check,
                   DROP CONSTRAINT capture_frame_protocol_columns_check;
                 ALTER TABLE public.capture_frame
                   ADD CONSTRAINT capture_frame_protocol_check
                     CHECK (protocol IN ('can', 'modbus', 'serial')),
                   ADD CONSTRAINT capture_frame_protocol_columns_check
                     CHECK (CASE protocol WHEN 'modbus' THEN unit IS NOT NULL
                                          ELSE unit IS NULL END);
                 INSERT INTO public.capture_frame
                   (ts, protocol, id, flags, dlc, data_bytes, bus, unit, func, crc_valid)
                 SELECT now() - make_interval(secs => i * 86400.0 / {per_day}),
                        (ARRAY['can', 'modbus', 'serial'])[i % 3 + 1],
                        CASE i % 3 WHEN 0 THEN 256 WHEN 1 THEN 259 ELSE 0 END, 0, 8,
                        '\\x0102030405060708', 0,
                        CASE WHEN i % 3 = 1 THEN 1 END, CASE WHEN i % 3 = 1 THEN 3 END,
                        CASE WHEN i % 3 = 1 THEN true END
                   FROM generate_series(1, {days} * {per_day}) i;
                 SELECT compress_chunk(c) FROM show_chunks('public.capture_frame',
                   older_than => now() - INTERVAL '{} days') c;
                 UPDATE public.schema_version SET version = 4;",
                days / 2
            ),
        )
        .await;
        let client = live_databases(true).connect_raw(&name).await.unwrap();
        schema::refresh_rollup(&client).await.unwrap();
        name
    }

    /// `n` CAN frames of `id`, a millisecond apart from `from_us`.
    pub(crate) fn frames(id: u32, from_us: i64, n: i64) -> Vec<FrameRow> {
        use wslib_wiretap_protocol::ingest::RecordKind;
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

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_long_migration_holds_up_neither_a_new_database_nor_another_migration() {
        let slow = seeded_v3_database("slow", 40, 25_000).await;
        let beside = behind_database("beside").await;
        let dbs = live_databases(false);
        sweep(&dbs).await;
        dbs.start_migration(&slow).await.unwrap();

        let new = format!("{beside}_new");
        tokio::time::timeout(Duration::from_secs(5), dbs.create_database(&new))
            .await
            .expect("creating a database waited for the migration")
            .unwrap();
        dbs.start_migration(&beside).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            until_label(&dbs, &beside, "current"),
        )
        .await
        .expect("a second database's migration waited for the first");
        assert_eq!(
            state_of(&dbs, &slow).await.label(),
            "migrating",
            "the slow migration finished first, so this proved nothing"
        );
        let again = dbs.start_migration(&slow).await;
        assert!(
            matches!(&again, Err(MigrateRefusal::Conflict(m)) if m.ends_with("is migrating")),
            "{again:?}"
        );

        until_label(&dbs, &slow, "current").await;
        for name in [&slow, &beside, &new] {
            dbs.delete_database(name).await.unwrap();
        }
    }

    /// Rollup rows stored for the hour `frames(_, 1_700_000_000_000_000, _)` is in.
    async fn materialised_at_1_700_000_000(name: &str) -> i64 {
        let client = live_databases(true).connect_raw(name).await.unwrap();
        client
            .query_one(
                &format!(
                    "SELECT count(*) FROM {} \
                      WHERE bucket = date_trunc('hour', to_timestamp(1700000000))",
                    rollup_table(name).await
                ),
                &[],
            )
            .await
            .unwrap()
            .get(0)
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_span_drained_before_a_crash_is_refreshed_by_the_next_start() {
        let name = behind_database("crashed_drain").await;
        let dbs = live_databases(false);
        sweep(&dbs).await;
        dbs.write_rows(&name, &frames(0x7EB, 1_700_000_000_000_000, 4))
            .await
            .unwrap();
        let mut by_hand = dbs.connect_raw(&name).await.unwrap();
        schema::migrate(&by_hand, Some(schema::SCHEMA_VERSION - 1))
            .await
            .unwrap();
        // A rollup that covers the archive from its first frame, so the hole
        // the drain leaves is in the middle, where `rollup_status` cannot see it.
        by_hand
            .batch_execute(
                "INSERT INTO public.capture_frame (ts, protocol, id, flags, dlc, data_bytes, bus)
                 VALUES ('2022-01-01', 'can', 1, 0, 1, '\\x01', 0)",
            )
            .await
            .unwrap();
        schema::refresh_rollup(&by_hand).await.unwrap();
        // The drain commits, and the gateway dies before its refresh.
        let _ = writer::drain_pending(&mut by_hand).await.unwrap();
        assert_eq!(materialised_at_1_700_000_000(&name).await, 0);

        let restarted = live_databases(false);
        sweep(&restarted).await;
        assert_eq!(state_of(&restarted, &name).await.label(), "current");
        assert!(
            materialised_at_1_700_000_000(&name).await > 0,
            "the drained span was never materialised"
        );
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

    async fn scalar(name: &str, sql: &str) -> i64 {
        let client = live_databases(true).connect_raw(name).await.unwrap();
        client.query_one(sql, &[]).await.unwrap().get(0)
    }

    /// Per stored tag: rows in `capture_frame`, and frames the rollup counts.
    async fn per_protocol(name: &str) -> Vec<(String, i64, i64)> {
        let client = live_databases(true).connect_raw(name).await.unwrap();
        client
            .query(
                "SELECT protocol, count(*),
                        (SELECT coalesce(sum(frame_count), 0)::bigint
                           FROM public.capture_frame_hourly h WHERE h.protocol = f.protocol)
                   FROM public.capture_frame f GROUP BY 1 ORDER BY 1",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect()
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_v4_archive_relabels_its_modbus_rows_and_rollup_in_place() {
        let name = seeded_v4_database("relabel", 6, 300).await;
        let compressed = "SELECT count(*) FROM timescaledb_information.chunks \
                           WHERE hypertable_name = 'capture_frame' AND is_compressed";
        let compressed_before = scalar(&name, compressed).await;
        assert!(compressed_before > 0);
        let rollup = rollup_table(&name).await;
        let relabelled = per_protocol_relabelled(&name).await;

        let dbs = live_databases(false);
        sweep(&dbs).await;
        dbs.start_migration(&name).await.unwrap();
        until_label(&dbs, &name, "current").await;

        assert_eq!(per_protocol(&name).await, relabelled);
        assert_eq!(scalar(&name, compressed).await, compressed_before);
        assert_eq!(rollup_table(&name).await, rollup, "the rollup was rebuilt");
        assert_eq!(
            scalar(
                &name,
                &format!("SELECT count(*) FROM {rollup} WHERE protocol = 'modbus'")
            )
            .await,
            0,
            "the stored rollup still says modbus"
        );
        let client = dbs.connect_raw(&name).await.unwrap();
        let refused = client
            .batch_execute(
                "INSERT INTO public.capture_frame
                   (ts, protocol, id, flags, dlc, data_bytes, bus, unit, func, crc_valid)
                 VALUES (now(), 'modbus', 259, 0, 1, '\\x01', 0, 1, 3, true)",
            )
            .await;
        assert!(refused.is_err(), "the old tag is still accepted");
        dbs.delete_database(&name).await.unwrap();
    }

    /// [`per_protocol`] as it should read once 0005 has run.
    async fn per_protocol_relabelled(name: &str) -> Vec<(String, i64, i64)> {
        use wslib_wiretap_protocol::ingest::MODBUS_RTU_PROTOCOL;
        let relabelled: Vec<_> = per_protocol(name)
            .await
            .into_iter()
            .map(|(p, rows, rolled_up)| match p.as_str() {
                "modbus" => (MODBUS_RTU_PROTOCOL.to_string(), rows, rolled_up),
                _ => (p, rows, rolled_up),
            })
            .collect();
        assert_eq!(relabelled.len(), 3);
        relabelled
    }

    /// A v4 archive whose compressed chunks each hold more Modbus rows than
    /// TimescaleDB will decompress in one transaction, left as `half_done` by
    /// an earlier run, migrated to the end.
    async fn migrate_past_the_decompression_limit(prefix: &str, half_done: &str) {
        let name = seeded_v4_database(prefix, 6, 300).await;
        let relabelled = per_protocol_relabelled(&name).await;
        run(
            &name,
            &format!(
                "{half_done}
                 ALTER DATABASE {name}
                   SET timescaledb.max_tuples_decompressed_per_dml_transaction = 10"
            ),
        )
        .await;

        let dbs = live_databases(false);
        dbs.migrate_one(&name).await.unwrap();
        assert_eq!(per_protocol(&name).await, relabelled);
        let checks = "SELECT count(*) FROM pg_constraint \
                       WHERE conrelid = 'public.capture_frame'::regclass \
                         AND conname IN ('capture_frame_protocol_check', \
                                         'capture_frame_protocol_columns_check')";
        assert_eq!(scalar(&name, checks).await, 2);
        dbs.delete_database(&name).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_compressed_chunk_past_the_decompression_limit_still_relabels() {
        migrate_past_the_decompression_limit("decompress", "").await;
    }

    /// Where v0.1.11's run stopped: the CHECKs gone and the first chunk done.
    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_relabel_stopped_at_the_decompression_limit_resumes() {
        migrate_past_the_decompression_limit(
            "decompress_resume",
            "ALTER TABLE public.capture_frame
               DROP CONSTRAINT capture_frame_protocol_check,
               DROP CONSTRAINT capture_frame_protocol_columns_check;
             DO $$
             DECLARE c record;
             BEGIN
               SELECT format('%I.%I', chunk_schema, chunk_name)::regclass AS chunk,
                      range_start, range_end
                 INTO c FROM timescaledb_information.chunks
                WHERE hypertable_name = 'capture_frame' AND is_compressed
                ORDER BY range_start LIMIT 1;
               UPDATE public.capture_frame SET protocol = 'modbus_rtu'
                WHERE ts >= c.range_start AND ts < c.range_end AND protocol = 'modbus';
               PERFORM compress_chunk(c.chunk);
             END $$;",
        )
        .await;
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

    /// A non-superuser `LOGIN` role of its own, so rotating its password leaves
    /// the other tests' superuser alone; and `Databases` that log in as it.
    async fn as_new_role(prefix: &str, files: secrets::SecretFiles) -> (Databases, Secret) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let role = format!("{prefix}_{stamp}");
        let password = Secret::new(secrets::random_hex());
        run(
            "postgres",
            &format!(
                "CREATE ROLE {role} LOGIN NOSUPERUSER PASSWORD '{}'",
                password.expose()
            ),
        )
        .await;
        let mut config = live_databases(false).config.as_ref().clone();
        config.pg_user = role;
        config.pg_password = password.clone();
        config.secret_files = files;
        (Databases::new(Arc::new(config)), password)
    }

    async fn refuses(dbs: &Databases, password: &Secret) -> bool {
        let previous = dbs.set_pg_password(password.clone()).await;
        let refused = matches!(
            dbs.reach("postgres").await,
            Err(Unreached::Failed(e)) if e.code() == Some(&SqlState::INVALID_PASSWORD)
        );
        dbs.set_pg_password(previous).await;
        refused
    }

    fn secrets_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wiretap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("postgres-password")
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_rotated_password_works_at_once_and_a_pooled_connection_survives_it() {
        let (dbs, old) = as_new_role("p10_rotate", Default::default()).await;
        let pool = dbs.build_pool("postgres").unwrap();
        let held = pool.get().await.unwrap();

        let new = dbs.rotate_pg_password(None).await.unwrap();

        held.query_one("SELECT 1", &[]).await.unwrap();
        let fresh = (pool.get().await.unwrap(), pool.get().await.unwrap());
        fresh.1.query_one("SELECT 1", &[]).await.unwrap();
        drop(fresh);
        dbs.connect_raw("postgres").await.unwrap();
        assert!(refuses(&dbs, &old).await);
        assert!(!refuses(&dbs, &new).await);
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_rotated_password_is_saved_to_its_file() {
        let file = secrets_dir("rotate-file");
        let (dbs, _) = as_new_role(
            "p10_file",
            secrets::SecretFiles {
                pg_password: Some(file.clone()),
                admin_key: None,
            },
        )
        .await;
        let new = dbs.rotate_pg_password(Some(&file)).await.unwrap();
        assert_eq!(Secret::from_file(&file).unwrap().expose(), new.expose());
        assert!(!secrets::staged(&file).exists());
    }

    /// The crash D2 is for: the `ALTER ROLE` ran, and the rename never did.
    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_staged_password_the_cluster_took_is_promoted_at_start() {
        let file = secrets_dir("promote");
        let files = secrets::SecretFiles {
            pg_password: Some(file.clone()),
            admin_key: None,
        };
        let (rotator, old) = as_new_role("p10_promote", files.clone()).await;
        std::fs::write(&file, old.expose()).unwrap();
        let new = rotator.rotate_pg_password(None).await.unwrap();
        let _interrupted = secrets::stage(&file, new.expose()).unwrap();

        let mut config = rotator.config.as_ref().clone();
        config.pg_password = old;
        let started = Databases::new(Arc::new(config));
        started.reach_postgres().await.unwrap();
        assert_eq!(Secret::from_file(&file).unwrap().expose(), new.expose());
        assert!(!secrets::staged(&file).exists());
        started.connect_raw("postgres").await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_staged_password_the_cluster_never_took_is_discarded_at_start() {
        let file = secrets_dir("discard");
        let files = secrets::SecretFiles {
            pg_password: Some(file.clone()),
            admin_key: None,
        };
        let (dbs, old) = as_new_role("p10_discard", files).await;
        std::fs::write(&file, old.expose()).unwrap();
        let _interrupted = secrets::stage(&file, "never-applied").unwrap();

        dbs.reach_postgres().await.unwrap();
        assert_eq!(Secret::from_file(&file).unwrap().expose(), old.expose());
        assert!(!secrets::staged(&file).exists());
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

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_failed_replace_keeps_the_original_and_stores_the_ingest_held_meanwhile() {
        let name = fresh_database("unreplaced", "").await;
        let dbs = live_databases(false);
        dbs.migrate_one(&name).await.unwrap();
        dbs.write_rows(&name, &frames(0x7C0, 1_700_000_000_000_000, 4))
            .await
            .unwrap();
        let staging = dbs.begin_import(&name, true).await.unwrap();
        dbs.write_rows(&name, &frames(0x7C1, 1_700_000_000_000_000, 5))
            .await
            .unwrap();
        assert_eq!(archived(&name, 0x7C1).await, (0, 0), "not held");
        let refused = dbs.pool(&name).await.err().unwrap().to_string();
        assert!(refused.contains("being imported"), "{refused}");
        let again = dbs.begin_import(&name, true).await;
        assert!(matches!(again, Err(ImportError::Conflict(_))), "{again:?}");

        dbs.abandon_import(&name).await;
        assert!(!dbs.database_exists(&staging).await.unwrap());
        assert_eq!(archived(&name, 0x7C0).await, (4, 4));
        assert_eq!(archived(&name, 0x7C1).await, (5, 5));
        assert!(!buffer_exists(&name).await);
        assert!(dbs.pool(&name).await.is_ok());
        dbs.delete_database(&name).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn a_start_drops_what_an_interrupted_import_left_and_nothing_else() {
        let stamp = std::process::id();
        let left = [format!("__import_t{stamp}"), format!("__old_t{stamp}")];
        let kept = [
            format!("kept__old_t{stamp}"),
            format!("__import_busy{stamp}"),
            format!("__old_busy{stamp}"),
        ];
        for name in left.iter().chain(&kept) {
            run("postgres", &format!("CREATE DATABASE {name}")).await;
        }
        let dbs = live_databases(false);
        // Another gateway's import, between its connections; and a connection
        // to a leftover, as its TimescaleDB scheduler is after a restart.
        let running = dbs.connect_raw("postgres").await.unwrap();
        running
            .execute(
                "SELECT pg_advisory_lock(hashtextextended($1, 0))",
                &[&format!("busy{stamp}")],
            )
            .await
            .unwrap();
        let stray = dbs.connect_raw(&left[0]).await.unwrap();
        {
            let _cluster = CLUSTER.lock().await;
            dbs.drop_import_leftovers().await.unwrap();
        }
        for name in &left {
            assert!(!dbs.database_exists(name).await.unwrap(), "{name}");
        }
        drop((running, stray));
        for name in &kept {
            assert!(dbs.database_exists(name).await.unwrap(), "{name}");
            dbs.drop_database(name).await.unwrap();
        }
    }

    /// A fresh install's, and then an older install's that captured into it.
    #[tokio::test]
    #[ignore = "needs TimescaleDB; see live_databases"]
    async fn the_gateways_own_database_holds_no_captures_and_old_ones_are_counted_not_touched() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut config = live_databases(false).config.as_ref().clone();
        config.default_database = format!("gateway_{stamp}");
        let dbs = Databases::new(Arc::new(config));
        let own = dbs.default_database().to_string();
        dbs.create_gateway_database().await.unwrap();
        assert_eq!(version_on_disk(&dbs, &own).await, None);
        assert!(!dbs.list_names().await.unwrap().contains(&own));

        let old_shape = dbs.connect_raw(&own).await.unwrap();
        schema::migrate(&old_shape, None).await.unwrap();
        run(
            &own,
            "INSERT INTO public.capture_frame (ts, protocol, id, flags, dlc, data_bytes, bus)
             SELECT to_timestamp(1700000000 + i), 'can', 2001, 0, 1, '\\x01', 0
               FROM generate_series(1, 3) i",
        )
        .await;
        crate::keys::KeyStore::new(dbs.clone(), Some("bootstrap"))
            .bootstrap()
            .await
            .unwrap();
        let logs = wslib_telemetry::Logs::new(&["wiretap_backend"]);
        let _logging = tracing::subscriber::set_default(logs.subscriber());
        dbs.warn_of_old_captures().await;
        let warned = logs.snapshot().records;
        assert!(
            warned
                .iter()
                .any(|r| r.level == wslib_telemetry::LogLevel::Warn
                    && r.message.contains("no longer served")
                    && r.fields.contains(&("frames".into(), "3".into()))),
            "{warned:?}"
        );
        assert_eq!(archived(&own, 2001).await, (3, 3));
        assert!(!dbs.schema_states().await.contains_key(&own));
        assert!(dbs.ensure_database(&own, true).await.is_err());
        run("postgres", &format!("DROP DATABASE {own} WITH (FORCE)")).await;
    }

    pub(crate) async fn mark_migrating(dbs: &Databases, name: &str) {
        dbs.set_state(name, DbSchemaState::migrating()).await;
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
