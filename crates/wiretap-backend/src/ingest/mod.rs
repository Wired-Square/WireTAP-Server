//! Binary TCP ingest listener: accepts device connections, authenticates the
//! HELLO against the API key store, routes (and auto-creates) the target
//! capture database, and writes each batch to Postgres synchronously — the
//! client is only ACKed once the batch is durably stored (ACK-after-write), so
//! a DB outage back-pressures the device into its own disk cache.
//!
//! A capture daemon's v3 HELLO also records its devices and is answered with
//! their catalogue assignments, which its `CATALOG_GET`s then fetch.

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
use tokio::sync::{Mutex, Notify};
use tokio_postgres::error::SqlState;
use wiretap_protocol::ingest::*;

use crate::catalogs::Catalogs;
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

struct Live {
    info: IngestSessionInfo,
    daemon_id: String,
    interfaces: Vec<String>,
    reassigned: Arc<Notify>,
}

#[derive(Clone, Default)]
pub struct Sessions {
    inner: Arc<Mutex<HashMap<u64, Live>>>,
    next_id: Arc<AtomicU64>,
}

impl Sessions {
    pub async fn list(&self) -> Vec<IngestSessionInfo> {
        let live = self.inner.lock().await;
        live.values().map(|l| l.info.clone()).collect()
    }

    async fn open(&self, info: IngestSessionInfo, hello: &Hello, reassigned: Arc<Notify>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let live = Live {
            info,
            daemon_id: hello.daemon_id.clone(),
            interfaces: hello.devices.iter().map(|d| d.name.clone()).collect(),
            reassigned,
        };
        self.inner.lock().await.insert(id, live);
        id
    }

    async fn close(&self, id: u64) {
        self.inner.lock().await.remove(&id);
    }

    /// Close each live session of `daemon_id` whose HELLO named `interface`,
    /// once it has answered what it owes, so its reconnect reads the new
    /// assignment. Returns how many were told.
    pub async fn reassign(&self, daemon_id: &str, interface: &str) -> usize {
        let live = self.inner.lock().await;
        let affected = live
            .values()
            .filter(|l| l.daemon_id == daemon_id && l.interfaces.iter().any(|i| i == interface));
        affected.map(|l| l.reassigned.notify_one()).count()
    }
}

pub struct IngestServer {
    pub config: Arc<Config>,
    pub dbs: Databases,
    pub keys: KeyStore,
    pub sessions: Sessions,
    pub catalogs: Catalogs,
}

/// What a HELLO established, held for the life of the connection.
struct Session {
    pool: Pool,
    id: u64,
    daemon_id: String,
    devices: Vec<Device>,
}

/// What the session loop asks of the gateway, so the loop runs without
/// PostgreSQL in a test.
trait Gateway {
    /// The verdict, and the assignments a `HELLO_OK` carries.
    async fn hello(&mut self, hello: &Hello) -> (u8, Vec<Assignment>);
    async fn batch(&mut self, incoming: IncomingBatch) -> u8;
    async fn blob(&mut self, sha: &[u8; 20]) -> Result<&[u8], u8>;
    async fn catalog_status(&mut self, status: CatalogStatus);
}

/// One client's connection to this gateway.
struct Connection<'a> {
    server: &'a IngestServer,
    peer: &'a str,
    authed: Option<Session>,
    reassigned: Arc<Notify>,
    /// The blob last served: a daemon pulls one a chunk at a time.
    served: Option<([u8; 20], String)>,
}

impl Gateway for Connection<'_> {
    async fn hello(&mut self, hello: &Hello) -> (u8, Vec<Assignment>) {
        let reassigned = self.reassigned.clone();
        match self.server.handle_hello(hello, self.peer, reassigned).await {
            Ok((session, assignments)) => {
                if let Some(previous) = self.authed.replace(session) {
                    self.server.sessions.close(previous.id).await;
                }
                (HELLO_OK, assignments)
            }
            Err(status) => (status, Vec::new()),
        }
    }

    async fn batch(&mut self, incoming: IncomingBatch) -> u8 {
        let session = self
            .authed
            .as_ref()
            .expect("a batch follows an accepted HELLO");
        self.server.handle_batch(incoming, session).await
    }

    async fn blob(&mut self, sha: &[u8; 20]) -> Result<&[u8], u8> {
        if self.served.as_ref().is_none_or(|(served, _)| served != sha) {
            self.served = match self.server.catalogs.blob(sha).await {
                Ok(Some(content)) => Some((*sha, content)),
                Ok(None) => return Err(CATALOG_UNKNOWN),
                Err(e) => {
                    tracing::warn!("ingest client {}: {e}", self.peer);
                    return Err(CATALOG_UNAVAILABLE);
                }
            };
        }
        Ok(self.served.as_ref().map_or(&[], |(_, c)| c.as_bytes()))
    }

    /// A meta database failure is logged, and the session goes on.
    async fn catalog_status(&mut self, status: CatalogStatus) {
        let Some(session) = self.authed.as_ref().filter(|s| !s.daemon_id.is_empty()) else {
            return;
        };
        let catalogs = &self.server.catalogs;
        if let Err(e) = catalogs
            .record_status(&session.daemon_id, &session.devices, &status)
            .await
        {
            tracing::warn!("ingest client {}: {e}", self.peer);
        }
    }
}

