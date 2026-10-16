//! The routes against a fake control socket, as each backend serves them.

use std::sync::{Arc, Mutex};

use appliance_server::testing::{Harness, TempDir, body_json, console};
use appliance_types::Role;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::UnixListener;

use crate::daemon::Daemon;
use crate::routes::routes;

/// What the fake daemon answers. An accepted change is taken up only after
/// `applying` more status reads, as a restart would leave it.
#[derive(Default)]
struct Script {
    settings: Value,
    next: Option<Value>,
    applying: u32,
    status_reads: u32,
    refusal: Option<(StatusCode, Value)>,
}

type Shared = Arc<Mutex<Script>>;

struct Fake {
    dir: TempDir,
    script: Shared,
}

impl Fake {
    async fn start() -> Self {
        let dir = TempDir::new("wtui");
        let script = Shared::default();
        script.lock().unwrap().settings = json!({ "settings": [], "warnings": [] });
        let router = Router::new()
            .route("/v1/status", get(fake_status))
            .route("/v1/settings", get(fake_settings).put(fake_put))
            .with_state(Arc::clone(&script));
        let listener = UnixListener::bind(dir.join("control.sock")).unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { dir, script }
    }

    fn daemon(&self) -> Daemon {
        Daemon::new(self.dir.join("control.sock"))
    }

    fn refuse(&self, status: StatusCode, body: Value) {
        self.script.lock().unwrap().refusal = Some((status, body));
    }
}

async fn fake_status(State(script): State<Shared>) -> Json<Value> {
    let mut s = script.lock().unwrap();
    s.status_reads += 1;
    if s.next.is_some() && s.applying > 0 {
        s.applying -= 1;
        return Json(json!({ "applying": true }));
    }
    if let Some(next) = s.next.take() {
        s.settings = next;
    }
    Json(json!({ "applying": false, "devices": [] }))
}

async fn fake_settings(State(script): State<Shared>) -> Json<Value> {
    Json(script.lock().unwrap().settings.clone())
}

async fn fake_put(
    State(script): State<Shared>,
    Json(put): Json<serde_json::Map<String, Value>>,
) -> (StatusCode, Json<Value>) {
    let mut s = script.lock().unwrap();
    if let Some((status, body)) = s.refusal.take() {
        return (status, Json(body));
    }
    let settings: Vec<Value> = put
        .into_iter()
        .map(|(key, value)| json!({ "key": key, "value": value, "source": "ui", "pinned": false }))
        .collect();
    s.next = Some(json!({ "settings": settings, "warnings": [] }));
    s.applying = 3;
    (StatusCode::ACCEPTED, Json(json!({ "restarting": true })))
}

async fn behind_a_console(daemon: Daemon) -> Harness {
    Harness::builder().api(routes(daemon)).build().await
}

const CHANGE: &str = "/api/capture/settings";

#[tokio::test]
async fn anyone_signed_in_reads_and_only_an_admin_changes() {
    let fake = Fake::start().await;
    let app = behind_a_console(fake.daemon()).await;
    let change = json!({ "forward.database": "site" });

    for path in ["/api/capture/status", CHANGE] {
        let read = app.get(path).from_console(&console("v", Role::Viewer));
        assert_eq!(read.send().await.status(), StatusCode::OK, "{path}");
        assert_eq!(
            app.get(path).send().await.status(),
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
    for role in [Role::Viewer, Role::Operator] {
        let put = app.put(CHANGE).from_console(&console("x", role));
        let put = put.json(change.clone()).send().await;
        assert_eq!(put.status(), StatusCode::FORBIDDEN, "{role}");
    }
    assert!(
        fake.script.lock().unwrap().next.is_none(),
        "nothing reached the daemon"
    );

    for role in [Role::Admin, Role::HostAdmin] {
        let put = app.put(CHANGE).from_console(&console("x", role));
        let put = put.json(change.clone()).send().await;
        assert_eq!(put.status(), StatusCode::OK, "{role}");
    }
    let row = app.audit_row("changed the capture daemon's settings").await;
    assert_eq!(
        row.fields,
        [("keys".to_string(), "forward.database".to_string())]
    );
}

#[tokio::test]
async fn a_change_is_answered_once_the_daemon_runs_on_it() {
    let fake = Fake::start().await;
    let app = behind_a_console(fake.daemon()).await;
    let put = app.put(CHANGE).from_console(&console("a", Role::Admin));
    let put = put.json(json!({ "forward.database": "site" })).send().await;
    assert_eq!(put.status(), StatusCode::OK);

    let body = body_json(put).await;
    assert_eq!(body["state"], "ready");
    assert_eq!(body["settings"][0]["value"], "site", "{body}");
    assert_eq!(body["settings"][0]["source"], "ui");
    let s = fake.script.lock().unwrap();
    assert_eq!(
        s.status_reads, 4,
        "three while applying, then the new process"
    );
}

#[tokio::test]
async fn the_daemons_refusals_reach_the_screen_as_they_were() {
    let fake = Fake::start().await;
    let app = behind_a_console(fake.daemon()).await;
    for (status, body) in [
        (
            StatusCode::CONFLICT,
            json!({
                "error": "forward.port is pinned by flag --forward-port",
                "key": "forward.port",
                "where": "flag --forward-port",
            }),
        ),
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": "forward.daemon_id is not a setting a UI may change", "key": "forward.daemon_id" }),
        ),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({ "error": "[ingest] needs [forward] enabled" }),
        ),
    ] {
        fake.refuse(status, body.clone());
        let put = app.put(CHANGE).from_console(&console("a", Role::Admin));
        let put = put.json(json!({ "forward.port": 1 })).send().await;
        assert_eq!(put.status(), status);
        assert_eq!(body_json(put).await, body);
    }
}

