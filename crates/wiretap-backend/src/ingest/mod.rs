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
use wiretap_protocol::ingest as proto;

use crate::config::Config;
use crate::db::Databases;
use crate::keys::KeyStore;
use proto::*;
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
    time_relative: bool,
    /// The parser for the version the client announced.
    parse: BatchParser,
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

    async fn handle_client(&self, mut stream: TcpStream, peer: &str) -> Result<(), String> {
        let idle_limit = Duration::from_secs_f64(self.config.ingest_keepalive_secs * 3.0);
        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        let mut read_buf = [0u8; 65536];
        let mut authed: Option<Session> = None;

        let result = loop {
            // Read with idle timeout (any traffic counts as keepalive)
            let n = match tokio::time::timeout(idle_limit, stream.read(&mut read_buf)).await {
                Ok(Ok(0)) => break Ok(()),
                Ok(Ok(n)) => n,
                Ok(Err(e)) => break Err(format!("read: {e}")),
                Err(_) => break Err("idle timeout".into()),
            };
            buf.extend_from_slice(&read_buf[..n]);

            loop {
                let frame = match proto::take_frame(&mut buf) {
                    Ok(Some(f)) => f,
                    Ok(None) => break,
                    Err(e) => return self.finish(authed, Err(e)).await,
                };

                if !frame.crc_ok {
                    // Best effort: a corrupt BATCH can be retried by seq
                    if frame.mtype == MSG_BATCH && frame.body.len() >= 4 {
                        let seq = u32::from_le_bytes(frame.body[0..4].try_into().unwrap());
                        stream
                            .write_all(&proto::encode_ack(seq, ACK_CRC, 0))
                            .await
                            .map_err(|e| format!("write: {e}"))?;
                    }
                    continue;
                }

                match frame.mtype {
                    MSG_HELLO => match self.handle_hello(&frame.body, peer).await {
                        Ok((ack, session)) => {
                            stream
                                .write_all(&ack)
                                .await
                                .map_err(|e| format!("write: {e}"))?;
                            match session {
                                Some(s) => authed = Some(s),
                                None => return self.finish(authed, Ok(())).await,
                            }
                        }
                        Err(e) => return self.finish(authed, Err(e)).await,
                    },
                    MSG_PING => {
                        stream
                            .write_all(&proto::encode_message(MSG_PONG, b""))
                            .await
                            .map_err(|e| format!("write: {e}"))?;
                    }
                    MSG_BATCH => {
                        let Some(session) = authed.as_ref() else {
                            return self.finish(authed, Err("batch before hello".into())).await;
                        };
                        let ack = self.handle_batch(&frame.body, session).await;
                        stream
                            .write_all(&ack)
                            .await
                            .map_err(|e| format!("write: {e}"))?;
                    }
                    _ => {} // unknown type: ignore (forward compatibility)
                }
            }
        };
        self.finish(authed, result).await
    }

    /// Deregister the session (if any) and pass the result through.
    async fn finish(
        &self,
        authed: Option<Session>,
        result: Result<(), String>,
    ) -> Result<(), String> {
        if let Some(session) = authed {
            self.sessions.inner.lock().await.remove(&session.id);
        }
        result
    }

    /// Returns the HELLO_ACK to send plus the established session (None when
    /// the ACK is a rejection and the connection should close after sending).
    async fn handle_hello(
        &self,
        body: &[u8],
        peer: &str,
    ) -> Result<(Vec<u8>, Option<Session>), String> {
        let now_us = Utc::now().timestamp_micros() as u64;
        let reject = |status: u8| Ok((proto::encode_hello_ack(status, now_us), None));

        let hello = proto::parse_hello(body).map_err(|e| format!("bad hello: {e}"))?;
        let Some(parse) = proto::batch_parser(hello.version) else {
            return reject(HELLO_BAD_VERSION);
        };

        let key = String::from_utf8_lossy(&hello.token).into_owned();
        let Some(info) = self.keys.validate(&key).await else {
            tracing::warn!("ingest client {peer} failed auth");
            return reject(HELLO_BAD_AUTH);
        };
        if !info.role.allows_ingest() {
            tracing::warn!("ingest client {peer} key '{}' lacks ingest role", info.name);
            return reject(HELLO_BAD_AUTH);
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
                return reject(HELLO_BAD_AUTH);
            }
            (Some(pin), _) => pin.clone(),
            (None, "") => self.dbs.default_database().to_string(),
            (None, requested) => requested.to_string(),
        };

        let pool = match self.dbs.ensure_database(&database, true).await {
            Ok(pool) => pool,
            Err(e) => {
                tracing::warn!("ingest client {peer}: database '{database}': {e}");
                return reject(HELLO_BAD_DATABASE);
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
        Ok((
            proto::encode_hello_ack(HELLO_OK, now_us),
            Some(Session {
                pool,
                id: session_id,
                time_relative: hello.time_relative,
                parse,
            }),
        ))
    }

    /// Write one batch to Postgres, then ACK. The client only treats frames as
    /// delivered once they are durably stored; a DB failure yields ACK_OVERLOADED
    /// so the device caches and retries (no frames are buffered in gateway RAM).
    async fn handle_batch(&self, body: &[u8], session: &Session) -> Vec<u8> {
        let batch = match (session.parse)(body, self.config.ingest_max_batch_frames) {
            None => return proto::encode_ack(0, ACK_MALFORMED, 0),
            Some(Err(seq)) => return proto::encode_ack(seq, ACK_MALFORMED, 0),
            Some(Ok(b)) => b,
        };

        let seq = batch.seq;
        let rows: Vec<FrameRow> = batch
            .stamped(session.time_relative, Utc::now().timestamp_micros() as u64)
            .map(|(ts_us, r)| {
                FrameRow::new(ts_us as i64, r.kind, r.id_flags, r.flags, r.bus, r.payload)
            })
            .collect();
        let count = rows.len() as u64;

        match copy_rows(&session.pool, &rows).await {
            Ok(()) => {
                if let Some(s) = self.sessions.inner.lock().await.get_mut(&session.id) {
                    s.frames += count;
                    s.batches += 1;
                }
                proto::encode_ack(seq, ACK_OK, 0)
            }
            Err(e) => {
                tracing::warn!("ingest write failed (seq {seq}): {e}");
                proto::encode_ack(seq, ACK_OVERLOADED, 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