/// Answer one connection's messages in turn until it closes, or until
/// `reassigned` says its catalogue assignment changed.
async fn serve(
    mut stream: TcpStream,
    peer: &str,
    mut machine: ServerSession,
    gateway: &mut impl Gateway,
    reassigned: &Notify,
) -> Result<(), String> {
    let idle_limit = machine.idle_limit().unwrap_or(Duration::MAX);
    let mut read_buf = [0u8; 65536];

    loop {
        tokio::select! {
            // Read with idle timeout (any traffic counts as keepalive)
            read = tokio::time::timeout(idle_limit, stream.read(&mut read_buf)) => {
                let n = match read {
                    Ok(Ok(0)) => return Ok(()),
                    Ok(Ok(n)) => n,
                    Ok(Err(e)) => return Err(format!("read: {e}")),
                    Err(_) => return Err("idle timeout".into()),
                };
                machine.receive(&read_buf[..n]);
            }
            () = reassigned.notified() => machine.close_reassigned(),
        }

        while let Some(action) = machine.poll() {
            let reply = match action {
                Action::Reply(bytes) => bytes,
                Action::Hello(hello) => {
                    let (status, assignments) = gateway.hello(&hello).await;
                    machine
                        .answer_hello(status, now_us(), &assignments)
                        .expect("one assignment per device, and a HELLO names at most 255")
                }
                Action::HelloRefused(status) => machine
                    .answer_hello(status, now_us(), &[])
                    .expect("no assignments to overflow"),
                Action::Batch(incoming) => {
                    let seq = incoming.batch.seq;
                    let status = gateway.batch(incoming).await;
                    machine.ack(seq, status, 0)
                }
                Action::Nack { seq, status } => machine.ack(seq, status, 0),
                Action::CatalogGet(get) => {
                    machine.answer_catalog(gateway.blob(&get.blob_sha).await)
                }
                Action::CatalogStatus(status) => {
                    gateway.catalog_status(status).await;
                    continue;
                }
                Action::Close(CloseReason::Refused(_)) => return Ok(()),
                Action::Close(CloseReason::Reassigned) => {
                    tracing::info!("ingest client {peer}: catalogue assignment changed; closing");
                    return Ok(());
                }
                Action::Close(reason) => return Err(format!("closed: {reason:?}")),
            };
            stream
                .write_all(&reply)
                .await
                .map_err(|e| format!("write: {e}"))?;
        }
    }
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
        let reassigned = Arc::new(Notify::new());
        let mut conn = Connection {
            server: self,
            peer,
            authed: None,
            reassigned: reassigned.clone(),
            served: None,
        };
        let machine = ServerSession::new(ServerConfig {
            versions: 2..=3,
            max_records: self.config.ingest_max_batch_frames,
            short_batch: ShortBatch::NackSeqZero,
            idle_limit: Some(Duration::from_secs_f64(
                self.config.ingest_keepalive_secs * 3.0,
            )),
        });
        let result = serve(stream, peer, machine, &mut conn, &reassigned).await;
        if let Some(session) = conn.authed {
            self.sessions.close(session.id).await;
        }
        result
    }

    /// The established session and the assignments its HELLO_ACK carries, or
    /// the status that refuses the HELLO.
    async fn handle_hello(
        &self,
        hello: &Hello,
        peer: &str,
        reassigned: Arc<Notify>,
    ) -> Result<(Session, Vec<Assignment>), u8> {
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

        let assignments = if hello.daemon_id.is_empty() {
            Vec::new()
        } else {
            self.daemon_hello(hello, &database, peer).await
        };
        let session_id = self
            .sessions
            .open(
                IngestSessionInfo {
                    peer: peer.to_string(),
                    key_name: info.name,
                    database: database.clone(),
                    protocol_version: hello.version,
                    frames: 0,
                    batches: 0,
                    connected_at: Utc::now(),
                },
                hello,
                reassigned,
            )
            .await;
        tracing::info!(
            "ingest client {peer} authenticated, database '{database}', protocol v{}",
            hello.version
        );
        Ok((
            Session {
                pool,
                id: session_id,
                daemon_id: hello.daemon_id.clone(),
                devices: hello.devices.clone(),
            },
            assignments,
        ))
    }

    /// Record a daemon's devices and read their assignments. The meta
    /// database failing costs the daemon its assignments, not its session:
    /// it keeps the catalogues it has and archives on.
    async fn daemon_hello(&self, hello: &Hello, database: &str, peer: &str) -> Vec<Assignment> {
        let id = &hello.daemon_id;
        let read = async {
            self.catalogs
                .record_hello(id, database, &hello.devices)
                .await?;
            self.catalogs.assignments_for(id, &hello.devices).await
        };
        read.await.unwrap_or_else(|e| {
            tracing::warn!("ingest client {peer} (daemon {id}): {e}; answering no assignments");
            Vec::new()
        })
    }

    /// Write one batch to Postgres, then ACK. The client only treats frames as
    /// delivered once they are durably stored; a DB failure yields ACK_OVERLOADED
    /// so the device caches and retries (no frames are buffered in gateway RAM),
    /// unless no retry could store it (see [`writer::refused_the_rows`]).
    async fn handle_batch(&self, incoming: IncomingBatch, session: &Session) -> u8 {
        let seq = incoming.batch.seq;
        let rows = rows(incoming);
        let count = rows.len() as u64;

        match copy_rows(&session.pool, &rows).await {
            Ok(()) => {
                if let Some(s) = self.sessions.inner.lock().await.get_mut(&session.id) {
                    s.info.frames += count;
                    s.info.batches += 1;
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

fn rows(incoming: IncomingBatch) -> Vec<FrameRow> {
    incoming
        .batch
        .stamped(incoming.time_relative, now_us())
        .map(|(ts_us, r)| {
            FrameRow::new(ts_us as i64, r.kind, r.id_flags, r.flags, r.bus, r.payload)
        })
        .collect()
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
            catalogs: Catalogs::new(dbs.clone(), Sessions::default()),
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

    fn device(bus: u8, name: &str) -> Device {
        Device {
            bus,
            name: name.into(),
        }
    }

    fn daemon_hello(daemon_id: &str, devices: Vec<Device>) -> Hello {
        Hello {
            daemon_id: daemon_id.into(),
            devices,
            ..hello(3, "")
        }
    }

    async fn hello_status(dbs: Databases, database: &str) -> u8 {
        let hello = hello(PROTO_VERSION, database);
        let peer = "192.0.2.10:40000";
        match server(dbs).handle_hello(&hello, peer, Arc::default()).await {
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

    /// The next message, or `None` once the gateway has closed.
    async fn reply(c: &mut TcpStream, buf: &mut Vec<u8>) -> Option<WireFrame> {
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(frame) = take_frame(buf).unwrap() {
                return Some(frame);
            }
            let n = c.read(&mut chunk).await.unwrap();
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    async fn hello_ack(c: &mut TcpStream, hello: &Hello) -> HelloAck {
        c.write_all(&encode_hello(hello).unwrap()).await.unwrap();
        let frame = reply(c, &mut Vec::new())
            .await
            .expect("closed without a HELLO_ACK");
        assert_eq!(frame.mtype, MSG_HELLO_ACK);
        parse_hello_ack(&frame.body).unwrap()
    }

    /// Takes every HELLO and every batch, and holds blobs by their SHA-1.
    #[derive(Default)]
    struct Fake {
        blobs: Vec<Vec<u8>>,
        /// Fired while the batch is being written, as an assignment changing
        /// mid-write would.
        reassigned_mid_batch: Option<Arc<Notify>>,
        statuses: Arc<std::sync::Mutex<Vec<CatalogStatus>>>,
    }

    impl Gateway for Fake {
        async fn hello(&mut self, _: &Hello) -> (u8, Vec<Assignment>) {
            (HELLO_OK, Vec::new())
        }

        async fn batch(&mut self, _: IncomingBatch) -> u8 {
            if let Some(reassigned) = &self.reassigned_mid_batch {
                reassigned.notify_one();
            }
            ACK_OK
        }

        async fn blob(&mut self, sha: &[u8; 20]) -> Result<&[u8], u8> {
            let found = self
                .blobs
                .iter()
                .find(|b| wiretap_model::blob_sha1(b) == *sha);
            found.map(Vec::as_slice).ok_or(CATALOG_UNKNOWN)
        }

        async fn catalog_status(&mut self, status: CatalogStatus) {
            self.statuses.lock().unwrap().push(status);
        }
    }

    /// A client connected to [`serve`] over `fake`, and what wakes it.
    async fn serving(mut fake: Fake) -> (TcpStream, Arc<Notify>) {
        let reassigned = fake.reassigned_mid_batch.clone().unwrap_or_default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let notify = reassigned.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let machine = ServerSession::new(ServerConfig {
                versions: 2..=3,
                max_records: MAX_BATCH_RECORDS,
                short_batch: ShortBatch::NackSeqZero,
                idle_limit: None,
            });
            let _ = serve(stream, "test", machine, &mut fake, &notify).await;
        });
        (TcpStream::connect(addr).await.unwrap(), reassigned)
    }

    fn batch(seq: u32) -> Vec<u8> {
        let mut records = Vec::new();
        encode_record_into(&mut records, 0, 0, RecordKind::Can, 0, 0, 0x123, &[1]);
        encode_batch(seq, 1_700_000_000_000_000, 1, &records)
    }

    /// A `CLOSE` naming the reassignment, and then the end of the stream.
    async fn closed_as_reassigned(c: &mut TcpStream, buf: &mut Vec<u8>) {
        let close = reply(c, buf).await.expect("a CLOSE before the end");
        assert_eq!(close.mtype, MSG_CLOSE);
        assert_eq!(parse_close(&close.body).unwrap().reason, CLOSE_REASSIGNED);
        assert!(reply(c, buf).await.is_none(), "still open");
    }

    #[tokio::test]
    async fn a_reassignment_closes_the_session() {
        let (mut c, reassigned) = serving(Fake::default()).await;
        let hello = daemon_hello("bench", vec![device(0, "can0")]);
        assert_eq!(hello_ack(&mut c, &hello).await.status, HELLO_OK);
        reassigned.notify_one();
        closed_as_reassigned(&mut c, &mut Vec::new()).await;
    }

    #[tokio::test]
    async fn a_reassignment_mid_batch_is_closed_on_after_the_ack() {
        let reassigned = Arc::new(Notify::new());
        let (mut c, _) = serving(Fake {
            reassigned_mid_batch: Some(reassigned),
            ..Fake::default()
        })
        .await;
        hello_ack(&mut c, &daemon_hello("bench", vec![device(0, "can0")])).await;
        c.write_all(&batch(7)).await.unwrap();
        let mut buf = Vec::new();
        let ack = reply(&mut c, &mut buf).await.expect("the ACK it was owed");
        assert_eq!(parse_ack(&ack.body).unwrap().seq, 7);
        closed_as_reassigned(&mut c, &mut buf).await;
    }

    /// CRLF, and longer than one chunk.
    #[tokio::test]
    async fn a_blob_is_served_a_chunk_at_a_time_byte_for_byte() {
        let blob = "[meta]\r\nname = \"crlf\"\r\n# padding\r\n".repeat(3_000);
        assert!(blob.len() > MAX_CATALOG_CHUNK);
        let sha = wiretap_model::blob_sha1(blob.as_bytes());
        let (mut c, _) = serving(Fake {
            blobs: vec![blob.clone().into_bytes()],
            ..Fake::default()
        })
        .await;
        hello_ack(&mut c, &daemon_hello("bench", vec![device(0, "can0")])).await;

        let mut buf = Vec::new();
        let mut got = Vec::new();
        let mut next = Some(CatalogGet {
            blob_sha: sha,
            offset: 0,
        });
        while let Some(get) = next {
            c.write_all(&encode_catalog_get(&get)).await.unwrap();
            let frame = reply(&mut c, &mut buf).await.unwrap();
            let chunk = parse_catalog(&frame.body).unwrap();
            assert_eq!(chunk.status, CATALOG_OK);
            got.extend_from_slice(&chunk.data);
            next = chunk.next();
        }
        assert_eq!(got, blob.as_bytes());

        let unknown = CatalogGet {
            blob_sha: [0; 20],
            offset: 0,
        };
        c.write_all(&encode_catalog_get(&unknown)).await.unwrap();
        let frame = reply(&mut c, &mut buf).await.unwrap();
        assert_eq!(parse_catalog(&frame.body).unwrap().status, CATALOG_UNKNOWN);
    }

    #[tokio::test]
    async fn a_catalog_status_is_taken_without_a_reply() {
        let fake = Fake::default();
        let statuses = Arc::clone(&fake.statuses);
        let (mut c, _) = serving(fake).await;
        hello_ack(&mut c, &daemon_hello("bench", vec![device(0, "can0")])).await;
        let status = CatalogStatus {
            entries: vec![CatalogStatusEntry {
                bus: 0,
                active: ActiveCatalog::Assigned([7; 20]),
                refused: None,
            }],
        };
        c.write_all(&encode_catalog_status(&status).unwrap())
            .await
            .unwrap();
        c.write_all(&encode_message(MSG_PING, b"")).await.unwrap();
        let next = reply(&mut c, &mut Vec::new()).await.unwrap();
        assert_eq!(next.mtype, MSG_PONG, "the PING's, not one for the status");
        assert_eq!(*statuses.lock().unwrap(), [status]);
    }

    fn session_info() -> IngestSessionInfo {
        IngestSessionInfo {
            peer: "192.0.2.10:40000".into(),
            key_name: "bench".into(),
            database: "wiretap".into(),
            protocol_version: 3,
            frames: 0,
            batches: 0,
            connected_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn a_reassignment_closes_only_the_sessions_that_carry_the_interface() {
        let sessions = Sessions::default();
        let (framed, raw, other) = (Arc::default(), Arc::default(), Arc::default());
        let site = daemon_hello("bench", vec![device(0, "can0"), device(1, "/dev/ttyUSB0")]);
        let rs485_raw = daemon_hello("bench", vec![device(1, "/dev/ttyUSB1")]);
        let elsewhere = daemon_hello("other", vec![device(0, "can0")]);
        for (hello, notify) in [(&site, &framed), (&rs485_raw, &raw), (&elsewhere, &other)] {
            sessions
                .open(session_info(), hello, Arc::clone(notify))
                .await;
        }

        assert_eq!(sessions.reassign("bench", "can0").await, 1);
        let woken = |n: &Arc<Notify>| {
            let n = n.clone();
            async move {
                tokio::time::timeout(Duration::from_millis(50), n.notified())
                    .await
                    .is_ok()
            }
        };
        assert!(woken(&framed).await);
        assert!(!woken(&raw).await, "its HELLO did not name can0");
        assert!(!woken(&other).await, "another daemon's can0");
        assert_eq!(sessions.reassign("bench", "can9").await, 0);
    }

    #[tokio::test]
    async fn a_meta_database_failure_at_hello_answers_no_assignments() {
        let server = server(unreachable_databases(true));
        let hello = daemon_hello("bench", vec![device(0, "can0")]);
        let assignments = server.daemon_hello(&hello, "wiretap", "test").await;
        assert!(assignments.is_empty());
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

    /// Taken to the database like any other batch, as serial rows: here the
    /// database is down, so it is refused as overloaded rather than malformed.
    #[tokio::test]
    async fn a_raw_serial_batch_is_written_as_serial_rows() {
        let row = &rows(incoming(RecordKind::RawSerial, raw_serial_id(1)))[0];
        assert_eq!(
            (row.protocol, row.id, row.dlc),
            (wiretap_model::Protocol::Serial, 0, 1)
        );

        let server = server(unreachable_databases(true));
        let pool = Pool::builder(deadpool_postgres::Manager::new(
            server.config.pg_dsn("wiretap").parse().unwrap(),
            tokio_postgres::NoTls,
        ))
        .build()
        .unwrap();
        let session = Session {
            pool,
            id: 0,
            daemon_id: String::new(),
            devices: Vec::new(),
        };
        let raw = incoming(RecordKind::RawSerial, raw_serial_id(1));
        assert_eq!(server.handle_batch(raw, &session).await, ACK_OVERLOADED);
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
        let info = IngestSessionInfo {
            protocol_version: 2,
            ..session_info()
        };
        sessions.open(info, &hello(2, ""), Arc::default()).await;
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
