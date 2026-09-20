//! HTTP API: query surface (ports of the desktop dbquery commands), admin
//! (keys, databases, ingest sessions, activity), capture import, health.
//! Auth is `Authorization: Bearer <api-key>`; roles read|ingest|admin.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, MethodRouter};
use axum::{Extension, Json, Router};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db;
use crate::events;
use crate::ingest::proto;
use crate::ingest::writer::FrameRow;
use crate::keys::{KeyInfo, Role};
use crate::running;
use crate::schema;
use crate::sql;
use crate::state::AppState;
use crate::types::ImportResult;

type St = Arc<AppState>;

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<String> for ApiError {
    fn from(msg: String) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg)
    }
}

fn forbidden(msg: &str) -> ApiError {
    ApiError(StatusCode::FORBIDDEN, msg.to_string())
}

fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.into())
}

pub fn router(state: St) -> Router {
    let authed = Router::new()
        // databases
        .route(
            "/v1/databases",
            polled(get(list_databases)).merge(post(create_database)),
        )
        .route("/v1/databases/{db}", delete(delete_database))
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
async fn health(State(state): State<St>) -> Json<serde_json::Value> {
    let db_ok = state.dbs.connect_raw("postgres").await.is_ok();
    Json(json!({
        "status": if db_ok { "ok" } else { "degraded" },
        "version": crate::VERSION,
        "db_ok": db_ok,
        "schema": schema_consensus(&state.dbs.schema_states().await),
    }))
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
    // migrated still refuses reads and writes, and folding it in would report a
    // tidy consensus while nothing worked.
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
    // Pooled, not `connect_raw`: both admin pages poll this every 2 s while
    // anything is busy, and a fresh connect per database per poll is a backend
    // process and a SCRAM handshake each time. Callers only reach here for a
    // database already known Current, so the pool's own gate passes through.
    let pool = dbs.pool(name).await.ok()?;
    let client = pool.get().await.ok()?;
    match schema::rollup_status(&client).await.ok()? {
        schema::RollupState::Empty => Some(("empty", None)),
        schema::RollupState::Incomplete => Some(("incomplete", None)),
        schema::RollupState::Covered { lag_secs } => Some(("covered", Some(lag_secs))),
    }
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

async fn list_databases(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
) -> Result<Json<serde_json::Value>, ApiError> {
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
    let mut databases: Vec<serde_json::Value> = Vec::new();
    for r in &rows {
        let name: String = r.get("datname");
        // Same filter the sweep uses: a name this gateway would never manage is
        // not a capture database, and listing it as "unknown" would report the
        // deployment as mixed forever.
        if !db::valid_db_name(&name) {
            continue;
        }
        if key.database_pin.as_ref().is_some_and(|pin| *pin != name) {
            continue;
        }
        let schema = states.get(&name);
        // Only worth asking a database that is actually usable; one mid-migration
        // would refuse the connection anyway.
        let rollup = match schema {
            Some(db::DbSchemaState::Current { .. }) => rollup_state(&state.dbs, &name).await,
            _ => None,
        };
        databases.push(json!({
            "name": name,
            "size_bytes": r.get::<_, i64>("size_bytes"),
            "schema_state": schema.map_or("unknown", |s| s.label()),
            "schema_version": schema.and_then(|s| s.version()),
            "busy_secs": schema.and_then(|s| s.busy_secs()),
            "schema_error": match schema {
                Some(db::DbSchemaState::Failed { error, .. }) => Some(error.clone()),
                _ => None,
            },
            "rollup_state": rollup.map(|(st, _)| st),
            "rollup_lag_secs": rollup.and_then(|(_, lag)| lag),
            "rollup_busy_secs": rebuilding.get(&name),
        }));
    }
    // The version they are all headed for, so the UI can render "v0 → v1"
    // without hardcoding what current means.
    Ok(Json(json!({
        "databases": databases,
        "schema_version": crate::schema::SCHEMA_VERSION,
    })))
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

/// `protocol` absent means [`sql::DEFAULT_PROTOCOL`] on every read below, so a
/// desktop that predates the parameter keeps seeing exactly what it did.
#[derive(Deserialize)]
struct ProtocolQuery {
    protocol: Option<sql::Protocol>,
}

#[derive(Deserialize)]
struct TimeRangeQuery {
    start: Option<String>,
    end: Option<String>,
    protocol: Option<sql::Protocol>,
}

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
) -> Result<Json<serde_json::Value>, ApiError> {
    check_read(&key, &db)?;
    let client = client_for(&state, &db).await?;
    let entries = sql::inventory(&client, range.start, range.end, range.protocol).await?;
    Ok(Json(json!({ "entries": entries })))
}

#[derive(Deserialize)]
struct FramesQuery {
    start: Option<String>,
    end: Option<String>,
    after: Option<String>,
    limit: Option<u32>,
    protocol: Option<sql::Protocol>,
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
    Json(p): Json<sql::PayloadsParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_read(&key, &db)?;
    let client = client_for(&state, &db).await?;
    let payloads = sql::payloads(&client, &p).await?;
    Ok(Json(json!({ "payloads": payloads })))
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------
// Gated by `check_read`, writes included: annotating an archive is a user's
// act, users hold read keys, and the alternative is handing every desktop an
// admin key. A pinned read key annotates only its own database, as it reads
// only that.

#[derive(Deserialize)]
struct EventsQuery {
    start: Option<String>,
    end: Option<String>,
    limit: Option<u32>,
}

async fn events_list(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_read(&key, &db)?;
    let client = client_for(&state, &db).await?;
    // Uncapped, unlike `frames`: there is no cursor here, so a cap would
    // override the client's limit in silence.
    let events = events::list(&client, q.start, q.end, q.limit.unwrap_or(1000)).await?;
    Ok(Json(json!({ "events": events })))
}

async fn events_create(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path(db): Path<String>,
    Json(new): Json<events::NewEvent>,
) -> Result<(StatusCode, Json<events::Event>), ApiError> {
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
    Json(patch): Json<events::EventPatch>,
) -> Result<Json<events::Event>, ApiError> {
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
    sql::ByteChangesParams,
    crate::types::ByteChangeQueryResult,
    sql::byte_changes
);
query_handler!(
    q_frame_changes,
    sql::FrameChangesParams,
    crate::types::FrameChangeQueryResult,
    sql::frame_changes
);
query_handler!(
    q_mirror_validation,
    sql::MirrorValidationParams,
    crate::types::MirrorValidationQueryResult,
    sql::mirror_validation
);
query_handler!(
    q_mux_statistics,
    sql::MuxStatisticsParams,
    crate::types::MuxStatisticsQueryResult,
    sql::mux_statistics
);
query_handler!(
    q_first_last,
    sql::FirstLastParams,
    crate::types::FirstLastQueryResult,
    sql::first_last
);
query_handler!(
    q_frequency,
    sql::FrequencyParams,
    crate::types::FrequencyQueryResult,
    sql::frequency
);
query_handler!(
    q_distribution,
    sql::DistributionParams,
    crate::types::DistributionQueryResult,
    sql::distribution
);
query_handler!(
    q_gap_analysis,
    sql::GapAnalysisParams,
    crate::types::GapAnalysisQueryResult,
    sql::gap_analysis
);
query_handler!(
    q_pattern_search,
    sql::PatternSearchParams,
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
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin(&key)?;
    let client = client_for(&state, &db).await?;
    Ok(Json(
        json!({ "ok": sql::signal_backend(&client, pid, false).await? }),
    ))
}

async fn activity_terminate(
    State(state): State<St>,
    Extension(key): Extension<KeyInfo>,
    Path((db, pid)): Path<(String, i32)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin(&key)?;
    let client = client_for(&state, &db).await?;
    Ok(Json(
        json!({ "ok": sql::signal_backend(&client, pid, true).await? }),
    ))
}

// ---------------------------------------------------------------------------
// Capture import
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ImportQuery {
    #[serde(default)]
    create: bool,
}

const IMPORT_RECORD_HEADER: usize = 14; // ts_us u64, id_flags u32, bus u8, len u8
const IMPORT_CHUNK_ROWS: usize = 8192;

/// Streaming capture import: body is a sequence of flat binary records
/// `ts_us u64 LE, id_flags u32 LE, bus u8, len u8, payload` (id_flags packed
/// as in the TCP ingest protocol). COPYed in chunks as the body streams.
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
    let pool = state
        .dbs
        .ensure_database(&db, q.create)
        .await
        .map_err(not_found)?;

    let mut stream = req.into_body().into_data_stream();
    let mut pending: Vec<u8> = Vec::with_capacity(65536);
    let mut rows: Vec<FrameRow> = Vec::with_capacity(IMPORT_CHUNK_ROWS);
    let mut imported: u64 = 0;

    loop {
        let chunk = stream
            .try_next()
            .await
            .map_err(|e| ApiError::from(format!("body read failed: {e}")))?;
        let done = chunk.is_none();
        if let Some(bytes) = chunk {
            pending.extend_from_slice(&bytes);
        }

        // Drain complete records from the pending buffer
        let mut off = 0;
        while pending.len() >= off + IMPORT_RECORD_HEADER {
            let plen = pending[off + 13] as usize;
            let max = proto::RecordKind::Can.max_payload();
            if plen > max {
                return Err(ApiError::from(format!(
                    "record payload length {plen} > {max}"
                )));
            }
            if pending.len() < off + IMPORT_RECORD_HEADER + plen {
                break;
            }
            let ts_us = i64::from_le_bytes(pending[off..off + 8].try_into().unwrap());
            let id_flags = u32::from_le_bytes(pending[off + 8..off + 12].try_into().unwrap());
            let bus = pending[off + 12];
            let data = pending[off + 14..off + 14 + plen].to_vec();
            rows.push(FrameRow::can(ts_us, id_flags, bus, data));
            off += IMPORT_RECORD_HEADER + plen;
        }
        pending.drain(..off);

        if rows.len() >= IMPORT_CHUNK_ROWS || (done && !rows.is_empty()) {
            crate::ingest::writer::copy_rows(&pool, &rows)
                .await
                .map_err(ApiError::from)?;
            imported += rows.len() as u64;
            rows.clear();
        }
        if done {
            if !pending.is_empty() {
                return Err(ApiError::from(format!(
                    "truncated record: {} trailing bytes",
                    pending.len()
                )));
            }
            break;
        }
    }

    Ok(Json(ImportResult {
        imported,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    }))
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
                },
            ),
        ]);
        assert_eq!(schema_consensus(&s), "failed");
    }

    /// Never a database name: this endpoint is unauthenticated.
    #[test]
    fn the_consensus_never_leaks_a_database_name() {
        let s = states(&[(
            "sungrow_ben_wired",
            db::DbSchemaState::Failed {
                version: 0,
                error: "connection to sungrow_ben_wired refused".into(),
            },
        )]);
        assert!(!schema_consensus(&s).contains("sungrow"));
    }

    #[test]
    fn an_empty_cluster_is_unknown_not_a_version() {
        assert_eq!(schema_consensus(&HashMap::new()), "unknown");
    }
}
