//! The Capture screen's routes, under `/api/capture`: the daemon's own
//! answers, passed through, behind the chassis's session.
//!
//! A reading is a capability union, as the kit's `Capability` takes: the
//! daemon's body with `state: "ready"`, or `state: "unsupported"` and why the
//! daemon is not answering. A refusal from the daemon keeps its status and
//! body, so the screen shows the daemon's own sentence and `where`.

use appliance_server::{Admin, ApplianceState, Mount, Viewer, audit};
use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use hyper::Method;
use serde_json::{Map, Value, json};

use crate::daemon::{Daemon, Down};

#[derive(Clone, FromRef)]
struct Capture {
    chassis: ApplianceState,
    daemon: Daemon,
}

pub fn routes(daemon: Daemon) -> Mount {
    let routes = Router::new()
        .route("/capture/status", get(status))
        .route("/capture/settings", get(settings).put(change));
    Mount::new(routes, move |chassis| Capture { chassis, daemon })
}

async fn status(Viewer(_): Viewer, State(daemon): State<Daemon>) -> Response {
    reading(daemon.get("/v1/status").await)
}

async fn settings(Viewer(_): Viewer, State(daemon): State<Daemon>) -> Response {
    reading(daemon.get("/v1/settings").await)
}

/// Pass a change to the daemon and, once it has restarted on it, answer the
/// settings it now runs on.
async fn change(
    Admin(who): Admin,
    State(app): State<ApplianceState>,
    State(daemon): State<Daemon>,
    Json(put): Json<Map<String, Value>>,
) -> Response {
    let keys = put.keys().cloned().collect::<Vec<_>>().join(", ");
    let body = Value::Object(put).to_string().into();
    match daemon.request(Method::PUT, "/v1/settings", body).await {
        Err(down) => refused(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("the capture daemon is not answering: {down}"),
        ),
        Ok((StatusCode::ACCEPTED, _)) => {
            audit!(&app, &who, "changed the capture daemon's settings", "keys" => &keys).await;
            match daemon.restarted().await {
                Ok(()) => reading(daemon.get("/v1/settings").await),
                Err(late) => refused(StatusCode::GATEWAY_TIMEOUT, late),
            }
        }
        Ok(refusal) => passed_through(refusal),
    }
}

fn reading(answer: Result<(StatusCode, Value), Down>) -> Response {
    match answer {
        Ok((StatusCode::OK, Value::Object(mut body))) => {
            body.insert("state".into(), "ready".into());
            Json(body).into_response()
        }
        Ok(refusal) => passed_through(refusal),
        Err(because) => Json(json!({ "state": "unsupported", "because": because })).into_response(),
    }
}

fn passed_through((status, body): (StatusCode, Value)) -> Response {
    (status, Json(body)).into_response()
}

fn refused(status: StatusCode, error: Down) -> Response {
    passed_through((status, json!({ "error": error })))
}
