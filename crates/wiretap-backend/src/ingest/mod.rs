//! Binary TCP ingest listener: accepts device connections, authenticates the
//! HELLO against the API key store, routes (and auto-creates) the target
//! capture database, and writes each batch to Postgres synchronously — the
//! client is only ACKed once the batch is durably stored (ACK-after-write), so
//! a DB outage back-pressures the device into its own disk cache.

pub mod writer;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_postgres::error::SqlState;
use wiretap_protocol::ingest::*;

use crate::config::Config;
use crate::db::{Databases, DbError};
use crate::keys::KeyStore;
use writer::{copy_rows, FrameRow};

#[derive(Debug, Serialize, Clone)]
pub struct IngestSessionInfo {
    pub peer: String,
    pub key_name: String,
    pub database: String,
    pub protocol_version: u8,
    pub frames: u64,
    pub batches: u64,
    pub connected_at: DateTime<Utc>,
}

#[derive(Clone, Default)]
pub struct Sessions {
    inner: Arc<Mutex<HashMap<u64, IngestSessionInfo>>>,
    next_id: Arc<AtomicU64>,
}

impl Sessions {
    pub async fn list(&self) -> Vec<IngestSessionInfo> {
        self.inner.lock().await.values().cloned().collect()
    }

    async fn open(&self, info: IngestSessionInfo) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().await.insert(id, info);
        id
    }

    async fn close(&self, id: u64) {
        self.inner.lock().await.remove(&id);
    }
}

pub struct IngestServer {
    pub config: Arc<Config>,
    pub dbs: Databases,
    pub keys: KeyStore,
    pub sessions: Sessions,
}

/// What a HELLO established, held for the life of the connection.
struct Session {
    pool: Pool,
    id: u64,
}

