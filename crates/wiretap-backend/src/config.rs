//! Backend configuration, sourced from environment variables (the natural
//! configuration surface inside a container). Every value has a default
//! suitable for the shipped docker-compose stack.

use std::env;
use std::path::PathBuf;

use wslib_secret::Secret;

use crate::secrets::SecretFiles;

#[derive(Debug, Clone)]
pub struct Config {
    pub http_listen: String,
    pub ingest_listen: String,
    pub pg_host: String,
    pub pg_port: u16,
    pub pg_user: String,
    pub pg_password: Secret,
    /// Database created on first start and used when an ingest client
    /// doesn't name one. Also hosts the wiretap_meta schema (API keys).
    pub default_database: String,
    /// Break-glass admin API key, honoured until rotated (cannot be revoked
    /// from the admin UI). Optional — without it, keys must be seeded in SQL.
    pub bootstrap_admin_key: Option<Secret>,
    pub secret_files: SecretFiles,
    /// Allow ingest clients / imports to auto-create unknown databases.
    pub auto_create_databases: bool,
    /// Migrate capture databases to the current schema on start. Off by
    /// default: a migration can take hours, so a database behind waits for an
    /// operator's Migrate now, buffering its ingest meanwhile.
    pub auto_migrate: bool,
    pub ingest_keepalive_secs: f64,
    pub ingest_max_batch_frames: usize,
    /// Prefix every HTTP route on `http_listen` is served under, e.g.
    /// `/apps/wiretap-gateway`; empty serves at the root.
    pub base_path: String,
    pub admin_dir: PathBuf,
    pub app_socket: Option<AppSocket>,
}

/// The Unix socket an ApplianceOS console proxies the app's routes to.
#[derive(Debug, Clone)]
pub struct AppSocket {
    pub path: PathBuf,
    /// The only peer whose `Appliance-Identity` is believed.
    pub console_uid: u32,
    pub base_path: String,
}

/// The uid every unmapped uid appears as under userns-remap, so it cannot tell
/// the console apart.
const OVERFLOW_UID: u32 = 65534;

fn var_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn parse_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// `apps/x/` becomes `/apps/x`, and `/` or nothing the root. Only plain path
/// segments: the result is nested into the router as a literal prefix.
fn normalise_base_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        return Some(String::new());
    }
    let plain = |segment: &str| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c))
    };
    trimmed.split('/').all(plain).then(|| format!("/{trimmed}"))
}

fn base_path_var(name: &str, default: &str) -> Result<String, String> {
    let raw = var_or(name, default);
    normalise_base_path(&raw).ok_or_else(|| {
        format!("{name} {raw:?} must be plain path segments, like /apps/wiretap-gateway")
    })
}

fn app_socket_from_env() -> Result<Option<AppSocket>, String> {
    let Some(path) = env::var_os("WIRETAP_APP_SOCKET").filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(format!(
            "WIRETAP_APP_SOCKET {path:?} must be an absolute path"
        ));
    }
    let raw_uid = var_or("WIRETAP_CONSOLE_UID", "0");
    let console_uid = raw_uid
        .parse()
        .ok()
        .filter(|&uid| uid != OVERFLOW_UID)
        .ok_or_else(|| {
            format!("WIRETAP_CONSOLE_UID {raw_uid:?} must be a uid other than {OVERFLOW_UID}")
        })?;
    Ok(Some(AppSocket {
        path,
        console_uid,
        base_path: base_path_var("WIRETAP_APP_BASE_PATH", "/apps/wiretap-gateway")?,
    }))
}