#[tokio::test]
async fn a_daemon_that_is_not_running_is_a_state_not_an_error() {
    let dir = TempDir::new("wtui-down");
    let stale = dir.join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());

    for (socket, said) in [
        (dir.join("absent.sock"), "there is no control socket"),
        (stale, "nothing is listening"),
    ] {
        let app = behind_a_console(Daemon::new(socket)).await;
        for path in ["/api/capture/status", CHANGE] {
            let read = app.get(path).from_console(&console("v", Role::Viewer));
            let read = read.send().await;
            assert_eq!(read.status(), StatusCode::OK);
            let body = body_json(read).await;
            assert_eq!(body["state"], "unsupported");
            assert!(body["because"].as_str().unwrap().contains(said), "{body}");
        }
        let put = app.put(CHANGE).from_console(&console("a", Role::Admin));
        let put = put.json(json!({ "logging.level": "DEBUG" })).send().await;
        assert_eq!(put.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body_json(put).await["error"]
                .as_str()
                .unwrap()
                .contains(said)
        );
    }
}

#[tokio::test]
async fn served_self_an_account_meets_the_same_ladder() {
    use appliance_https::testing::Harness;

    let fake = Fake::start().await;
    let app = Harness::builder()
        .users_routes()
        .api(routes(fake.daemon()))
        .build()
        .await;
    let operator = app.signed_in("operator", Role::Operator).await;
    let admin = app.admin("admin").await;
    let change = json!({ "logging.level": "DEBUG" });

    let read = app
        .get("/api/capture/status")
        .cookie(&operator)
        .send()
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    let put = app
        .put(CHANGE)
        .cookie(&operator)
        .json(change.clone())
        .send()
        .await;
    assert_eq!(put.status(), StatusCode::FORBIDDEN);
    let put = app.put(CHANGE).cookie(&admin).json(change).send().await;
    assert_eq!(put.status(), StatusCode::OK);
}

#[tokio::test]
async fn served_self_an_account_with_totp_reaches_capture_after_its_code() {
    use appliance_https::testing::{Harness, body_json, totp_code};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let browser_time = || {
        let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        since.as_millis() as u64
    };
    let fake = Fake::start().await;
    let app = Harness::builder().api(routes(fake.daemon())).build().await;
    let operator = app.signed_in("operator", Role::Operator).await;
    let enrol = app.post("/api/session/totp").cookie(&operator);
    let enrolment = enrol.json(json!({ "password": Harness::PASSWORD }));
    let secret = body_json(enrolment.send().await).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let code = totp_code(&secret, SystemTime::now());
    let confirm = app.post("/api/session/totp/confirm").cookie(&operator);
    let confirm = confirm.json(json!({ "code": code, "browser_time": browser_time() }));
    assert_eq!(confirm.send().await.status(), StatusCode::OK);

    let password = json!({ "username": "operator", "password": Harness::PASSWORD });
    let waiting = app.post("/api/session").json(password).send().await;
    assert_eq!(waiting.status(), StatusCode::ACCEPTED);
    assert!(Harness::session_from(&waiting).is_none());
    let pending = body_json(waiting).await["pending"].clone();
    let code = totp_code(&secret, SystemTime::now() + Duration::from_secs(30));
    let step = json!({ "pending": pending, "code": code, "browser_time": browser_time() });
    let signed_in = app.post("/api/session/code").json(step).send().await;
    assert_eq!(signed_in.status(), StatusCode::OK);
    let session = Harness::session_from(&signed_in).expect("a session");
    let read = app.get("/api/capture/status").cookie(&session);
    assert_eq!(read.send().await.status(), StatusCode::OK);
}
