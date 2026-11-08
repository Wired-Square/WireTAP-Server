//! The gateway's API, for a UI bundled with a gateway: `/api/gateway/v1/…` is
//! passed to the gateway's `/v1/…` on its app socket, with the caller in
//! `Appliance-Identity`, which the gateway believes only from this process's
//! uid. The gateway decides what that caller may do.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{FromRef, Request, State};
use axum::http::header::{
    ACCEPT, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, HOST, RETRY_AFTER,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::net::UnixStream;
use wslib_appliance_server::{
    About, ApplianceState, Identity, Mount, ProductAbout, Uncompressed, Viewer,
};
use wslib_appliance_store::Store;
use wslib_appliance_types::Availability;
use wslib_appliance_types::about::Subsystem;
use wslib_appliance_types::console::{ConsoleIdentity, IDENTITY_HEADER};

const PASSED_ON: [HeaderName; 3] = [ACCEPT, CONTENT_LENGTH, CONTENT_TYPE];
const PASSED_BACK: [HeaderName; 5] = [
    CACHE_CONTROL,
    CONTENT_DISPOSITION,
    CONTENT_LENGTH,
    CONTENT_TYPE,
    RETRY_AFTER,
];
const HEALTH_WAIT: Duration = Duration::from_secs(5);
const ARCHIVE: &str = "application/vnd.wiretap.archive";

#[derive(Debug, Clone)]
pub struct Gateway {
    socket: Arc<PathBuf>,
}

#[derive(Clone, FromRef)]
struct Proxied {
    chassis: ApplianceState,
    gateway: Gateway,
}

pub fn routes(gateway: Gateway) -> Mount {
    let routes = Router::new().route("/gateway/v1/{*rest}", any(proxy));
    let about = gateway.clone();
    Mount::new(routes, move |chassis| Proxied { chassis, gateway }).about(about)
}

impl Gateway {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: Arc::new(socket.into()),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    async fn send(
        &self,
        request: hyper::Request<Body>,
    ) -> Result<hyper::Response<Incoming>, String> {
        let at = self.socket.display();
        let stream = UnixStream::connect(&*self.socket)
            .await
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => format!("there is no socket at {at}; it is not running"),
                io::ErrorKind::ConnectionRefused => {
                    format!("nothing is listening on {at}; it is not running")
                }
                _ => format!("{at} cannot be reached: {e}"),
            })?;
        let broken = |e: hyper::Error| format!("{at} broke off: {e}");
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(broken)?;
        tokio::spawn(connection);
        sender.send_request(request).await.map_err(broken)
    }

    async fn health(&self) -> Availability {
        let request = hyper::Request::get("/healthz")
            .header(HOST, "localhost")
            .body(Body::empty())
            .expect("a static request");
        match tokio::time::timeout(HEALTH_WAIT, self.send(request)).await {
            Ok(Ok(answer)) if answer.status() == StatusCode::OK => {
                Availability::ready(self.socket.display().to_string())
            }
            Ok(Ok(answer)) => Availability::unsupported(format!(
                "it answers {}: its database is not answering",
                answer.status()
            )),
            Ok(Err(down)) => Availability::unsupported(down),
            Err(_) => Availability::unsupported(format!(
                "no answer within {} s on {}",
                HEALTH_WAIT.as_secs(),
                self.socket.display()
            )),
        }
    }
}

impl About for Gateway {
    async fn about(&self, _: &Store) -> ProductAbout {
        ProductAbout::new(
            Vec::new(),
            vec![Subsystem::new("Gateway", self.health().await)],
        )
    }
}

/// An account as the console asserts one. The control socket is nobody's.
fn asserted(who: &Identity) -> Option<ConsoleIdentity> {
    let (id, username, role, reauthenticated_at) = match who {
        Identity::Resolved(account) => (
            &account.id,
            &account.username,
            account.role,
            account.reauthenticated_at,
        ),
        Identity::Session(user) => (&user.id, &user.username, user.role, None),
        _ => return None,
    };
    Some(ConsoleIdentity {
        id: id.clone(),
        username: username.clone(),
        role,
        reauthenticated_at,
    })
}

async fn proxy(Viewer(who): Viewer, State(gateway): State<Gateway>, request: Request) -> Response {
    let Some(caller) = asserted(&who) else {
        return refused(
            StatusCode::FORBIDDEN,
            "the gateway answers an account, which the control socket is not".into(),
        );
    };
    let (parts, body) = request.into_parts();
    let path = parts.uri.path_and_query().map_or("", |p| p.as_str());
    let mut forward = hyper::Request::new(body);
    *forward.method_mut() = parts.method;
    *forward.uri_mut() = path
        .strip_prefix("/gateway")
        .unwrap_or(path)
        .parse()
        .expect("a path and query this router matched");
    let headers = forward.headers_mut();
    copy(&parts.headers, headers, &PASSED_ON);
    headers.insert(HOST, HeaderValue::from_static("localhost"));
    headers.insert(
        IDENTITY_HEADER,
        HeaderValue::try_from(caller.to_string()).expect("an identity is visible ASCII"),
    );

    match gateway.send(forward).await {
        Ok(answer) => {
            let (parts, body) = answer.into_parts();
            let mut response = Response::new(Body::new(body));
            *response.status_mut() = parts.status;
            copy(&parts.headers, response.headers_mut(), &PASSED_BACK);
            if parts
                .headers
                .get(CONTENT_TYPE)
                .is_some_and(|t| t == ARCHIVE)
            {
                response.extensions_mut().insert(Uncompressed);
            }
            response
        }
        Err(down) => refused(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("the gateway is not answering: {down}"),
        ),
    }
}

fn copy(from: &HeaderMap, to: &mut HeaderMap, names: &[HeaderName]) {
    for name in names {
        if let Some(value) = from.get(name) {
            to.insert(name, value.clone());
        }
    }
}

fn refused(status: StatusCode, error: String) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}
