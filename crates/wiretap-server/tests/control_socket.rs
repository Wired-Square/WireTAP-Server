//! The daemon binary, changed over its control socket: a `PUT` answers 202,
//! and the daemon executes itself again, on the same PID, on the change.
//!
//! Ingest-only, with the gateway on a dead port and no config file at all, so
//! it starts on any host without CAN.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Daemon {
    dir: PathBuf,
    socket: PathBuf,
}

impl Daemon {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("wiretap-control-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("control.sock");
        Self { dir, socket }
    }

    fn start(&self, dead_port: u16) -> Child {
        let child = Command::new(env!("CARGO_BIN_EXE_wiretap-server"))
            .args(["-C", self.dir.join("absent.toml").to_str().unwrap()])
            .args([
                "-i",
                "",
                "--ingest-enable",
                "--ingest-host",
                "127.0.0.1",
                "--ingest-port",
                "0",
            ])
            .args(["--forward-enable", "--forward-port", &dead_port.to_string()])
            .args(["--control-socket", self.socket.to_str().unwrap()])
            .env("STATE_DIRECTORY", &self.dir)
            .env_remove("RUNTIME_DIRECTORY")
            .env_remove("WIRETAP_FORWARD_TOKEN")
            .env_remove("WIRETAP_INGEST_TOKEN")
            .stderr(Stdio::null())
            .spawn()
            .expect("the daemon starts");
        let deadline = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(&self.socket).is_err() {
            assert!(Instant::now() < deadline, "no control socket after 10 s");
            std::thread::sleep(Duration::from_millis(50));
        }
        child
    }

    /// A `GET`, or `None` while nothing answers, as across the restart.
    fn try_request(&self, method: &str, path: &str) -> Option<Value> {
        self.exchange(method, path, "").ok().map(|(_, body)| body)
    }

    fn request(&self, method: &str, path: &str, body: &str) -> (u16, Value) {
        self.exchange(method, path, body).expect("an answer")
    }

    fn exchange(&self, method: &str, path: &str, body: &str) -> io::Result<(u16, Value)> {
        let mut s = UnixStream::connect(&self.socket)?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )?;
        let mut response = String::new();
        s.read_to_string(&mut response)?;
        let bad = || io::Error::other(format!("not a whole response: {response:?}"));
        let (head, body) = response.split_once("\r\n\r\n").ok_or_else(bad)?;
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(bad)?;
        Ok((status, serde_json::from_str(body).map_err(|_| bad())?))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn wait(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the daemon did not exit within 30 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn setting<'a>(settings: &'a Value, key: &str) -> &'a Value {
    settings["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == key)
        .unwrap()
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_change_over_the_socket_is_applied_by_a_restart() {
    let dead_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let d = Daemon::new();

    let mut daemon = d.start(dead_port);
    assert_eq!(mode(&d.socket), 0o660);
    let (status, body) = d.request("GET", "/v1/status", "");
    assert_eq!(status, 200);
    assert_eq!(body["ingest"]["enabled"], true);
    assert_eq!(body["forward"]["enabled"], true);

    let (status, body) = d.request("PUT", "/v1/settings", r#"{"forward.port": 1}"#);
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["where"], "flag --forward-port");

    let (status, body) = d.request("PUT", "/v1/settings", r#"{"forward.database": "site"}"#);
    assert_eq!(status, 202, "{body}");

    // The same process, executed again, answers on the change.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(daemon.try_wait().unwrap().is_none(), "the daemon exited");
        if let Some(settings) = d.try_request("GET", "/v1/settings") {
            let database = setting(&settings, "forward.database");
            if database["source"] == "ui" {
                assert_eq!(database["value"], "site");
                assert_eq!(database["pinned"], false);
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no restart onto the change in 30 s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // SAFETY: a signal to a child this test started.
    unsafe { libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(wait(&mut daemon).code(), Some(0));
}