impl IngestServer {
    pub async fn run(self: Arc<Self>) -> Result<(), String> {
        let listener = TcpListener::bind(&self.config.ingest_listen)
            .await
            .map_err(|e| format!("ingest bind {}: {e}", self.config.ingest_listen))?;
        tracing::info!("ingest listening on {}", self.config.ingest_listen);
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let server = self.clone();
                    tokio::spawn(async move {
                        let peer = peer.to_string();
                        tracing::info!("ingest client {peer} connected");
                        if let Err(e) = server.handle_client(stream, &peer).await {
                            tracing::debug!("ingest client {peer}: {e}");
                        }
                        tracing::info!("ingest client {peer} disconnected");
                    });
                }
                Err(e) => tracing::warn!("ingest accept error: {e}"),
            }
        }
    }

    async fn handle_client(&self, stream: TcpStream, peer: &str) -> Result<(), String> {
        let mut authed = None;
        let result = self.serve(stream, peer, &mut authed).await;
        if let Some(session) = authed {
            self.sessions.close(session.id).await;
        }
        result
    }

    async fn serve(
        &self,
        mut stream: TcpStream,
        peer: &str,
        authed: &mut Option<Session>,
    ) -> Result<(), String> {
        let mut machine = ServerSession::new(ServerConfig {
            versions: 2..=3,
            max_records: self.config.ingest_max_batch_frames,
            short_batch: ShortBatch::NackSeqZero,
            idle_limit: Some(Duration::from_secs_f64(
                self.config.ingest_keepalive_secs * 3.0,
            )),
        });
        let idle_limit = machine.idle_limit().unwrap_or(Duration::MAX);
        let mut read_buf = [0u8; 65536];

        loop {
            // Read with idle timeout (any traffic counts as keepalive)
            let n = match tokio::time::timeout(idle_limit, stream.read(&mut read_buf)).await {
                Ok(Ok(0)) => return Ok(()),
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(format!("read: {e}")),
                Err(_) => return Err("idle timeout".into()),
            };
            machine.receive(&read_buf[..n]);

            while let Some(action) = machine.poll() {
                let reply = match action {
                    Action::Reply(bytes) => bytes,
                    Action::Hello(hello) => {
                        let status = match self.handle_hello(&hello, peer).await {
                            Ok(session) => {
                                if let Some(previous) = authed.replace(session) {
                                    self.sessions.close(previous.id).await;
                                }
                                HELLO_OK
                            }
                            Err(status) => status,
                        };
                        machine
                            .answer_hello(status, now_us(), &[])
                            .expect("no assignments to overflow")
                    }
                    Action::HelloRefused(status) => machine
                        .answer_hello(status, now_us(), &[])
                        .expect("no assignments to overflow"),
                    Action::Batch(incoming) => {
                        let session = authed.as_ref().expect("a batch follows an accepted HELLO");
                        let seq = incoming.batch.seq;
                        let status = self.handle_batch(incoming, session).await;
                        machine.ack(seq, status, 0)
                    }
                    Action::Nack { seq, status } => machine.ack(seq, status, 0),
                    Action::CatalogGet(_) => machine.answer_catalog(Err(CATALOG_UNKNOWN)),
                    Action::Close(CloseReason::Refused(_)) => return Ok(()),
                    Action::Close(reason) => return Err(format!("closed: {reason:?}")),
                };
                stream
                    .write_all(&reply)
                    .await
                    .map_err(|e| format!("write: {e}"))?;
            }
        }
    }

    /// The established session, or the status that refuses the HELLO.
    async fn handle_hello(&self, hello: &Hello, peer: &str) -> Result<Session, u8> {
        let key = String::from_utf8_lossy(&hello.token).into_owned();
        let Some(info) = self.keys.validate(&key).await else {
            tracing::warn!("ingest client {peer} failed auth");
            return Err(HELLO_BAD_AUTH);
        };
        if !info.role.allows_ingest() {
            tracing::warn!("ingest client {peer} key '{}' lacks ingest role", info.name);
            return Err(HELLO_BAD_AUTH);
        }

        // Resolve the target database: explicit > key pin > server default.
        // A pinned key may not name any other database.
        let database = match (&info.database_pin, hello.database.as_str()) {
            (Some(pin), "") => pin.clone(),
            (Some(pin), requested) if requested != pin => {
                tracing::warn!(
                    "ingest client {peer} key '{}' pinned to '{pin}' requested '{requested}'",
                    info.name
                );
                return Err(HELLO_BAD_AUTH);
            }
            (Some(pin), _) => pin.clone(),
            (None, "") => self.dbs.default_database().to_string(),
            (None, requested) => requested.to_string(),
        };

        let pool = match self.dbs.ensure_database(&database, true).await {
            Ok(pool) => pool,
            Err(e) => {
                tracing::warn!("ingest client {peer}: database '{database}': {e}");
                return Err(match e {
                    DbError::Refused(_) => HELLO_BAD_DATABASE,
                    DbError::Unavailable(_) => HELLO_UNAVAILABLE,
                });
            }
        };

        let session_id = self
            .sessions
            .open(IngestSessionInfo {
                peer: peer.to_string(),
                key_name: info.name,
                database: database.clone(),
                protocol_version: hello.version,
                frames: 0,
                batches: 0,
                connected_at: Utc::now(),
            })
            .await;
        tracing::info!(
            "ingest client {peer} authenticated, database '{database}', protocol v{}",
            hello.version
        );
        Ok(Session {
            pool,
            id: session_id,
        })
    }

    /// Write one batch to Postgres, then ACK. The client only treats frames as
    /// delivered once they are durably stored; a DB failure yields ACK_OVERLOADED
    /// so the device caches and retries (no frames are buffered in gateway RAM),
    /// unless no retry could store it (see [`writer::refused_the_rows`]).
    async fn handle_batch(&self, incoming: IncomingBatch, session: &Session) -> u8 {
        let seq = incoming.batch.seq;
        let count = incoming.batch.records.len() as u64;
        let rows: Result<Vec<FrameRow>, _> = incoming
            .batch
            .stamped(incoming.time_relative, now_us())
            .map(|(ts_us, r)| {
                FrameRow::new(ts_us as i64, r.kind, r.id_flags, r.flags, r.bus, r.payload)
            })
            .collect();

        match async { copy_rows(&session.pool, &rows?).await }.await {
            Ok(()) => {
                if let Some(s) = self.sessions.inner.lock().await.get_mut(&session.id) {
                    s.frames += count;
                    s.batches += 1;
                }
                ACK_OK
            }
            Err(e) => {
                tracing::warn!("ingest write failed (seq {seq}): {e}");
                ack_for_copy_error(e.code.as_ref())
            }
        }
    }
}

fn ack_for_copy_error(code: Option<&SqlState>) -> u8 {
    if writer::refused_the_rows(code) {
        ACK_MALFORMED
    } else {
        ACK_OVERLOADED
    }
}