/// A secret from `var`, or else from the file `file_var` names, which is then
/// where it is persisted.
fn secret_var(var: &str, file_var: &str) -> Result<(Option<Secret>, Option<PathBuf>), String> {
    if let Some(value) = env::var(var).ok().filter(|v| !v.is_empty()) {
        return Ok((Some(Secret::new(value)), None));
    }
    let Some(path) = env::var_os(file_var).filter(|p| !p.is_empty()) else {
        return Ok((None, None));
    };
    let path = PathBuf::from(path);
    let secret = Secret::from_file(&path).map_err(|e| e.to_string())?;
    Ok((Some(secret), Some(path)))
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let (pg_password, pg_password_file) =
            secret_var("POSTGRES_PASSWORD", "WIRETAP_PG_PASSWORD_FILE")?;
        let pg_password = pg_password
            .ok_or("POSTGRES_PASSWORD or WIRETAP_PG_PASSWORD_FILE is required".to_string())?;
        let (bootstrap_admin_key, admin_key_file) =
            secret_var("WIRETAP_ADMIN_KEY", "WIRETAP_ADMIN_KEY_FILE")?;
        Ok(Self {
            http_listen: var_or("WIRETAP_HTTP_LISTEN", "0.0.0.0:8423"),
            ingest_listen: var_or("WIRETAP_INGEST_LISTEN", "0.0.0.0:9323"),
            pg_host: var_or("WIRETAP_PG_HOST", "timescaledb"),
            pg_port: parse_or("WIRETAP_PG_PORT", 5432),
            pg_user: var_or("WIRETAP_PG_USER", "postgres"),
            pg_password,
            default_database: var_or("WIRETAP_DEFAULT_DB", "wiretap"),
            bootstrap_admin_key,
            secret_files: SecretFiles {
                pg_password: pg_password_file,
                admin_key: admin_key_file,
            },
            auto_create_databases: parse_or("WIRETAP_AUTO_CREATE", true),
            auto_migrate: parse_or("WIRETAP_AUTO_MIGRATE", false),
            ingest_keepalive_secs: parse_or("WIRETAP_INGEST_KEEPALIVE_SECS", 30.0),
            ingest_max_batch_frames: parse_or("WIRETAP_INGEST_MAX_BATCH_FRAMES", 256),
            base_path: base_path_var("WIRETAP_BASE_PATH", "")?,
            admin_dir: var_or("WIRETAP_ADMIN_DIR", "/usr/share/wiretap-admin").into(),
            app_socket: app_socket_from_env()?,
        })
    }

    /// One database, without the password: that is added per connection,
    /// so a rotated one reaches every pool.
    pub fn pg_config(&self, database: &str) -> tokio_postgres::Config {
        let mut config = tokio_postgres::Config::new();
        config
            .host(&self.pg_host)
            .port(self.pg_port)
            .user(&self.pg_user)
            .dbname(database);
        config
    }
}

#[cfg(test)]
mod tests {
    use super::{normalise_base_path, secret_var};

    /// Each test owns its variable names: the environment is the process's.
    #[test]
    fn a_secret_in_the_environment_wins_over_its_file_which_is_then_not_its_source() {
        let file = std::env::temp_dir().join(format!("wiretap-secret-var-{}", std::process::id()));
        std::fs::write(&file, "from-file\n").unwrap();
        std::env::set_var("WIRETAP_TEST_SV_FILE", &file);

        let (value, source) = secret_var("WIRETAP_TEST_SV_UNSET", "WIRETAP_TEST_SV_FILE").unwrap();
        assert_eq!(value.unwrap().expose(), "from-file");
        assert_eq!(source.as_deref(), Some(file.as_path()));

        std::env::set_var("WIRETAP_TEST_SV_ENV", "from-env");
        let (value, source) = secret_var("WIRETAP_TEST_SV_ENV", "WIRETAP_TEST_SV_FILE").unwrap();
        assert_eq!(value.unwrap().expose(), "from-env");
        assert!(source.is_none());

        std::env::set_var("WIRETAP_TEST_SV_MISSING", file.with_extension("missing"));
        assert!(secret_var("WIRETAP_TEST_SV_UNSET", "WIRETAP_TEST_SV_MISSING").is_err());
        let (value, source) = secret_var("WIRETAP_TEST_SV_UNSET", "WIRETAP_TEST_SV_UNSET").unwrap();
        assert!(value.is_none() && source.is_none());
    }

    #[test]
    fn a_base_path_gains_a_leading_slash_and_loses_a_trailing_one() {
        for (raw, want) in [
            ("", ""),
            ("/", ""),
            ("  ", ""),
            ("/apps/wiretap-gateway", "/apps/wiretap-gateway"),
            ("apps/wiretap-gateway/", "/apps/wiretap-gateway"),
            ("/v1.2_x~y/", "/v1.2_x~y"),
        ] {
            assert_eq!(normalise_base_path(raw).as_deref(), Some(want), "{raw:?}");
        }
    }

    #[test]
    fn a_base_path_that_is_not_plain_segments_is_refused() {
        for raw in [
            "/apps//x",
            "/apps/../x",
            "/./x",
            "/apps/{name}",
            "/apps/*rest",
            "/apps/x?y",
            "/apps x",
            "https://host/apps",
            "/apps/%2e%2e",
        ] {
            assert!(normalise_base_path(raw).is_none(), "{raw:?} was accepted");
        }
    }
}
