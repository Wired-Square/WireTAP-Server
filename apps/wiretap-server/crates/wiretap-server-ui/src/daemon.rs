//! The capture daemon's control socket: HTTP/JSON on a Unix socket, and the
//! only way this UI reaches the daemon.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::UnixStream;

pub const DEFAULT_SOCKET: &str = "/run/wiretap-server/control.sock";

const ANSWER_WAIT: Duration = Duration::from_secs(10);
/// A change is applied by the daemon stopping as on `SIGTERM`, which flushes
/// its cache, and starting again.
const RESTART_WAIT: Duration = Duration::from_secs(30);
const RESTART_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct Daemon {
    socket: Arc<PathBuf>,
}

/// Why the daemon gave no answer, to follow "the capture daemon is not
/// answering:".
pub type Down = String;

impl Daemon {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: Arc::new(socket.into()),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub async fn get(&self, path: &str) -> Result<(StatusCode, Value), Down> {
        self.request(Method::GET, path, Bytes::new()).await
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
    ) -> Result<(StatusCode, Value), Down> {
        tokio::time::timeout(ANSWER_WAIT, self.exchange(method, path, body))
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "no answer within {} s on {}",
                    ANSWER_WAIT.as_secs(),
                    self.socket.display()
                ))
            })
    }

    async fn exchange(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
    ) -> Result<(StatusCode, Value), Down> {
        let stream = UnixStream::connect(&*self.socket)
            .await
            .map_err(|e| self.unreachable(&e))?;
        let broken =
            |e: &dyn std::fmt::Display| format!("{} broke off: {e}", self.socket.display());
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| broken(&e))?;
        tokio::spawn(connection);
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, "localhost")
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(body))
            .expect("a method, a path and two static headers");
        let response = sender.send_request(request).await.map_err(|e| broken(&e))?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| broken(&e))?
            .to_bytes();
        let body = serde_json::from_slice(&body).map_err(|e| broken(&e))?;
        Ok((status, body))
    }

    fn unreachable(&self, e: &io::Error) -> Down {
        let at = self.socket.display();
        match e.kind() {
            io::ErrorKind::NotFound => {
                format!("there is no control socket at {at}; it is not running")
            }
            io::ErrorKind::ConnectionRefused => {
                format!("nothing is listening on {at}; it is not running")
            }
            io::ErrorKind::PermissionDenied => {
                format!("this UI may not open {at}: run it in the daemon's group")
            }
            _ => format!("{at} cannot be reached: {e}"),
        }
    }

    /// Wait out a change the daemon accepted: the process that took it
    /// answers `applying` until it executes itself again, so the first
    /// answer saying otherwise is the daemon running on the change.
    pub async fn restarted(&self) -> Result<(), Down> {
        let back = async {
            loop {
                if let Ok((StatusCode::OK, status)) = self.get("/v1/status").await
                    && status["applying"] == false
                {
                    return;
                }
                tokio::time::sleep(RESTART_POLL).await;
            }
        };
        tokio::time::timeout(RESTART_WAIT, back).await.map_err(|_| {
            format!(
                "the capture daemon took the change but has not come back within {} s; \
                 it may still be restarting",
                RESTART_WAIT.as_secs()
            )
        })
    }
}
