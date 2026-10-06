//! HTTP API: query surface (ports of the desktop dbquery commands), admin
//! (keys, databases, ingest sessions, activity, daemons and their catalogues),
//! capture import, health.
//! Auth is `Authorization: Bearer <api-key>`; roles read|ingest|admin.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put, MethodRouter};
use axum::{Extension, Json, Router};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use wiretap_gateway::{
    AssignCatalog, AssignedCatalog, AssignmentConflict, CatalogRejected, DaemonList, StoredCatalog,
    UnassignParams,
};
use wiretap_protocol::import::{parse_header, parse_record, BODY_HEADER};
use wiretap_protocol::ingest::RecordKind;

use crate::catalogs::AssignError;
use crate::db;
use crate::events;
use crate::ingest::writer::FrameRow;
use crate::keys::{KeyInfo, Role};
use crate::running;
use crate::schema;
use crate::sql;
use crate::state::AppState;
use crate::types::{
    DatabaseInfo, ErrorBody, Event, EventPatch, EventsQuery, EventsResponse, FramesQuery, Health,
    ImportResult, InventoryResponse, NewEvent, PayloadsParams, PayloadsResponse, ProtocolQuery,
    SignalResponse, TimeRangeQuery,
};

type St = Arc<AppState>;

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

impl From<String> for ApiError {
    fn from(msg: String) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg)
    }
}

impl From<sql::QueryError> for ApiError {
    fn from(e: sql::QueryError) -> Self {
        match e {
            sql::QueryError::BadRequest(msg) => ApiError(StatusCode::BAD_REQUEST, msg),
            sql::QueryError::Database(msg) => ApiError(StatusCode::SERVICE_UNAVAILABLE, msg),
        }
    }
}

fn forbidden(msg: &str) -> ApiError {
    ApiError(StatusCode::FORBIDDEN, msg.to_string())
}

fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.into())
}

fn unavailable(msg: String) -> ApiError {
    ApiError(StatusCode::SERVICE_UNAVAILABLE, msg)
}

pub fn router(state: St) -> Router {
    let authed = Router::new()
        // databases
        .route(
            "/v1/databases",
            polled(get(list_databases)).merge(post(create_database)),
        )
        .route("/v1/databases/{db}", delete(delete_database))
        .route("/v1/databases/{db}/migrate", post(migrate_database))
        .route("/v1/databases/{db}/rollup/refresh", post(refresh_rollup))
        .route("/v1/db/{db}/time-bounds", get(time_bounds))
        .route("/v1/db/{db}/inventory", get(inventory))
        .route("/v1/db/{db}/frames", get(frames))
        .route("/v1/db/{db}/payloads", post(payloads))
        // events: a user's annotations on one database
        .route("/v1/db/{db}/events", get(events_list).post(events_create))
        .route(
            "/v1/db/{db}/events/{id}",
            patch(events_update).delete(events_delete),
        )
        // analytical queries
        .route("/v1/db/{db}/query/byte-changes", post(q_byte_changes))
        .route("/v1/db/{db}/query/frame-changes", post(q_frame_changes))
        .route(
            "/v1/db/{db}/query/mirror-validation",
            post(q_mirror_validation),
        )
        .route("/v1/db/{db}/query/mux-statistics", post(q_mux_statistics))
        .route("/v1/db/{db}/query/first-last", post(q_first_last))
        .route("/v1/db/{db}/query/frequency", post(q_frequency))
        .route("/v1/db/{db}/query/distribution", post(q_distribution))
        .route("/v1/db/{db}/query/gap-analysis", post(q_gap_analysis))
        .route("/v1/db/{db}/query/pattern-search", post(q_pattern_search))
        .route("/v1/queries/{id}", delete(cancel_query))
        // activity (admin)
        .route("/v1/db/{db}/activity", polled(get(activity)))
        .route("/v1/db/{db}/activity/{pid}/cancel", post(activity_cancel))
        .route("/v1/db/{db}/activity/{pid}", delete(activity_terminate))
        // capture import
        .route("/v1/db/{db}/import", post(import_capture))
        // admin
        .route("/v1/admin/keys", get(keys_list).post(keys_create))
        .route("/v1/admin/keys/{id}", delete(keys_delete))
        .route("/v1/admin/keys/{id}/revoke", post(keys_revoke))
        .route("/v1/admin/keys/{id}/restore", post(keys_restore))
        .route("/v1/admin/ingest-sessions", polled(get(ingest_sessions)))
        .route("/v1/admin/daemons", polled(get(daemons)))
        .route(
            "/v1/admin/assignments",
            put(assignments_put).delete(assignments_delete),
        )
        .route("/v1/admin/catalogs/{sha}", get(catalog_get))
        .route("/v1/admin/logs", polled(get(logs)))
        .layer(middleware::from_fn_with_state(state.clone(), auth_mw));

    // Admin SPA: static files with index.html fallback (client-side tabs).
    // Auth happens in the browser (the SPA stores an admin key and calls the
    // /v1/admin endpoints) — the static assets themselves are not secret.
    let admin_dir = std::path::PathBuf::from(
        std::env::var("WIRETAP_ADMIN_DIR").unwrap_or_else(|_| "/usr/share/wiretap-admin".into()),
    );
    let admin = tower_http::services::ServeDir::new(&admin_dir).fallback(
        tower_http::services::ServeFile::new(admin_dir.join("index.html")),
    );

    // The bundle is polled too: every SPA load pulls it.
    let admin = Router::new()
        .nest_service("/admin", admin)
        .layer(middleware::from_fn(mark_routine));

    Router::new()
        .route("/v1/health", polled(get(health)))
        .merge(authed)
        .merge(admin)
        // Outermost, so it wraps `auth_mw` and sees the 401s too.
        .layer(middleware::from_fn(access_log_mw))
        .with_state(state)
}