fn now_us() -> u64 {
    Utc::now().timestamp_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::{config_at, unreachable_databases};

    fn server(dbs: Databases) -> IngestServer {
        IngestServer {
            config: Arc::new(config_at(0, true)),
            keys: KeyStore::new(dbs.clone(), Some("bootstrap")),
            dbs,
            sessions: Sessions::default(),
        }
    }

    fn hello(version: u8, database: &str) -> Hello {
        Hello {
            version,
            ..Hello::v2(b"bootstrap", database, false)
        }
    }

    async fn hello_status(dbs: Databases, database: &str) -> u8 {
        let hello = hello(PROTO_VERSION, database);
        match server(dbs).handle_hello(&hello, "192.0.2.10:40000").await {
            Ok(_) => HELLO_OK,
            Err(status) => status,
        }
    }

    async fn connected(server: IngestServer) -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            let _ = server.handle_client(stream, &peer.to_string()).await;
        });
        TcpStream::connect(addr).await.unwrap()
    }

    async fn hello_ack(c: &mut TcpStream, hello: &Hello) -> HelloAck {
        c.write_all(&encode_hello(hello).unwrap()).await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let frame = loop {
            if let Some(frame) = take_frame(&mut buf).unwrap() {
                break frame;
            }
            let n = c.read(&mut chunk).await.unwrap();
            assert!(n > 0, "closed without a HELLO_ACK");
            buf.extend_from_slice(&chunk[..n]);
        };
        assert_eq!(frame.mtype, MSG_HELLO_ACK);
        parse_hello_ack(&frame.body).unwrap()
    }

    #[tokio::test]
    async fn a_v3_hello_passes_the_version_check() {
        let mut c = connected(server(unreachable_databases(true))).await;
        let mut hello = hello(3, "archive");
        hello.daemon_id = "bench".into();
        hello.devices = vec![Device {
            bus: 0,
            name: "can0".into(),
        }];
        let ack = hello_ack(&mut c, &hello).await;
        assert_eq!((ack.status, ack.accepted_version), (HELLO_UNAVAILABLE, 3));
    }

    #[tokio::test]
    async fn a_v1_hello_is_refused_naming_v3() {
        let mut c = connected(server(unreachable_databases(true))).await;
        let ack = hello_ack(&mut c, &hello(1, "")).await;
        assert_eq!((ack.status, ack.accepted_version), (HELLO_BAD_VERSION, 3));
    }

    fn incoming(kind: RecordKind, id_flags: u32) -> IncomingBatch {
        let mut records = Vec::new();
        encode_record_into(&mut records, 0, 0, kind, 0, 0, id_flags, &[1]);
        let mut msg = encode_batch(9, 1_700_000_000_000_000, 1, &records);
        let frame = take_frame(&mut msg).unwrap().unwrap();
        IncomingBatch {
            batch: parse_batch(&frame.body, MAX_BATCH_RECORDS)
                .unwrap()
                .unwrap(),
            time_relative: false,
            version: 3,
        }
    }

    #[tokio::test]
    async fn a_raw_serial_batch_is_refused_as_malformed_without_reaching_the_database() {
        let server = server(unreachable_databases(true));
        let pool = Pool::builder(deadpool_postgres::Manager::new(
            server.config.pg_dsn("wiretap").parse().unwrap(),
            tokio_postgres::NoTls,
        ))
        .build()
        .unwrap();
        let session = Session { pool, id: 0 };

        let raw = incoming(RecordKind::RawSerial, raw_serial_id(1));
        assert_eq!(server.handle_batch(raw, &session).await, ACK_MALFORMED);
        let can = incoming(RecordKind::Can, 0x123);
        assert_eq!(server.handle_batch(can, &session).await, ACK_OVERLOADED);
    }

    #[tokio::test]
    async fn a_database_outage_at_hello_is_unavailable_not_a_bad_database() {
        let status = hello_status(unreachable_databases(true), "archive").await;
        assert_eq!(status, HELLO_UNAVAILABLE);
    }

    #[tokio::test]
    async fn an_invalid_database_name_at_hello_is_a_bad_database() {
        let status = hello_status(unreachable_databases(true), "no spaces").await;
        assert_eq!(status, HELLO_BAD_DATABASE);
    }

    #[tokio::test]
    async fn the_sessions_listing_names_each_sessions_protocol_version() {
        let sessions = Sessions::default();
        sessions
            .open(IngestSessionInfo {
                peer: "192.0.2.10:40000".into(),
                key_name: "bench".into(),
                database: "wiretap".into(),
                protocol_version: 2,
                frames: 0,
                batches: 0,
                connected_at: Utc::now(),
            })
            .await;
        let listed = serde_json::to_value(sessions.list().await).unwrap();
        assert_eq!(listed[0]["protocol_version"], 2);
    }

    #[test]
    fn a_data_exception_is_refused_as_malformed_not_overloaded() {
        for code in [
            SqlState::DATETIME_FIELD_OVERFLOW,
            SqlState::INVALID_TEXT_REPRESENTATION,
        ] {
            assert_eq!(ack_for_copy_error(Some(&code)), ACK_MALFORMED, "{code:?}");
        }
    }

    #[test]
    fn an_integrity_constraint_violation_is_refused_as_malformed() {
        for code in [SqlState::UNIQUE_VIOLATION, SqlState::NOT_NULL_VIOLATION] {
            assert_eq!(ack_for_copy_error(Some(&code)), ACK_MALFORMED, "{code:?}");
        }
    }

    #[test]
    fn a_failure_a_retry_may_get_past_stays_overloaded() {
        for code in [
            SqlState::CONNECTION_FAILURE,
            SqlState::ADMIN_SHUTDOWN,
            SqlState::DISK_FULL,
            SqlState::UNDEFINED_TABLE,
        ] {
            assert_eq!(ack_for_copy_error(Some(&code)), ACK_OVERLOADED, "{code:?}");
        }
        assert_eq!(ack_for_copy_error(None), ACK_OVERLOADED, "no SQLSTATE");
    }
}
