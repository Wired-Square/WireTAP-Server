//! Backend configuration, sourced from environment variables (the natural
//! configuration surface inside a container). Every value has a default
//! suitable for the shipped docker-compose stack.

use std::env;

use wiretap_model::Secret;

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
    /// Break-glass admin API key, always honoured (cannot be revoked from
    /// the admin UI). Optional — without it, keys must be seeded in SQL.
    pub bootstrap_admin_key: Option<Secret>,
    /// Allow ingest clients / imports to auto-create unknown databases.
    pub auto_create_databases: bool,
    /// Migrate capture databases to the current schema on start. On by default:
    /// the alternative is a gateway that refuses to serve databases it could
    /// have fixed. Turn it off to migrate by hand — snapshotting a large archive
    /// first, say — and run schema/migrations/*.sql yourself.
    pub auto_migrate: bool,
    pub ingest_keepalive_secs: f64,
    pub ingest_max_batch_frames: usize,
}

fn var_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn parse_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let pg_password = env::var("POSTGRES_PASSWORD")
            .map(Secret::new)
            .map_err(|_| "POSTGRES_PASSWORD is required".to_string())?;
        Ok(Self {
            http_listen: var_or("WIRETAP_HTTP_LISTEN", "0.0.0.0:8423"),
            ingest_listen: var_or("WIRETAP_INGEST_LISTEN", "0.0.0.0:9323"),
            pg_host: var_or("WIRETAP_PG_HOST", "timescaledb"),
            pg_port: parse_or("WIRETAP_PG_PORT", 5432),
            pg_user: var_or("WIRETAP_PG_USER", "postgres"),
            pg_password,
            default_database: var_or("WIRETAP_DEFAULT_DB", "wiretap"),
            bootstrap_admin_key: env::var("WIRETAP_ADMIN_KEY")
                .ok()
                .filter(|k| !k.is_empty())
                .map(Secret::new),
            auto_create_databases: parse_or("WIRETAP_AUTO_CREATE", true),
            auto_migrate: parse_or("WIRETAP_AUTO_MIGRATE", true),
            ingest_keepalive_secs: parse_or("WIRETAP_INGEST_KEEPALIVE_SECS", 30.0),
            ingest_max_batch_frames: parse_or("WIRETAP_INGEST_MAX_BATCH_FRAMES", 256),
        })
    }

    /// libpq-style connection string for one database.
    pub fn pg_dsn(&self, database: &str) -> String {
        format!(
            "host={} port={} user={} password={} dbname={}",
            self.pg_host,
            self.pg_port,
            self.pg_user,
            self.pg_password.expose(),
            database
        )
    }
}