/// Polled rather than requested — a healthcheck, a tab refreshing itself — so
/// a success is logged at DEBUG. Declared on the route, because which routes
/// are polled is the route table's to know.
#[derive(Clone, Copy)]
struct Routine;

async fn mark_routine(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    response.extensions_mut().insert(Routine);
    response
}

fn polled(routes: MethodRouter<St>) -> MethodRouter<St> {
    routes.layer(middleware::from_fn(mark_routine))
}

/// Method, path, status, duration, peer, key *name*. Never a header.
async fn access_log_mw(
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let started = Instant::now();

    let response = next.run(req).await;

    let status = response.status().as_u16();
    let ms = started.elapsed().as_millis();
    let key = response
        .extensions()
        .get::<KeyInfo>()
        .map_or("-", |k| k.name.as_str());
    let routine = response.extensions().get::<Routine>().is_some();
    if status >= 500 {
        tracing::error!("{method} {uri} {status} {ms}ms peer={peer} key={key}");
    } else if status >= 400 {
        tracing::warn!("{method} {uri} {status} {ms}ms peer={peer} key={key}");
    } else if routine {
        tracing::debug!("{method} {uri} {status} {ms}ms peer={peer} key={key}");
    } else {
        tracing::info!("{method} {uri} {status} {ms}ms peer={peer} key={key}");
    }
    response
}

async fn auth_mw(
    State(state): State<St>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let key = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(ApiError(
            StatusCode::UNAUTHORIZED,
            "missing bearer token".into(),
        ))?;
    let info = state
        .keys
        .validate(key)
        .await
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "invalid API key".into()))?;
    req.extensions_mut().insert(info.clone());
    let mut response = next.run(req).await;
    // Also on the response: `access_log_mw` wraps this one, so by the time it
    // logs, the request it could have read this from is gone.
    response.extensions_mut().insert(info);
    Ok(response)
}

/// Database access for read-style endpoints: role must allow reads and a
/// pinned key may only see its own database.
fn check_read(key: &KeyInfo, db: &str) -> Result<(), ApiError> {
    if !key.role.allows_read() {
        return Err(forbidden("key role does not allow reads"));
    }
    if let Some(pin) = &key.database_pin {
        if pin != db {
            return Err(forbidden("key is pinned to another database"));
        }
    }
    Ok(())
}

fn check_admin(key: &KeyInfo) -> Result<(), ApiError> {
    if !key.role.allows_admin() {
        return Err(forbidden("admin role required"));
    }
    Ok(())
}

async fn client_for(state: &St, db: &str) -> Result<deadpool_postgres::Object, ApiError> {
    let pool = state.dbs.pool(db).await.map_err(not_found)?;
    pool.get()
        .await
        .map_err(|e| ApiError(StatusCode::SERVICE_UNAVAILABLE, format!("pool: {e}")))
}

// ---------------------------------------------------------------------------
// Health / databases
// ---------------------------------------------------------------------------

/// Unauthenticated — the compose healthcheck curls it with no key — so this
/// carries a consensus word only, never a database name. Names are for
/// `/v1/databases`, which at least requires a read-role key.
async fn health(State(state): State<St>) -> Json<Health> {
    let db_ok = state.dbs.connect_raw("postgres").await.is_ok();
    Json(Health {
        status: if db_ok { "ok" } else { "degraded" }.into(),
        version: crate::VERSION.into(),
        db_ok,
        schema: schema_consensus(&state.dbs.schema_states().await),
    })
}

/// One word for how the whole deployment stands: the shared version when every
/// database agrees, else the most urgent thing happening.
///
/// For external monitoring — this is the only schema signal available without an
/// API key, so a check can alert on a half-migrated deployment. The admin UI
/// does not use it; it holds a key and draws the detail from `/v1/databases`.
fn schema_consensus(states: &std::collections::HashMap<String, db::DbSchemaState>) -> String {
    use db::DbSchemaState::*;
    if states.is_empty() {
        return "unknown".into();
    }
    if states.values().any(|s| matches!(s, Failed { .. })) {
        return "failed".into();
    }
    if states.values().any(|s| matches!(s, Migrating { .. })) {
        return "migrating".into();
    }
    // Before the version fold: a database that is behind and *not* being
    // migrated still refuses reads, and folding it in would report a tidy
    // consensus while nothing could be read.
    if states.values().any(|s| matches!(s, Pending { .. })) {
        return "behind".into();
    }
    let mut versions: Vec<i32> = states.values().filter_map(|s| s.version()).collect();
    versions.sort_unstable();
    versions.dedup();
    match versions.as_slice() {
        [v] => format!("v{v}"),
        _ => "mixed".into(),
    }
}

/// The rollup's state for one database, as the admin UI shows it.
///
/// Delegates to [`schema::rollup_status`] rather than asking its own question:
/// an earlier version measured only the newest stored bucket, which reports a
/// rollup with a hole underneath as healthy — the exact state worth showing.
async fn rollup_state(dbs: &db::Databases, name: &str) -> Option<(&'static str, Option<i64>)> {
    // Not pooled: a pool keeps its idle connection for good, so probing every
    // database through one left a backend open per database for days. A
    // connect per poll costs little, as the UI polls only while something is busy.
    let client = dbs.connect_raw(name).await.ok()?;
    match schema::rollup_status(&client).await.ok()? {
        schema::RollupState::Empty => Some(("empty", None)),
        schema::RollupState::Incomplete => Some(("incomplete", None)),
        schema::RollupState::Covered { lag_secs } => Some(("covered", Some(lag_secs))),
    }
}

/// Not pooled, for the reason `rollup_state` gives.
async fn migration_progress(dbs: &db::Databases, name: &str) -> Option<schema::MigrationProgress> {
    let client = dbs.connect_raw(name).await.ok()?;
    schema::migration_progress(&client).await.ok()?
}

