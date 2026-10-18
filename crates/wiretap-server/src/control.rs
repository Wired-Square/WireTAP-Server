//! The control socket: HTTP/JSON on a Unix socket, for a UI on the same host.
//!
//! **The socket's permissions are the trust boundary.** It is created 0660 and
//! takes the daemon's group, and nothing here authenticates further: whoever
//! can connect can read every setting but a secret, and change any a UI may.
//!
//! A change is applied by a restart. `PUT /v1/settings` validates the new
//! settings as a start would, writes `ui.toml`, answers 202, and the daemon
//! stops as on a signal and executes itself again on the same PID. Only if
//! that fails does it exit with [`EXIT_TO_APPLY`] for its supervisor.

use std::convert::Infallible;
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Map, Value};
use tokio::net::UnixListener;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::cli::Cli;
use crate::pipeline::{self, RunError};
use crate::settings::{self, Env, Forward, Settings};
use crate::status::Live;
use crate::ui::{self, Setting};

/// `EX_TEMPFAIL`: the exit that asks a supervisor to start the daemon again,
/// when it cannot execute itself.
pub const EXIT_TO_APPLY: u8 = 75;

const MAX_BODY: usize = 64 * 1024;

/// What the socket needs from the start that made it.
pub struct Control {
    pub socket: PathBuf,
    pub cli: Cli,
    pub ui_path: Option<PathBuf>,
    /// What `GET /v1/settings` answers until the next start.
    pub settings: Vec<Setting>,
    pub warnings: Vec<String>,
}

impl Control {
    /// `--control-socket`, else the runtime directory's. Empty is none.
    pub fn socket(cli: &Cli, env: &Env) -> Option<PathBuf> {
        match cli.control_socket.as_deref() {
            Some("") => None,
            Some(p) => Some(p.into()),
            None => env
                .runtime_dir
                .as_ref()
                .map(|d| Path::new(d).join("control.sock")),
        }
    }

    /// Listen, replacing a socket a previous run left; anything else at the
    /// path is not this daemon's to remove.
    pub fn bind(self) -> Result<Server, (String, io::Error)> {
        let path = &self.socket;
        let fail = |e| (path.display().to_string(), e);
        if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
            fs::remove_file(path).map_err(fail)?;
        }
        let listener = UnixListener::bind(path).map_err(fail)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o660)).map_err(fail)?;
        info!("Control socket on {}", path.display());
        Ok(Server {
            listener,
            control: self,
        })
    }
}

pub struct Server {
    listener: UnixListener,
    control: Control,
}

struct State {
    control: Control,
    settings: Settings,
    live: Arc<Live>,
    /// Set by an accepted `PUT`, so a second cannot race it to `ui.toml`.
    applying: AtomicBool,
}

