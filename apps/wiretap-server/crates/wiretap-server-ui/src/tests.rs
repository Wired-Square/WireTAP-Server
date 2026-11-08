//! The routes against a fake daemon and a fake gateway, as each backend
//! serves them.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::UnixListener;
use wslib_appliance_server::testing::{Harness, body_json, console};
use wslib_appliance_types::Role;

use crate::daemon::Daemon;
use crate::gateway::Gateway;
use crate::routes::routes;
use crate::{api, install_crypto_provider};

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
        let dir = TempDir::with_prefix("wtui").unwrap();
        let script = Shared::default();
        script.lock().unwrap().settings = json!({ "settings": [], "warnings": [] });
        let router = Router::new()
            .route("/v1/status", get(fake_status))
            .route("/v1/settings", get(fake_settings).put(fake_put))
            .with_state(Arc::clone(&script));
        let listener = UnixListener::bind(dir.path().join("control.sock")).unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { dir, script }
    }

    fn daemon(&self) -> Daemon {
        Daemon::new(self.dir.path().join("control.sock"))
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
    install_crypto_provider();
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
    let dir = TempDir::with_prefix("wtui-down").unwrap();
    let stale = dir.path().join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());

    for (socket, said) in [
        (dir.path().join("absent.sock"), "there is no control socket"),
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
async fn about_lists_the_capture_daemon_and_whether_it_answers() {
    let fake = Fake::start().await;
    let absent = fake.dir.path().join("absent.sock");
    for (daemon, state, said) in [
        (
            fake.daemon(),
            "ready",
            format!("running, on {}", fake.daemon().socket().display()),
        ),
        (
            Daemon::new(absent),
            "unsupported",
            "there is no control socket".to_string(),
        ),
    ] {
        let app = behind_a_console(daemon).await;
        let about = app
            .get("/api/about")
            .from_console(&console("a", Role::Admin));
        let subsystems = body_json(about.send().await).await["subsystems"].clone();
        assert_eq!(subsystems[0]["name"], "Capture daemon", "{subsystems}");
        assert_eq!(
            subsystems[0]["availability"]["state"], state,
            "{subsystems}"
        );
        assert!(subsystems.to_string().contains(&said), "{subsystems}");
    }
}

#[tokio::test]
async fn served_self_an_account_meets_the_same_ladder() {
    use wslib_appliance_https::testing::Harness;

    let fake = Fake::start().await;
    install_crypto_provider();
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
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use wslib_appliance_https::testing::{Harness, body_json};

    let browser_time = || {
        let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        since.as_millis() as u64
    };
    let fake = Fake::start().await;
    install_crypto_provider();
    let app = Harness::builder().api(routes(fake.daemon())).build().await;
    let operator = app.signed_in("operator", Role::Operator).await;
    let enrol = app.post("/api/session/totp").cookie(&operator);
    let enrolment = enrol.json(json!({ "password": Harness::PASSWORD }));
    let secret = body_json(enrolment.send().await).await["secret"]
        .as_str()
        .unwrap()
        .to_string();
    let key = wslib_auth_totp::Key::from_base32(&secret).expect("a base32 secret");
    let code = key.code_at(SystemTime::now());
    let confirm = app.post("/api/session/totp/confirm").cookie(&operator);
    let confirm = confirm.json(json!({ "code": code, "browser_time": browser_time() }));
    assert_eq!(confirm.send().await.status(), StatusCode::OK);

    let password = json!({ "username": "operator", "password": Harness::PASSWORD });
    let waiting = app.post("/api/session").json(password).send().await;
    assert_eq!(waiting.status(), StatusCode::ACCEPTED);
    assert!(Harness::session_from(&waiting).is_none());
    let pending = body_json(waiting).await["pending"].clone();
    let code = key.code_at(SystemTime::now() + Duration::from_secs(30));
    let step = json!({ "pending": pending, "code": code, "browser_time": browser_time() });
    let signed_in = app.post("/api/session/code").json(step).send().await;
    assert_eq!(signed_in.status(), StatusCode::OK);
    let session = Harness::session_from(&signed_in).expect("a session");
    let read = app.get("/api/capture/status").cookie(&session);
    assert_eq!(read.send().await.status(), StatusCode::OK);
}

/// A gateway that answers every `/v1/…` with what reached it, and `/healthz`
/// with `health`.
struct FakeGateway {
    dir: TempDir,
    heard: Arc<Mutex<u32>>,
    health: Arc<Mutex<StatusCode>>,
}

impl FakeGateway {
    async fn start() -> Self {
        let dir = TempDir::with_prefix("wtgw").unwrap();
        let heard = Arc::new(Mutex::new(0));
        let health = Arc::new(Mutex::new(StatusCode::OK));
        let status = Arc::clone(&health);
        let counted = Arc::clone(&heard);
        let router = Router::new()
            .route(
                "/healthz",
                get(move || async move { *status.lock().unwrap() }),
            )
            .route(
                "/v1/export",
                get(|| async {
                    let archive = "WTAR, gzipped already".repeat(8);
                    (
                        [("content-type", "application/vnd.wiretap.archive")],
                        archive,
                    )
                }),
            )
            .fallback(move |request: axum::extract::Request| async move {
                *counted.lock().unwrap() += 1;
                fake_gateway_echo(request).await
            });
        let listener = UnixListener::bind(dir.path().join("gateway.sock")).unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { dir, heard, health }
    }

    fn gateway(&self) -> Gateway {
        Gateway::new(self.dir.path().join("gateway.sock"))
    }

    fn heard(&self) -> u32 {
        *self.heard.lock().unwrap()
    }
}

async fn fake_gateway_echo(request: axum::extract::Request) -> axum::response::Response {
    use axum::response::IntoResponse;

    let (parts, body) = request.into_parts();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let body = axum::body::to_bytes(body, 1 << 20).await.unwrap();
    let status = match parts.uri.path() {
        "/v1/refused" => StatusCode::FORBIDDEN,
        _ => StatusCode::OK,
    };
    let heard = json!({
        "method": parts.method.as_str(),
        "uri": parts.uri.to_string(),
        "identity": header("appliance-identity"),
        "authorization": header("authorization"),
        "cookie": header("cookie"),
        "content_type": header("content-type"),
        "body": String::from_utf8(body.to_vec()).unwrap(),
    });
    (
        status,
        [
            ("content-disposition", "attachment; filename=\"a.wtar\""),
            ("set-cookie", "appliance_session=forged"),
            ("x-gateway", "internal"),
        ],
        Json(heard),
    )
        .into_response()
}

async fn with_gateway(gateway: Option<Gateway>) -> Harness {
    let fake = Fake::start().await;
    install_crypto_provider();
    api(fake.daemon(), gateway)
        .into_iter()
        .fold(Harness::builder(), |harness, mount| harness.api(mount))
        .build()
        .await
}

#[tokio::test]
async fn the_gateway_hears_the_signed_in_caller_and_nothing_of_the_browsers() {
    use std::time::{SystemTime, UNIX_EPOCH};
    use wslib_appliance_types::console::ConsoleIdentity;

    let fake = FakeGateway::start().await;
    let app = with_gateway(Some(fake.gateway())).await;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let admin: ConsoleIdentity = format!(
        "v1; id=z; user=Zo%C3%AB; role=admin; reauth={}",
        now.as_secs()
    )
    .parse()
    .unwrap();
    let post = app
        .post("/api/gateway/v1/admin/keys?database=site")
        .from_console(&admin)
        .header("authorization", "Bearer from-the-browser")
        .header("cookie", "appliance_session=abc")
        .body("application/json", br#"{"name":"bench"}"#.to_vec())
        .send()
        .await;
    assert_eq!(post.status(), StatusCode::OK);
    let headers = post.headers().clone();
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"a.wtar\""
    );
    assert!(headers.get("set-cookie").is_none());
    assert!(headers.get("x-gateway").is_none());

    let heard = body_json(post).await;
    assert_eq!(heard["method"], "POST");
    assert_eq!(heard["uri"], "/v1/admin/keys?database=site");
    assert_eq!(heard["body"], r#"{"name":"bench"}"#);
    assert_eq!(heard["content_type"], "application/json");
    assert_eq!(heard["authorization"], Value::Null);
    assert_eq!(heard["cookie"], Value::Null);
    let identity: ConsoleIdentity = heard["identity"].as_str().unwrap().parse().unwrap();
    assert_eq!(identity.username, "Zoë");
    assert_eq!(identity.role, Role::Admin);
    assert_eq!(identity.reauthenticated_at, admin.reauthenticated_at);
    assert!(admin.reauthenticated_at.is_some());

    let read = app
        .get("/api/gateway/v1/whoami")
        .from_console(&console("v", Role::Viewer));
    let heard = body_json(read.send().await).await;
    let identity: ConsoleIdentity = heard["identity"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        (identity.username.as_str(), identity.role),
        ("v", Role::Viewer)
    );
    assert_eq!(identity.reauthenticated_at, None);
}

#[tokio::test]
async fn only_an_account_reaches_the_gateway() {
    let fake = FakeGateway::start().await;
    let app = with_gateway(Some(fake.gateway())).await;
    let path = "/api/gateway/v1/whoami";

    assert_eq!(
        app.get(path).send().await.status(),
        StatusCode::UNAUTHORIZED
    );
    let local = app.get(path).local().send().await;
    assert_eq!(local.status(), StatusCode::FORBIDDEN);
    assert_eq!(fake.heard(), 0);
}

#[tokio::test]
async fn the_gateways_refusals_and_only_its_api_come_through() {
    let fake = FakeGateway::start().await;
    let app = with_gateway(Some(fake.gateway())).await;
    let viewer = console("v", Role::Viewer);

    let refused = app.delete("/api/gateway/v1/refused").from_console(&viewer);
    let refused = refused.send().await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(refused).await["method"], "DELETE");

    for outside in ["/api/gateway/healthz", "/api/gateway/admin/"] {
        let outside = app.get(outside).from_console(&viewer).send().await;
        assert_eq!(outside.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(fake.heard(), 1);
}

#[tokio::test]
async fn an_archive_comes_through_plain_and_the_gateways_json_may_be_gzipped() {
    let fake = FakeGateway::start().await;
    let app = with_gateway(Some(fake.gateway())).await;
    let viewer = console("v", Role::Viewer);
    let accepting_gzip = async |path| {
        let request = app.get(path).from_console(&viewer);
        request.header("accept-encoding", "gzip").send().await
    };

    let archive = accepting_gzip("/api/gateway/v1/export").await;
    assert_eq!(archive.status(), StatusCode::OK);
    assert_eq!(
        archive.headers()["content-type"],
        "application/vnd.wiretap.archive"
    );
    assert!(archive.headers().get("content-encoding").is_none());

    let json = accepting_gzip("/api/gateway/v1/whoami").await;
    assert_eq!(json.headers()["content-encoding"], "gzip");
}

#[tokio::test]
async fn without_a_gateway_there_is_no_gateway_surface() {
    let app = with_gateway(None).await;
    let admin = console("a", Role::Admin);

    let whoami = app.get("/api/gateway/v1/whoami").from_console(&admin);
    assert_eq!(whoami.send().await.status(), StatusCode::NOT_FOUND);
    let about = app.get("/api/about").from_console(&admin).send().await;
    let subsystems = body_json(about).await["subsystems"].to_string();
    assert!(!subsystems.contains("Gateway"), "{subsystems}");
}

#[tokio::test]
async fn about_lists_the_gateway_and_whether_its_database_answers() {
    let fake = FakeGateway::start().await;
    let absent = Gateway::new(fake.dir.path().join("absent.sock"));
    let admin = console("a", Role::Admin);

    for (gateway, health, state, said) in [
        (
            fake.gateway(),
            StatusCode::OK,
            "ready",
            format!("running, on {}", fake.gateway().socket().display()),
        ),
        (
            fake.gateway(),
            StatusCode::SERVICE_UNAVAILABLE,
            "unsupported",
            "its database is not answering".to_string(),
        ),
        (
            absent,
            StatusCode::OK,
            "unsupported",
            "there is no socket".to_string(),
        ),
    ] {
        *fake.health.lock().unwrap() = health;
        let app = with_gateway(Some(gateway)).await;
        let about = app.get("/api/about").from_console(&admin).send().await;
        let subsystems = body_json(about).await["subsystems"].clone();
        let gateway = &subsystems[1];
        assert_eq!(gateway["name"], "Gateway", "{subsystems}");
        assert_eq!(gateway["availability"]["state"], state, "{subsystems}");
        assert!(gateway.to_string().contains(&said), "{subsystems}");
    }
}

#[tokio::test]
async fn a_gateway_that_is_not_running_is_a_503_saying_so() {
    let dir = TempDir::with_prefix("wtgw-down").unwrap();
    let stale = dir.path().join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());

    for (socket, said) in [
        (dir.path().join("absent.sock"), "there is no socket"),
        (stale, "nothing is listening"),
    ] {
        let app = with_gateway(Some(Gateway::new(socket))).await;
        let read = app
            .get("/api/gateway/v1/databases")
            .from_console(&console("v", Role::Viewer));
        let read = read.send().await;
        assert_eq!(read.status(), StatusCode::SERVICE_UNAVAILABLE);
        let error = body_json(read).await["error"].as_str().unwrap().to_string();
        assert!(error.starts_with("the gateway is not answering"), "{error}");
        assert!(error.contains(said), "{error}");
    }
}

#[tokio::test]
async fn served_self_the_gateway_hears_the_account_not_a_forged_header() {
    use wslib_appliance_https::testing::{Harness, body_json};
    use wslib_appliance_types::console::ConsoleIdentity;

    let fake = FakeGateway::start().await;
    let daemon = Fake::start().await;
    install_crypto_provider();
    let app = api(daemon.daemon(), Some(fake.gateway()))
        .into_iter()
        .fold(Harness::builder(), |harness, mount| harness.api(mount))
        .build()
        .await;
    let operator = app.signed_in("operator", Role::Operator).await;
    let read = app
        .get("/api/gateway/v1/whoami")
        .cookie(&operator)
        .header("appliance-identity", "v1; id=x; user=root; role=admin")
        .send()
        .await;
    let heard = body_json(read).await;
    let identity: ConsoleIdentity = heard["identity"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        (identity.username.as_str(), identity.role),
        ("operator", Role::Operator)
    );
}