/// Materialise the hourly rollup. Returns as soon as it has started — it is
/// minutes on a large archive — and the state is read back from
/// `GET /v1/databases`.
async fn refresh_rollup(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin(&key)?;
    // Server-side, not just the button's `disabled`: rebuilding the rollup of a
    // database that has not migrated would run against a schema that has no
    // capture_frame_hourly to refresh.
    if !matches!(
        state.dbs.schema_states().await.get(&db),
        Some(db::DbSchemaState::Current { .. })
    ) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("database '{db}' is not at the current schema"),
        ));
    }
    let dbs = state.dbs.clone();
    let name = db.clone();
    tokio::spawn(async move {
        let _ = dbs.refresh_rollup(&name).await;
    });
    Ok(Json(json!({ "status": "started", "database": db })))
}

/// Migrate one database to the current schema. Returns as soon as it has
/// started; the state, and the chunks done of a long one, are read back from
/// `GET /v1/databases`.
async fn migrate_database(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin(&key)?;
    state.dbs.start_migration(&db).await.map_err(|e| match e {
        db::MigrateRefusal::NotFound(m) => not_found(m),
        db::MigrateRefusal::Conflict(m) => ApiError(StatusCode::CONFLICT, m),
        db::MigrateRefusal::Unavailable(m) => unavailable(m),
    })?;
    Ok(Json(json!({ "status": "started" })))
}

/// [`DatabaseInfo`] with a migration's progress and the ingest buffered
/// meanwhile, until the shared type carries them. The desktop's parser ignores
/// fields it does not know.
#[derive(Serialize)]
struct DatabaseRow {
    #[serde(flatten)]
    info: DatabaseInfo,
    migration: Option<schema::MigrationProgress>,
    buffered_rows: Option<u64>,
}

#[derive(Serialize)]
struct DatabaseRows {
    databases: Vec<DatabaseRow>,
    schema_version: i32,
    /// Whether a database behind is migrated without being asked.
    auto_migrate: bool,
}

async fn list_databases(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
) -> Result<Json<DatabaseRows>, ApiError> {
    if !key.role.allows_read() {
        return Err(forbidden("key role does not allow reads"));
    }
    let client = state
        .dbs
        .connect_raw("postgres")
        .await
        .map_err(ApiError::from)?;
    let rows = client
        .query(
            "SELECT datname, pg_database_size(datname) AS size_bytes FROM pg_database \
             WHERE NOT datistemplate AND datname <> 'postgres' ORDER BY datname",
            &[],
        )
        .await
        .map_err(|e| ApiError::from(format!("database list failed: {e}")))?;
    let states = state.dbs.schema_states().await;
    let rebuilding = state.dbs.rollup_rebuilds().await;
    let buffered = state.dbs.buffered_rows().await;
    let mut databases = Vec::new();
    for r in &rows {
        let name: String = r.get("datname");
        // NULL for a database dropped since the list was read.
        let Some(size_bytes) = r.get("size_bytes") else {
            continue;
        };
        // Same filter the sweep uses: a name this gateway would never manage is
        // not a capture database, and listing it as "unknown" would report the
        // deployment as mixed forever.
        if !db::valid_database_name(&name) {
            continue;
        }
        if key.database_pin.as_ref().is_some_and(|pin| *pin != name) {
            continue;
        }
        let schema = states.get(&name);
        // Only worth asking a database that is actually usable; one mid-migration
        // would refuse the connection anyway.
        let (rollup, migration) = match schema {
            Some(db::DbSchemaState::Current { .. }) => {
                (rollup_state(&state.dbs, &name).await, None)
            }
            Some(_) => (None, migration_progress(&state.dbs, &name).await),
            None => (None, None),
        };
        let info = DatabaseInfo {
            size_bytes,
            schema_state: schema.map_or("unknown", |s| s.label()).into(),
            schema_version: schema.and_then(|s| s.version()),
            busy_secs: schema.and_then(|s| s.busy_secs()),
            schema_error: match schema {
                Some(db::DbSchemaState::Failed { error, .. }) => Some(error.clone()),
                _ => None,
            },
            rollup_state: rollup.map(|(st, _)| st.into()),
            rollup_lag_secs: rollup.and_then(|(_, lag)| lag),
            rollup_busy_secs: rebuilding.get(&name).copied(),
            name,
        };
        databases.push(DatabaseRow {
            buffered_rows: buffered.get(&info.name).copied(),
            info,
            migration,
        });
    }
    // The version they are all headed for, so the UI can render "v0 → v1"
    // without hardcoding what current means.
    Ok(Json(DatabaseRows {
        databases,
        schema_version: crate::schema::SCHEMA_VERSION,
        auto_migrate: state.dbs.auto_migrate(),
    }))
}

#[derive(Deserialize)]
struct CreateDatabaseBody {
    name: String,
}

async fn create_database(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Json(body): Json<CreateDatabaseBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin(&key)?;
    state
        .dbs
        .create_database(&body.name)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "created": body.name })))
}

async fn delete_database(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin(&key)?;
    // Refuse while a device is actively ingesting into this database.
    if state.sessions.list().await.iter().any(|s| s.database == db) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("database '{db}' is being ingested — stop the device first"),
        ));
    }
    state
        .dbs
        .delete_database(&db)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "deleted": db })))
}

// ---------------------------------------------------------------------------
// Read endpoints
// ---------------------------------------------------------------------------

async fn time_bounds(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
    Query(q): Query<ProtocolQuery>,
) -> Result<Json<crate::types::TimeBounds>, ApiError> {
    check_read(&key, &db)?;
    let client = client_for(&state, &db).await?;
    Ok(Json(sql::time_bounds(&client, q.protocol).await?))
}