impl Server {
    /// Answer until the runtime stops, notifying `applied` once a change has
    /// been written and the client has its answer.
    pub async fn serve(self, settings: Settings, live: Arc<Live>, applied: Arc<Notify>) {
        let state = Arc::new(State {
            control: self.control,
            settings,
            live,
            applying: AtomicBool::new(false),
        });
        loop {
            let stream = match self.listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    warn!("control socket accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let state = Arc::clone(&state);
            let applied = Arc::clone(&applied);
            tokio::spawn(async move {
                let accepted = Arc::new(AtomicBool::new(false));
                let service = {
                    let accepted = Arc::clone(&accepted);
                    service_fn(move |req| {
                        let (state, accepted) = (Arc::clone(&state), Arc::clone(&accepted));
                        async move { Ok::<_, Infallible>(state.handle(req, &accepted).await) }
                    })
                };
                if let Err(e) = http1::Builder::new()
                    .keep_alive(false)
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                {
                    debug!("control connection: {e}");
                }
                if accepted.load(Ordering::Relaxed) {
                    applied.notify_one();
                }
            });
        }
    }
}

type Answer = (StatusCode, Value);

fn refuse(status: StatusCode, error: impl std::fmt::Display) -> Answer {
    (status, json!({ "error": error.to_string() }))
}

impl State {
    async fn handle(&self, req: Request<Incoming>, accepted: &AtomicBool) -> Response<Full<Bytes>> {
        let (status, body) = match (req.method(), req.uri().path()) {
            (&Method::GET, "/v1/status") => (StatusCode::OK, self.status()),
            (&Method::GET, "/v1/settings") => (StatusCode::OK, self.settings()),
            (&Method::PUT, "/v1/settings") => {
                match Limited::new(req.into_body(), MAX_BODY).collect().await {
                    Ok(body) => self.put(&body.to_bytes(), accepted),
                    Err(e) => refuse(StatusCode::PAYLOAD_TOO_LARGE, e),
                }
            }
            (_, "/v1/status" | "/v1/settings") => {
                refuse(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
            }
            _ => refuse(StatusCode::NOT_FOUND, "not found"),
        };
        Response::builder()
            .status(status)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body.to_string())))
            .expect("a status and a static header")
    }

    fn status(&self) -> Value {
        let (s, live) = (&self.settings, &self.live);
        let clients = |c: &OnceLock<Arc<AtomicUsize>>| c.get().map(|c| c.load(Ordering::Relaxed));
        let links: Vec<Value> = live.archives.get().map_or(Vec::new(), |archives| {
            archives
                .iter()
                .map(|(db, c)| {
                    let last_ack = c.last_ack_ms.load(Ordering::Relaxed);
                    json!({
                        "database": Forward::label_of(db),
                        "connected": c.connected.load(Ordering::Relaxed),
                        "last_ack_unix_ms": (last_ack != 0).then_some(last_ack),
                        "cache_pending": c.cache_pending.load(Ordering::Relaxed),
                        "enqueued": c.enqueued.load(Ordering::Relaxed),
                        "written": c.written.load(Ordering::Relaxed),
                        "dropped": c.dropped.load(Ordering::Relaxed),
                    })
                })
                .collect()
        });
        json!({
            "version": crate::VERSION,
            "daemon_id": s.forward.as_ref().map(|f| &f.daemon_id),
            "uptime_secs": live.started.elapsed().as_secs(),
            "applying": self.applying.load(Ordering::Relaxed),
            "devices": s.devices.iter().zip(&live.devices).map(|(d, (_, status))| json!({
                "interface": d.interface,
                "kind": if d.is_can() { "can" } else { "serial" },
                "bus": d.bus.0,
                "mode": d.mode.as_str(),
                "state": status.state(),
                "frames": status.frames(),
            })).collect::<Vec<_>>(),
            "forward": {
                "enabled": s.forward.is_some(),
                "gateway": s.forward.as_ref().map(|f| format!("{}:{}", f.host, f.port)),
                "links": links,
            },
            "ingest": {
                "enabled": s.ingest.is_some(),
                "listen": s.ingest.as_ref().map(|i| format!("{}:{}", i.host, i.port)),
                "clients": clients(&live.ingest_clients),
            },
            "gvret": {
                "enabled": !s.can_devices().is_empty(),
                "listen": format!("{}:{}", s.host, s.port),
                "clients": clients(&live.gvret_clients),
            },
            "warnings": self.control.warnings,
        })
    }

    fn settings(&self) -> Value {
        let config = self.control.cli.config.as_deref();
        json!({
            "settings": self.control.settings.iter().map(|s| s.to_json(config)).collect::<Vec<_>>(),
            "warnings": self.control.warnings,
        })
    }

    fn put(&self, body: &[u8], accepted: &AtomicBool) -> Answer {
        if self.applying.swap(true, Ordering::Relaxed) {
            return refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "another change is being applied",
            );
        }
        let c = &self.control;
        match apply(
            &c.cli,
            c.ui_path.as_deref(),
            &Env::from_env(),
            &self.settings,
            body,
        ) {
            Ok(()) => {
                accepted.store(true, Ordering::Relaxed);
                (StatusCode::ACCEPTED, json!({ "restarting": true }))
            }
            Err(refused) => {
                self.applying.store(false, Ordering::Relaxed);
                refused
            }
        }
    }
}

