//! The database password and the admin key on disk, and
//! `wiretap-backend rotate`, which asks the running gateway to rotate one.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;
use rand::RngCore;
use serde::Deserialize;
use wiretap_model::Secret;

use crate::config::Config;

/// Where each secret is persisted; `None` when it comes from the environment.
#[derive(Debug, Clone, Default)]
pub struct SecretFiles {
    pub pg_password: Option<PathBuf>,
    pub admin_key: Option<PathBuf>,
}

/// 32 random bytes as hex, as the image's entrypoint makes them.
pub fn random_hex() -> String {
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    hex::encode(raw)
}

/// A trailing newline is not part of it, as `initdb --pwfile` reads it.
pub fn read(path: &Path) -> Result<Secret, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value = raw.trim_end_matches(['\r', '\n']);
    if value.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok(Secret::new(value))
}

pub fn staged(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".next");
    name.into()
}

pub fn stage(file: &Path, value: &str) -> io::Result<()> {
    let mut staged = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(staged(file))?;
    staged.write_all(value.as_bytes())?;
    staged.sync_all()
}

pub fn promote(file: &Path) -> io::Result<()> {
    fs::rename(staged(file), file)?;
    let directory = file.parent().filter(|p| !p.as_os_str().is_empty());
    File::open(directory.unwrap_or(Path::new(".")))?.sync_all()
}

pub fn persist(file: &Path, value: &str) -> Result<(), String> {
    stage(file, value)
        .and_then(|()| promote(file))
        .map_err(|e| format!("{}: {e}", file.display()))
}

/// What a rotation answers, over the API and so to the CLI.
#[derive(Deserialize)]
struct Rotated {
    secret: String,
    note: String,
}

#[derive(Deserialize)]
struct Refused {
    error: String,
}

/// The gateway's own TCP listener, reached on loopback when it listens on all
/// addresses.
async fn local_listener(config: &Config) -> Result<SocketAddr, String> {
    let listen = &config.http_listen;
    let mut addr = tokio::net::lookup_host(listen)
        .await
        .map_err(|e| format!("WIRETAP_HTTP_LISTEN {listen:?}: {e}"))?
        .next()
        .ok_or(format!("WIRETAP_HTTP_LISTEN {listen:?}: no address"))?;
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => addr.set_ip(Ipv4Addr::LOCALHOST.into()),
        IpAddr::V6(ip) if ip.is_unspecified() => addr.set_ip(Ipv6Addr::LOCALHOST.into()),
        _ => {}
    }
    Ok(addr)
}

/// `wiretap-backend rotate db-password|admin-key`: the gateway does the work,
/// authenticated by the admin key it was started with. The new secret goes to
/// stdout, and what to do with it to stderr.
pub async fn rotate_command(config: &Config, which: &str) -> Result<(), String> {
    if !matches!(which, "db-password" | "admin-key") {
        return Err(format!(
            "unknown secret '{which}': db-password or admin-key"
        ));
    }
    let key = config.bootstrap_admin_key.as_ref().ok_or(
        "no admin key to ask the gateway with: WIRETAP_ADMIN_KEY or WIRETAP_ADMIN_KEY_FILE",
    )?;
    let addr = local_listener(config).await?;
    let request = hyper::Request::post(format!(
        "{}/v1/admin/secrets/{which}/rotate",
        config.base_path
    ))
    .header(hyper::header::HOST, addr.to_string())
    .header(
        hyper::header::AUTHORIZATION,
        format!("Bearer {}", key.expose()),
    )
    .body(Empty::<Bytes>::new())
    .map_err(|e| e.to_string())?;

    let exchange = async {
        let stream = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| format!("the gateway on {addr}: {e}"))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(connection);
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        Ok::<_, String>((status, body))
    };
    let (status, body) = tokio::time::timeout(Duration::from_secs(60), exchange)
        .await
        .map_err(|_| format!("the gateway on {addr} did not answer"))??;

    if !status.is_success() {
        let why = serde_json::from_slice::<Refused>(&body)
            .map_or_else(|_| String::from_utf8_lossy(&body).into_owned(), |r| r.error);
        let hint = if status == hyper::StatusCode::UNAUTHORIZED {
            "; if the admin key was rotated since the gateway started, pass the current one \
             as WIRETAP_ADMIN_KEY"
        } else {
            ""
        };
        return Err(format!("the gateway refused ({status}): {why}{hint}"));
    }
    let rotated: Rotated = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    println!("{}", rotated.secret);
    eprintln!("{}", rotated.note);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wiretap-secrets-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("secret")
    }

    #[test]
    fn a_trailing_newline_is_not_part_of_the_secret() {
        let file = scratch("newline");
        fs::write(&file, "abc\n").unwrap();
        assert_eq!(read(&file).unwrap().expose(), "abc");
        fs::write(&file, "\n").unwrap();
        assert!(read(&file).is_err());
    }

    #[test]
    fn a_staged_secret_replaces_the_file_only_when_promoted() {
        use std::os::unix::fs::PermissionsExt;
        let file = scratch("stage");
        fs::write(&file, "old").unwrap();
        stage(&file, "new").unwrap();
        assert_eq!(read(&file).unwrap().expose(), "old");
        promote(&file).unwrap();
        assert_eq!(read(&file).unwrap().expose(), "new");
        assert!(!staged(&file).exists());
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