async fn inventory(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
    Query(range): Query<TimeRangeQuery>,
) -> Result<Json<InventoryResponse>, ApiError> {
    check_read(&key, &db)?;
    let client = client_for(&state, &db).await?;
    let entries = sql::inventory(&client, range.start, range.end, range.protocol).await?;
    Ok(Json(InventoryResponse { entries }))
}

async fn frames(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
    Query(q): Query<FramesQuery>,
) -> Result<Json<crate::types::FrameBatch>, ApiError> {
    check_read(&key, &db)?;
    let limit = q.limit.unwrap_or(1000).min(5000);
    let client = client_for(&state, &db).await?;
    Ok(Json(
        sql::frames_batch(
            &client,
            q.start,
            q.end,
            q.after.as_deref(),
            limit,
            q.protocol,
        )
        .await?,
    ))
}

async fn payloads(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
    Json(p): Json<PayloadsParams>,
) -> Result<Json<PayloadsResponse>, ApiError> {
    check_read(&key, &db)?;
    let client = client_for(&state, &db).await?;
    let payloads = sql::payloads(&client, &p).await?;
    Ok(Json(PayloadsResponse { payloads }))
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------
// Gated by `check_read`, writes included: annotating an archive is a user's
// act, users hold read keys, and the alternative is handing every desktop an
// admin key. A pinned read key annotates only its own database, as it reads
// only that.

async fn events_list(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<EventsResponse>, ApiError> {
    check_read(&key, &db)?;
    let client = client_for(&state, &db).await?;
    // Uncapped, unlike `frames`: there is no cursor here, so a cap would
    // override the client's limit in silence.
    let events = events::list(&client, q.start, q.end, q.limit.unwrap_or(1000)).await?;
    Ok(Json(EventsResponse { events }))
}

async fn events_create(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
    Json(new): Json<NewEvent>,
) -> Result<(StatusCode, Json<Event>), ApiError> {
    check_read(&key, &db)?;
    let client = client_for(&state, &db).await?;
    Ok((
        StatusCode::CREATED,
        Json(events::create(&client, &new).await?),
    ))
}

async fn events_update(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path((db, id)): Path<(String, i64)>,
    Json(patch): Json<EventPatch>,
) -> Result<Json<Event>, ApiError> {
    check_read(&key, &db)?;
    let client = client_for(&state, &db).await?;
    events::update(&client, id, &patch)
        .await?
        .map(Json)
        .ok_or_else(|| not_found("event not found"))
}

async fn events_delete(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path((db, id)): Path<(String, i64)>,
) -> Result<StatusCode, ApiError> {
    check_read(&key, &db)?;
    let client = client_for(&state, &db).await?;
    if events::delete(&client, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found("event not found"))
    }
}

// ---------------------------------------------------------------------------
// Analytical queries — one handler per type, sharing the guard pattern
// ---------------------------------------------------------------------------

/// Run a query with cancellation registered under its query_id.
macro_rules! query_handler {
    ($name:ident, $params:ty, $result:ty, $sql_fn:path) => {
        async fn $name(
            State(state): State<St>,
            Extension(key): Extension<KeyInfo>,
            Path(db): Path<String>,
            Json(p): Json<$params>,
        ) -> Result<Json<$result>, ApiError> {
            check_read(&key, &db)?;
            let client = client_for(&state, &db).await?;
            let query_id = p.query_id.clone().unwrap_or_else(|| {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                format!("{}_{}", stringify!($name), nanos)
            });
            let _guard = running::QueryGuard::new(query_id, client.cancel_token()).await;
            let result = $sql_fn(&client, &p).await?;
            Ok(Json(result))
        }
    };
}

query_handler!(
    q_byte_changes,
    crate::types::ByteChangesParams,
    crate::types::ByteChangeQueryResult,
    sql::byte_changes
);
query_handler!(
    q_frame_changes,
    crate::types::FrameChangesParams,
    crate::types::FrameChangeQueryResult,
    sql::frame_changes
);
query_handler!(
    q_mirror_validation,
    crate::types::MirrorValidationParams,
    crate::types::MirrorValidationQueryResult,
    sql::mirror_validation
);
query_handler!(
    q_mux_statistics,
    crate::types::MuxStatisticsParams,
    crate::types::MuxStatisticsQueryResult,
    sql::mux_statistics
);
query_handler!(
    q_first_last,
    crate::types::FirstLastParams,
    crate::types::FirstLastQueryResult,
    sql::first_last
);
query_handler!(
    q_frequency,
    crate::types::FrequencyParams,
    crate::types::FrequencyQueryResult,
    sql::frequency
);
query_handler!(
    q_distribution,
    crate::types::DistributionParams,
    crate::types::DistributionQueryResult,
    sql::distribution
);
query_handler!(
    q_gap_analysis,
    crate::types::GapAnalysisParams,
    crate::types::GapAnalysisQueryResult,
    sql::gap_analysis
);
query_handler!(
    q_pattern_search,
    crate::types::PatternSearchParams,
    crate::types::PatternSearchQueryResult,
    sql::pattern_search
);

async fn cancel_query(
    Extension(key): Extension<KeyInfo>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !key.role.allows_read() {
        return Err(forbidden("key role does not allow reads"));
    }
    let cancelled = running::cancel(&id).await?;
    if cancelled {
        Ok(Json(json!({ "cancelled": id })))
    } else {
        Err(not_found(format!("query not found: {id}")))
    }
}

// ---------------------------------------------------------------------------
// Activity (admin)
// ---------------------------------------------------------------------------

async fn activity(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
) -> Result<Json<crate::types::DatabaseActivityResult>, ApiError> {
    check_admin(&key)?;
    let client = client_for(&state, &db).await?;
    Ok(Json(sql::activity(&client, &db).await?))
}

async fn activity_cancel(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path((db, pid)): Path<(String, i32)>,
) -> Result<Json<SignalResponse>, ApiError> {
    check_admin(&key)?;
    let client = client_for(&state, &db).await?;
    Ok(Json(SignalResponse {
        ok: sql::signal_backend(&client, pid, false).await?,
    }))
}

async fn activity_terminate(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path((db, pid)): Path<(String, i32)>,
) -> Result<Json<SignalResponse>, ApiError> {
    check_admin(&key)?;
    let client = client_for(&state, &db).await?;
    Ok(Json(SignalResponse {
        ok: sql::signal_backend(&client, pid, true).await?,
    }))
}

// ---------------------------------------------------------------------------
// Capture import
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ImportQuery {
    #[serde(default)]
    create: bool,
}

const IMPORT_CHUNK_ROWS: usize = 8192;

/// Streaming capture import: the body is a `wiretap_protocol::import` header
/// then records, COPYed in chunks as it streams.
async fn import_capture(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
    Query(q): Query<ImportQuery>,
    req: Request<Body>,
) -> Result<Json<ImportResult>, ApiError> {
    if !(key.role.allows_ingest() || key.role.allows_admin()) {
        return Err(forbidden("ingest or admin role required"));
    }
    if let Some(pin) = &key.database_pin {
        if pin != &db {
            return Err(forbidden("key is pinned to another database"));
        }
    }
    let t0 = Instant::now();
    let mut stream = req.into_body().into_data_stream();
    let mut next_chunk = async || {
        stream
            .try_next()
            .await
            .map_err(|e| ApiError::from(format!("body read failed: {e}")))
    };
    let mut pending: Vec<u8> = Vec::new();
    let header = loop {
        if let Some(n) = parse_header(&pending)? {
            break n;
        }
        let Some(bytes) = next_chunk().await? else {
            return Err(ApiError::from(format!(
                "import body ends before its {BODY_HEADER}-byte header"
            )));
        };
        pending.extend_from_slice(&bytes);
    };
    pending.drain(..header);
    state
        .dbs
        .ensure_database(&db, q.create)
        .await
        .map_err(not_found)?;

    let mut rows: Vec<FrameRow> = Vec::with_capacity(IMPORT_CHUNK_ROWS);
    let mut imported: u64 = 0;
    loop {
        let read = import_rows(&pending, &mut rows)?;
        pending.drain(..read);
        let chunk = next_chunk().await?;
        if rows.len() >= IMPORT_CHUNK_ROWS || (chunk.is_none() && !rows.is_empty()) {
            state
                .dbs
                .write_rows(&db, &rows)
                .await
                .map_err(|e| match e {
                    db::WriteError::Db(db::DbError::Refused(m)) => not_found(m),
                    db::WriteError::Db(db::DbError::Unavailable(m)) => unavailable(m),
                    db::WriteError::Copy(e) => sql::QueryError::from(e).into(),
                })?;
            imported += rows.len() as u64;
            rows.clear();
        }
        let Some(bytes) = chunk else { break };
        pending.extend_from_slice(&bytes);
    }
    if !pending.is_empty() {
        return Err(ApiError::from(format!(
            "truncated record: {} trailing bytes",
            pending.len()
        )));
    }

    Ok(Json(ImportResult {
        imported,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    }))
}

fn import_rows(buf: &[u8], rows: &mut Vec<FrameRow>) -> Result<usize, String> {
    let mut read = 0;
    while let Some((r, consumed)) = parse_record(&buf[read..])? {
        read += consumed;
        rows.push(FrameRow::new(
            r.ts_us,
            RecordKind::Can,
            r.id_flags,
            r.flags,
            r.bus,
            r.payload,
        ));
    }
    Ok(read)
}

// ---------------------------------------------------------------------------
// Admin: keys + ingest sessions
// ---------------------------------------------------------------------------

async fn keys_list(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin(&key)?;
    Ok(Json(json!({ "keys": state.keys.list().await? })))
}

#[derive(Deserialize)]
struct CreateKeyBody {
    name: String,
    role: String,
    database_pin: Option<String>,
}

async fn keys_create(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Json(body): Json<CreateKeyBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin(&key)?;
    let role = Role::parse(&body.role)
        .ok_or_else(|| ApiError::from(format!("unknown role '{}'", body.role)))?;
    let (id, plaintext) = state
        .keys
        .create(&body.name, role, body.database_pin.as_deref())
        .await?;
    // Plaintext is returned ONCE and never stored
    Ok(Json(json!({ "id": id, "key": plaintext })))
}

async fn keys_revoke(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin(&key)?;
    if state.keys.revoke(id).await? {
        Ok(Json(json!({ "revoked": id })))
    } else {
        Err(not_found(format!("key not found or already revoked: {id}")))
    }
}

async fn keys_restore(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin(&key)?;
    if state.keys.unrevoke(id).await? {
        Ok(Json(json!({ "restored": id })))
    } else {
        Err(not_found(format!("key not found or not revoked: {id}")))
    }
}

async fn keys_delete(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin(&key)?;
    if state.keys.delete(id).await? {
        Ok(Json(json!({ "deleted": id })))
    } else {
        Err(not_found(format!("key not found: {id}")))
    }
}

async fn ingest_sessions(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin(&key)?;
    Ok(Json(json!({ "sessions": state.sessions.list().await })))
}

// ---------------------------------------------------------------------------
// Admin: daemons and their catalogues
// ---------------------------------------------------------------------------

impl IntoResponse for AssignError {
    fn into_response(self) -> Response {
        match self {
            AssignError::Rejected(findings) => {
                let error = findings
                    .iter()
                    .map(|f| format!("{}: {}", f.field, f.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                let body = CatalogRejected { error, findings };
                (StatusCode::BAD_REQUEST, Json(body)).into_response()
            }
            AssignError::Conflict(current) => {
                let error = "the assignment changed since it was read".into();
                let body = AssignmentConflict { error, current };
                (StatusCode::CONFLICT, Json(body)).into_response()
            }
            AssignError::Database(e) => unavailable(e).into_response(),
        }
    }
}

async fn daemons(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
) -> Result<Json<DaemonList>, ApiError> {
    check_admin(&key)?;
    Ok(Json(state.catalogs.daemons().await.map_err(unavailable)?))
}

async fn assignments_put(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Json(body): Json<AssignCatalog>,
) -> Response {
    if let Err(e) = check_admin(&key) {
        return e.into_response();
    }
    match state.catalogs.assign(&body, &key.name).await {
        Ok(assignment) => Json(AssignedCatalog {
            daemon_id: body.daemon_id,
            interface: body.interface,
            assignment,
            warnings: Vec::new(),
        })
        .into_response(),
        Err(e) => e.into_response(),
    }
}

async fn assignments_delete(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Query(params): Query<UnassignParams>,
) -> Response {
    if let Err(e) = check_admin(&key) {
        return e.into_response();
    }
    match state.catalogs.clear(&params, &key.name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn catalog_get(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(sha): Path<String>,
) -> Result<Json<StoredCatalog>, ApiError> {
    check_admin(&key)?;
    let blob_sha = hex::decode(&sha)
        .ok()
        .and_then(|b| <[u8; 20]>::try_from(b).ok())
        .ok_or_else(|| ApiError::from(format!("{sha:?} is not a SHA-1")))?;
    let stored = state
        .catalogs
        .stored(&blob_sha)
        .await
        .map_err(unavailable)?;
    stored
        .map(Json)
        .ok_or_else(|| not_found(format!("no catalogue {sha}")))
}

#[derive(Deserialize)]
struct LogsQuery {
    level: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct LogsResponse {
    records: Vec<crate::logbuf::LogRecord>,
    capacity: usize,
}

/// Recent log records, newest first.
async fn logs(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Query(q): Query<LogsQuery>,
) -> Result<Json<LogsResponse>, ApiError> {
    check_admin(&key)?;
    let level = q
        .level
        .as_deref()
        .map(|s| s.parse().map_err(|_| format!("unknown level '{s}'")))
        .transpose()?;
    Ok(Json(LogsResponse {
        records: state.logs.snapshot(level, q.limit.unwrap_or(200)),
        capacity: state.logs.capacity(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Instant;
    use wiretap_protocol::can::CanFrame;
    use wiretap_protocol::import;
    use wiretap_protocol::ingest::RecordFields;

    fn states(pairs: &[(&str, db::DbSchemaState)]) -> HashMap<String, db::DbSchemaState> {
        pairs
            .iter()
            .map(|(n, s)| (n.to_string(), s.clone()))
            .collect()
    }

    /// This is what an external monitor alerts on, and it is the only schema
    /// signal available without an API key.
    #[test]
    fn the_consensus_reports_the_shared_version_when_they_agree() {
        let s = states(&[
            ("a", db::DbSchemaState::Current { version: 1 }),
            ("b", db::DbSchemaState::Current { version: 1 }),
        ]);
        assert_eq!(schema_consensus(&s), "v1");
    }

    /// A database behind and *not* being migrated still refuses reads and
    /// writes. Folding it into the version tally reported a tidy "v0" while
    /// nothing worked — which is what an operator running with automatic
    /// migration off would have seen.
    #[test]
    fn a_database_left_behind_is_not_a_clean_consensus() {
        let s = states(&[
            ("a", db::DbSchemaState::Current { version: 1 }),
            ("b", db::DbSchemaState::Pending { version: 0 }),
        ]);
        assert_eq!(schema_consensus(&s), "behind");

        let all_behind = states(&[("a", db::DbSchemaState::Pending { version: 0 })]);
        assert_eq!(schema_consensus(&all_behind), "behind");
    }

    /// Most urgent first: a failure outranks a migration in progress, which
    /// outranks a version disagreement.
    #[test]
    fn the_consensus_reports_the_most_urgent_state() {
        let s = states(&[
            ("a", db::DbSchemaState::Current { version: 1 }),
            (
                "b",
                db::DbSchemaState::Migrating {
                    since: Instant::now(),
                },
            ),
        ]);
        assert_eq!(schema_consensus(&s), "migrating");

        let s = states(&[
            (
                "a",
                db::DbSchemaState::Migrating {
                    since: Instant::now(),
                },
            ),
            (
                "b",
                db::DbSchemaState::Failed {
                    version: 0,
                    error: "boom".into(),
                    since: Instant::now(),
                },
            ),
        ]);
        assert_eq!(schema_consensus(&s), "failed");
    }

    /// Never a database name: this endpoint is unauthenticated.
    #[test]
    fn the_consensus_never_leaks_a_database_name() {
        let s = states(&[(
            "site_a_can",
            db::DbSchemaState::Failed {
                version: 0,
                error: "connection to site_a_can refused".into(),
                since: Instant::now(),
            },
        )]);
        assert!(!schema_consensus(&s).contains("site_a_can"));
    }

    #[test]
    fn a_query_blames_the_client_only_for_its_request() {
        let status = |e| ApiError::from(e).into_response().status();
        assert_eq!(
            status(sql::QueryError::BadRequest("bad cursor".into())),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(sql::QueryError::Database("Query failed: db error".into())),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn an_empty_cluster_is_unknown_not_a_version() {
        assert_eq!(schema_consensus(&HashMap::new()), "unknown");
    }

    /// The router over `dbs`, taking the key `bootstrap` as admin.
    async fn gateway(dbs: db::Databases) -> std::net::SocketAddr {
        let sessions = crate::ingest::Sessions::default();
        let state = Arc::new(AppState {
            keys: crate::keys::KeyStore::new(dbs.clone(), Some("bootstrap")),
            catalogs: crate::catalogs::Catalogs::new(dbs.clone(), sessions.clone()),
            dbs,
            sessions,
            logs: crate::logbuf::LogBuffer::new(16),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await });
        addr
    }

    async fn request(
        method: &str,
        path: &str,
        key: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        let at = gateway(db::tests::unreachable_databases(true)).await;
        request_to(at, method, path, key, body).await
    }

    async fn request_to(
        at: std::net::SocketAddr,
        method: &str,
        path: &str,
        key: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        send_to(at, method, path, key, "application/json", body.as_bytes()).await
    }

    async fn send(
        method: &str,
        path: &str,
        key: Option<&str>,
        content_type: &str,
        body: &[u8],
    ) -> (u16, serde_json::Value) {
        let at = gateway(db::tests::unreachable_databases(true)).await;
        send_to(at, method, path, key, content_type, body).await
    }

    /// The status, and the body as JSON (`null` when it is not).
    async fn send_to(
        at: std::net::SocketAddr,
        method: &str,
        path: &str,
        key: Option<&str>,
        content_type: &str,
        body: &[u8],
    ) -> (u16, serde_json::Value) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(at).await.unwrap();
        let auth = key.map_or(String::new(), |k| format!("Authorization: Bearer {k}\r\n"));
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n{auth}\
             Content-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap_or_default())
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see db::tests::live_databases"]
    async fn migrate_starts_one_behind_database_and_leaves_the_others_behind() {
        use db::tests::{archived, buffer_exists, frames, until_label, version_on_disk};
        let chosen = db::tests::behind_database("chosen").await;
        let other = db::tests::behind_database("other").await;
        let dbs = db::tests::live_databases(false);
        dbs.migrate_all().await;
        for name in [&chosen, &other] {
            dbs.write_rows(name, &frames(0x7D0, 1_700_000_000_000_000, 3))
                .await
                .unwrap();
        }
        let at = gateway(dbs.clone()).await;
        let migrate = |name: &str| {
            let path = format!("/v1/databases/{name}/migrate");
            async move { request_to(at, "POST", &path, Some("bootstrap"), None).await }
        };

        let (status, body) = migrate(&chosen).await;
        assert_eq!((status, &body), (200, &json!({ "status": "started" })));
        let (status, _) = migrate(&chosen).await;
        assert_eq!(status, 409, "a second start while it runs");

        until_label(&dbs, &chosen, "current").await;
        assert_eq!(
            version_on_disk(&dbs, &chosen).await,
            Some(schema::SCHEMA_VERSION)
        );
        assert_eq!(archived(&chosen, 0x7D0).await, (3, 3));
        assert_eq!(db::tests::state_of(&dbs, &other).await.label(), "pending");
        assert_eq!(
            version_on_disk(&dbs, &other).await,
            Some(schema::SCHEMA_VERSION - 1)
        );
        assert!(
            buffer_exists(&other).await,
            "the other's buffer was drained"
        );

        let (status, _) = migrate(&chosen).await;
        assert_eq!(status, 409, "a start on a current database");
        let (status, _) = migrate("no_such_database").await;
        assert_eq!(status, 404);

        for name in [&chosen, &other] {
            dbs.delete_database(name).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "needs TimescaleDB; see db::tests::live_databases"]
    async fn the_databases_list_shows_a_migration_advancing_chunk_by_chunk() {
        let name = db::tests::seeded_v3_database("progress", 40, 2880).await;
        let dbs = db::tests::live_databases(false);
        dbs.migrate_all().await;
        let at = gateway(dbs.clone()).await;
        let path = format!("/v1/databases/{name}/migrate");
        let (status, _) = request_to(at, "POST", &path, Some("bootstrap"), None).await;
        assert_eq!(status, 200);

        // Read as the list reads it, but without a connection to every other
        // database in the cluster between samples.
        let client = dbs.connect_raw(&name).await.unwrap();
        let mut seen = Vec::new();
        let mut listed = serde_json::Value::Null;
        tokio::time::timeout(std::time::Duration::from_secs(120), async {
            while db::tests::state_of(&dbs, &name).await.label() != "current" {
                let now = schema::migration_progress(&client).await.unwrap();
                let done = now.as_ref().and_then(|p| p.chunks_done);
                if done.is_some() && seen.last() != done.as_ref() {
                    seen.extend(done);
                }
                if listed.is_null() && now.is_some_and(|p| p.phase == "rollup") {
                    let (_, list) =
                        request_to(at, "GET", "/v1/databases", Some("bootstrap"), None).await;
                    listed = list["databases"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|d| d["name"] == name.as_str())
                        .unwrap()["migration"]
                        .clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the migration never finished");

        let total = listed["chunks_total"].as_i64().unwrap();
        assert!(total >= 40, "{listed}");
        assert!(seen.len() > 2, "chunks_done never advanced: {seen:?}");
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
        assert_eq!(seen.last().copied(), Some(total as i32));
        assert_eq!(listed["chunks_done"], total, "{listed}");
        assert_eq!(listed["rows_done"], 40 * 2880, "{listed}");
        for field in [
            "current_chunk_start",
            "compressed_left",
            "uncompressed_left",
            "avg_s_compressed",
            "avg_s_uncompressed",
            "last_error",
            "updated_at",
        ] {
            assert!(listed.get(field).is_some(), "no {field}: {listed}");
        }
        dbs.delete_database(&name).await.unwrap();
    }

    fn assign(content: &str, provenance: serde_json::Value) -> serde_json::Value {
        json!({
            "daemon_id": "bench",
            "interface": "/dev/ttyUSB0",
            "content": content,
            "provenance": provenance,
        })
    }

    #[tokio::test]
    async fn the_admin_catalogue_routes_need_a_key() {
        for (method, path) in [
            ("GET", "/v1/admin/daemons"),
            ("PUT", "/v1/admin/assignments"),
            (
                "DELETE",
                "/v1/admin/assignments?daemon_id=bench&interface=can0",
            ),
            ("GET", "/v1/admin/catalogs/00"),
        ] {
            assert_eq!(request(method, path, None, None).await.0, 401, "{path}");
        }
    }

    #[tokio::test]
    async fn a_catalogue_that_does_not_validate_is_a_400_with_its_findings() {
        let body = assign("[meta]\nversion = 0\n", json!({}));
        let (status, rejected) = request(
            "PUT",
            "/v1/admin/assignments",
            Some("bootstrap"),
            Some(body),
        )
        .await;
        assert_eq!(status, 400);
        let rejected: CatalogRejected = serde_json::from_value(rejected).unwrap();
        let fields: Vec<&str> = rejected.findings.iter().map(|f| f.field.as_str()).collect();
        assert_eq!(fields, ["meta.name", "meta.version"]);
        assert!(
            rejected.error.starts_with("meta.name: "),
            "{}",
            rejected.error
        );
    }

    #[tokio::test]
    async fn a_provenance_sha_the_content_does_not_hash_to_is_a_400() {
        let wrong = json!({ "blob_sha": "00".repeat(20) });
        let body = assign("[meta]\nname = \"bench\"\n", wrong);
        let (status, rejected) = request(
            "PUT",
            "/v1/admin/assignments",
            Some("bootstrap"),
            Some(body),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(rejected["findings"][0]["field"], "provenance.blob_sha");
    }

    #[tokio::test]
    async fn a_catalogue_that_passes_is_taken_to_the_database() {
        let body = assign("[meta]\nname = \"bench\"\n", json!({}));
        let (status, _) = request(
            "PUT",
            "/v1/admin/assignments",
            Some("bootstrap"),
            Some(body),
        )
        .await;
        assert_eq!(status, 503, "PostgreSQL is not there");
    }

    #[tokio::test]
    async fn a_malformed_admin_request_is_a_client_error() {
        let key = Some("bootstrap");
        let (status, _) = request("GET", "/v1/admin/catalogs/xyz", key, None).await;
        assert_eq!(status, 400, "not a SHA");
        let no_interface = "/v1/admin/assignments?daemon_id=bench";
        assert_eq!(request("DELETE", no_interface, key, None).await.0, 400);
        let no_content = json!({ "daemon_id": "bench", "interface": "can0", "provenance": {} });
        let (status, _) = request("PUT", "/v1/admin/assignments", key, Some(no_content)).await;
        assert_eq!(status, 422);
    }

    #[tokio::test]
    async fn a_conflict_is_a_409_naming_what_is_assigned_now() {
        let now = "ab".repeat(20);
        let response = AssignError::Conflict(Some(now.clone())).into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let conflict: AssignmentConflict = serde_json::from_slice(&body).unwrap();
        assert_eq!(conflict.current, Some(now));
    }

    fn import_body(frames: &[(CanFrame, bool)]) -> Vec<u8> {
        let mut body = Vec::new();
        import::encode_header_into(&mut body);
        for (ts_us, (frame, transmitted)) in (0..).zip(frames) {
            import::encode_record_into(&mut body, ts_us, frame, *transmitted);
        }
        body
    }

    /// An import is stored as ingest stores the same frames: RTR, BRS and ESI
    /// in `flags`, and a remote frame's requested code as `dlc` with no data.
    #[test]
    fn an_import_stores_its_frames_as_ingest_does() {
        let mut fd = CanFrame::data(2, 0x18DA_F110, true, true, true, vec![0xAA; 12]);
        fd.esi = true;
        let frames = [
            (CanFrame::remote(0, 0x7F1, false, 8), false),
            (CanFrame::remote(1, 0x7F1, false, 2), true),
            (fd, false),
        ];
        let body = import_body(&frames);
        let mut imported = Vec::new();
        let records = &body[import::BODY_HEADER..];
        assert_eq!(import_rows(records, &mut imported), Ok(records.len()));

        let columns = |r: &FrameRow| (r.ts_us, r.id, r.flags, r.dlc, r.data.clone(), r.bus);
        let ingested: Vec<_> = (0..)
            .zip(&frames)
            .map(|(ts_us, (frame, transmitted))| {
                let (id_flags, flags) = RecordFields::from_can(frame, *transmitted).to_wire();
                let row = FrameRow::new(
                    ts_us,
                    RecordKind::Can,
                    id_flags,
                    flags,
                    frame.bus,
                    frame.data.clone(),
                );
                columns(&row)
            })
            .collect();
        assert_eq!(imported.iter().map(columns).collect::<Vec<_>>(), ingested);
    }

    /// The desktop shows the message; an older one sends a body without the
    /// header, which would otherwise be misread as records.
    #[tokio::test]
    async fn an_import_body_this_gateway_does_not_take_is_a_400_with_a_message() {
        let v2 = import_body(&[(
            CanFrame::data(0, 0x123, false, false, false, vec![1]),
            false,
        )]);
        let v1 = v2[import::BODY_HEADER..].to_vec();
        let v3 = [&b"WTIM\x03"[..], &v1].concat();
        for (body, says) in [
            (v1, "magic"),
            (v3, "version 3"),
            (b"WTI".to_vec(), "header"),
        ] {
            let (status, err) = send(
                "POST",
                "/v1/db/capture/import",
                Some("bootstrap"),
                "application/x-wiretap-frames",
                &body,
            )
            .await;
            assert_eq!(status, 400, "{says}: {err}");
            let msg = err["error"].as_str().unwrap_or_default();
            assert!(msg.contains(says), "{says}: {msg}");
        }
    }
}