/// Validate `body` as a start would and write it to `ui.toml`, or say why
/// not. `running` is what this process started on.
pub fn apply(
    cli: &Cli,
    ui_path: Option<&Path>,
    env: &Env,
    running: &Settings,
    body: &[u8],
) -> Result<(), Answer> {
    let Some(path) = ui_path else {
        return Err(refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "there is no ui.toml to write: set STATE_DIRECTORY or --ui-config",
        ));
    };
    let put: Map<String, Value> = serde_json::from_slice(body)
        .map_err(|e| refuse(StatusCode::BAD_REQUEST, format!("not a JSON object: {e}")))?;
    if put.is_empty() {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "the change names no setting",
        ));
    }
    let keys = ui::keys();
    if let Some(key) = put.keys().find(|k| !keys.contains(&k.as_str())) {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("{key} is not a setting a UI may change"), "key": key }),
        ));
    }

    let unstartable = |e: &dyn std::fmt::Display| refuse(StatusCode::UNPROCESSABLE_ENTITY, e);
    let file = match cli.config.as_deref().map(settings::load).transpose() {
        Ok(file) => file.flatten(),
        Err(e) => return Err(unstartable(&e)),
    };
    let next = ui::merge(&ui::load(path).unwrap_or_default(), &put)
        .map_err(|e| refuse(StatusCode::BAD_REQUEST, e))?;

    let sources = ui::sources(cli, file.as_ref(), &next, env);
    for key in put.keys() {
        if let Some((_, source)) = sources.iter().find(|(k, s)| k == key && s.pinned()) {
            let place = source.place(cli.config.as_deref());
            return Err((
                StatusCode::CONFLICT,
                json!({
                    "error": format!("{key} is pinned by {}", place.as_deref().unwrap_or("the config file")),
                    "key": key,
                    "where": place,
                }),
            ));
        }
    }
    if put.get("devices").is_some_and(Value::is_null) {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({
                "error": "devices cannot be cleared: the default, can0, transmits; set a device list instead",
                "key": "devices",
            }),
        ));
    }

    let resolved = settings::Settings::resolve(cli, file.as_ref(), Some(&next), env)
        .map_err(|e| unstartable(&e))?;
    let s = &resolved.settings;
    pipeline::check(s).map_err(|e| unstartable(&e))?;
    // The one listener whose address a UI can move, and the daemon holds the
    // old one: a port taken by something else would fail the start.
    if let Some(i) = &s.ingest {
        let unmoved = running
            .ingest
            .as_ref()
            .is_some_and(|r| (r.host.as_str(), r.port) == (i.host.as_str(), i.port));
        if !unmoved {
            std::net::TcpListener::bind((i.host.as_str(), i.port)).map_err(|err| {
                unstartable(&RunError::Bind {
                    addr: format!("{}:{}", i.host, i.port),
                    err,
                })
            })?;
        }
    }

    ui::write(path, &next).map_err(|e| {
        refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot write {}: {e}", path.display()),
        )
    })?;
    info!(
        "wrote the UI's settings to {}; restarting to apply them",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogues::tests::TempDir;
    use wslib_secret::Secret;

    /// A daemon started with `args` and, if given, a config file.
    struct Rig {
        dir: TempDir,
        cli: Cli,
        running: Settings,
    }

    fn env() -> Env {
        Env {
            hostname: Some("bench".into()),
            ..Env::default()
        }
    }

    fn rig(name: &str, args: &[&str], config: Option<&str>) -> Rig {
        let dir = TempDir::new(name);
        let path = dir.0.join("wiretap-server.toml");
        if let Some(text) = config {
            fs::write(&path, text).unwrap();
        }
        let mut argv = vec!["wiretap-server", "-C", path.to_str().unwrap()];
        argv.extend_from_slice(args);
        let cli = Cli::parse_args(argv);
        let file = settings::load(cli.config.as_deref().unwrap()).unwrap();
        let running = Settings::resolve(&cli, file.as_ref(), None, &env())
            .unwrap()
            .settings;
        Rig { dir, cli, running }
    }

    impl Rig {
        fn ui(&self) -> PathBuf {
            self.dir.0.join("ui.toml")
        }

        fn put_in(&self, env: &Env, body: Value) -> Result<(), Answer> {
            apply(
                &self.cli,
                Some(&self.ui()),
                env,
                &self.running,
                body.to_string().as_bytes(),
            )
        }

        fn put(&self, body: Value) -> Result<(), Answer> {
            self.put_in(&env(), body)
        }

        fn refused(&self, body: Value) -> (StatusCode, Value) {
            let refused = self.put(body).expect_err("refused");
            assert!(!self.ui().exists(), "nothing written");
            refused
        }
    }

    const INGEST_ONLY: &[&str] = &[
        "-i",
        "",
        "--ingest-enable",
        "--forward-enable",
        "--ingest-port",
        "0",
    ];

    #[test]
    fn a_change_a_start_would_take_is_written() {
        let r = rig("put-ok", INGEST_ONLY, None);
        r.put(json!({"forward.database": "site", "logging.level": "DEBUG"}))
            .unwrap();
        let written = ui::load(&r.ui()).unwrap();
        assert_eq!(written.forward.database.as_deref(), Some("site"));
        assert_eq!(written.logging.level.as_deref(), Some("DEBUG"));

        r.put(json!({"logging.level": null})).unwrap();
        let written = ui::load(&r.ui()).unwrap();
        assert_eq!(written.forward.database.as_deref(), Some("site"), "kept");
        assert_eq!(written.logging.level, None, "cleared");
    }

    #[test]
    fn a_key_a_ui_may_not_set_is_a_400() {
        let r = rig("put-400", INGEST_ONLY, None);
        for body in [
            json!({"forward.daemon_id": "x"}),
            json!({"forward.cache_path": "/elsewhere/cache.db"}),
            json!({"test_pattern.enable": true}),
            json!({"server.port": 2323}),
            json!({"forward.port": "eighty"}),
            json!({"devices": [{"interface": "can0", "mode": "active"}]}),
            json!(["not", "an", "object"]),
            json!({}),
        ] {
            assert_eq!(r.refused(body.clone()).0, StatusCode::BAD_REQUEST, "{body}");
        }
        assert_eq!(
            r.refused(json!({"forward.daemon_id": "x"})).1["key"],
            "forward.daemon_id"
        );
    }

    #[test]
    fn devices_cannot_be_cleared_back_to_the_transmitting_default() {
        let r = rig("put-null-devices", &[], None);
        let (status, body) = r.refused(json!({"devices": null}));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["key"], "devices");
    }

    /// Off Linux no device can start, so every list is a 422.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_device_list_replaced_by_another_stays_passive() {
        let r = rig("put-devices", &[], None);
        r.put(json!({"devices": [{"interface": "can1"}]})).unwrap();
        r.put(json!({"devices": [{"interface": "can2"}]})).unwrap();
        let ui = ui::load(&r.ui()).unwrap();
        let s = Settings::resolve(&r.cli, None, Some(&ui), &env())
            .unwrap()
            .settings;
        assert_eq!(s.devices.len(), 1);
        assert_eq!(s.devices[0].interface, "can2");
        assert_eq!(s.devices[0].mode, crate::settings::Mode::Passive);
    }

    #[test]
    fn a_pinned_key_is_a_409_naming_where() {
        let r = rig(
            "put-409",
            &[
                "-i",
                "",
                "--ingest-enable",
                "--forward-enable",
                "--forward-port",
                "9323",
            ],
            Some("[forward]\nhost = \"gw\"\n"),
        );
        let (status, body) = r.refused(json!({"forward.port": 1}));
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["where"], "flag --forward-port");

        let (status, body) = r.refused(json!({"forward.host": "elsewhere"}));
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["where"], r.cli.config.as_deref().unwrap());

        let from_env = Env {
            ingest_token: Some(Secret::new("t")),
            ..env()
        };
        let (status, body) = r
            .put_in(&from_env, json!({"ingest.token": "u"}))
            .unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["where"], "env WIRETAP_INGEST_TOKEN");
        assert!(!r.ui().exists());
    }

    #[test]
    fn a_change_that_would_stop_the_start_is_a_422() {
        let r = rig("put-422", &["-i", ""], None);
        let (status, body) = r.refused(json!({"ingest.enable": true}));
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body["error"].as_str().unwrap().contains("[forward]"),
            "{body}"
        );

        let (status, _) = r.refused(json!({"ingest.enable": true, "forward.enable": true, "forward.database": "Not A Name"}));
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let unpinned = rig("put-422-devices", &[], None);
        let (status, _) = unpinned.refused(json!({"devices": [{"interface": "/dev/ttyUSB0"}]}));
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let taken = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let (status, body) =
            r.refused(json!({"ingest.enable": true, "forward.enable": true, "ingest.port": port}));
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body["error"].as_str().unwrap().contains("cannot listen"),
            "{body}"
        );
    }

    #[test]
    fn with_nowhere_to_write_a_change_is_refused() {
        let r = rig("put-503", INGEST_ONLY, None);
        let (status, _) = apply(&r.cli, None, &env(), &r.running, b"{}").unwrap_err();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }
}
