//! Forwarding frames to a WireTAP gateway over the binary ingest protocol.
//!
//! The only [`BatchSink`] there is. It speaks the client half of
//! `wiretap_protocol::ingest`, whose server half the gateway parses — one codec,
//! both ends, so a change to the wire format cannot land on one side only.
//!
//! Every batch is acknowledged **after** the gateway has written it, so a slow
//! or failing archive is felt here as a failed write rather than an accepted
//! one, and the batcher puts the frames on disk instead of losing them. That is
//! the whole reason this is a stream protocol with ACKs rather than a POST.
//!
//! Each connection starts with a v3 `HELLO` naming this daemon and the devices
//! the database carries, and pulls any catalogue the gateway assigned before
//! the first batch.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{info, warn};
use wiretap_model::{blob_sha1_hex, Sample, Secret};
use wiretap_protocol::ingest as proto;

use crate::archive::{BatchSink, SinkError, SinkResult, WriteError};
use crate::cache::{FrameCache, SqliteCache};
use crate::catalogues::{Catalogues, Update};
use crate::settings::{Forward, LineCatalogue};
use crate::wire;

/// How long any single read or write may take, matching the Python's socket
/// timeout. Long enough for a gateway writing a batch to PostgreSQL over a
/// slow link, short enough that a black hole is noticed and cached around.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// One read from the gateway. Replies are tens of bytes; this is sized for the
/// syscall, not the message.
const READ_BUF: usize = 4096;

pub struct ForwardSink {
    host: String,
    port: u16,
    api_key: Secret,
    database: String,
    daemon_id: String,
    devices: Vec<proto::Device>,
    /// 3, until a gateway that predates it names 2, and then 2 for good,
    /// unless raw serial comes here, which v2 cannot carry.
    version: u8,
    raw_serial: bool,
    catalogues: Arc<Catalogues>,
    conn: Option<Connection>,
    /// Wraps with the protocol's `u32`, as the Python's `& 0xFFFFFFFF` did. It
    /// identifies an ACK against its batch, so it only has to be unique among
    /// those in flight — and there is only ever one.
    seq: u32,
    /// Reused across batches: a batch is bounded at `MAX_BODY` bytes, and this
    /// runs for every batch for the life of the process.
    records: Vec<u8>,
    dead_letter: PathBuf,
}

/// A connection and whatever of a reply has arrived so far.
struct Connection {
    stream: TcpStream,
    rx: Vec<u8>,
}

impl ForwardSink {
    pub fn new(forward: &Forward, catalogues: Arc<Catalogues>) -> Self {
        Self {
            host: forward.host.clone(),
            port: forward.port,
            api_key: forward.api_key.clone(),
            database: forward.database.clone(),
            daemon_id: forward.daemon_id.clone(),
            devices: forward.devices.clone(),
            version: proto::PROTO_VERSION,
            raw_serial: forward.raw_serial,
            catalogues,
            conn: None,
            seq: 0,
            records: Vec::new(),
            dead_letter: forward.batching.dead_letter_path(),
        }
    }

    fn connection(&mut self) -> Result<&mut Connection, SinkError> {
        self.conn
            .as_mut()
            .ok_or_else(|| SinkError("forward: not connected".into()))
    }

    /// Send one `BATCH` and wait for its acknowledgement.
    ///
    /// `chunk` and `base_ts_us` are one [`proto::fit_batch`]'s: a chunk that fits
    /// one batch, and its earliest frame's stamp.
    async fn send_chunk(&mut self, chunk: &[Arc<Sample>], base_ts_us: u64) -> SinkResult {
        // Absolute timestamps: the `[forward]` client does not set
        // `TIME_RELATIVE`, so the gateway takes these at face value rather than
        // re-basing them on its own clock.
        self.records.clear();
        for f in chunk {
            wire::encode_into(&mut self.records, base_ts_us, f);
        }
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        let message = proto::encode_batch(seq, base_ts_us, chunk.len() as u16, &self.records);

        let conn = self.connection()?;
        conn.send(&message).await?;
        let frame = conn.recv().await?;
        if frame.mtype != proto::MSG_ACK {
            return Err(SinkError("forward: malformed ACK".into()));
        }
        let ack = proto::parse_ack(&frame.body)
            .map_err(|_| SinkError("forward: malformed ACK".into()))?;
        // A gateway that cannot read a batch's seq refuses it as seq 0.
        let unread_seq = ack.seq == 0 && ack.status == proto::ACK_MALFORMED;
        if ack.seq != seq && !unread_seq {
            return Err(SinkError(format!(
                "forward: ACK for seq={} while awaiting seq={seq}",
                ack.seq
            )));
        }
        match ack.status {
            proto::ACK_OK => Ok(()),
            // Back-pressure, and the reason this protocol has an ACK at all:
            // failing here caches the frames rather than dropping them into a
            // gateway that has said it cannot take them.
            proto::ACK_OVERLOADED => Err(SinkError("forward: gateway overloaded".into())),
            proto::ACK_MALFORMED => self.quarantine(chunk, seq),
            status => Err(SinkError(format!(
                "forward: batch nacked (seq={} status={status})",
                ack.seq
            ))),
        }
    }

    /// Keep a batch the gateway will never take, so the link can move past it.
    /// If it cannot be kept, fail as any other refusal does and let the cache
    /// hold it: a batch is never dropped.
    fn quarantine(&self, chunk: &[Arc<Sample>], seq: u32) -> SinkResult {
        let path = &self.dead_letter;
        tokio::task::block_in_place(|| SqliteCache::open(path, u64::MAX)?.append(chunk))
            .map_err(|e| {
                SinkError(format!(
                    "forward: batch refused as malformed (seq={seq}) and not quarantined to {}: {e}",
                    path.display()
                ))
            })?;
        warn!(
            "gateway refused batch seq={seq} of {} frames as malformed; quarantined to {}",
            chunk.len(),
            path.display()
        );
        Ok(())
    }

    fn label(&self) -> &str {
        Forward::label_of(&self.database)
    }

    fn hello(&self) -> proto::Hello {
        let v2 = proto::Hello::v2(self.api_key.expose().as_bytes(), &self.database, false);
        match self.version {
            2 => v2,
            version => proto::Hello {
                version,
                daemon_id: self.daemon_id.clone(),
                devices: self.devices.clone(),
                ..v2
            },
        }
    }

    /// A new connection, and what the gateway said to its `HELLO`.
    async fn handshake(&self) -> Result<(Connection, proto::HelloAck), SinkError> {
        let stream = with_timeout(
            "connect",
            TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await?;
        let mut conn = Connection {
            stream,
            rx: Vec::new(),
        };
        let hello = proto::encode_hello(&self.hello())
            .map_err(|e| SinkError(format!("forward: cannot send HELLO: {e}")))?;
        conn.send(&hello).await?;
        let frame = conn.recv().await?;
        if frame.mtype != proto::MSG_HELLO_ACK {
            return Err(SinkError("forward: no HELLO_ACK from gateway".into()));
        }
        let ack =
            proto::parse_hello_ack(&frame.body).map_err(|e| SinkError(format!("forward: {e}")))?;
        Ok((conn, ack))
    }

    /// A gateway that predates catalogue assignment, where nothing needs v3.
    fn may_fall_back(&self, ack: &proto::HelloAck) -> bool {
        ack.status == proto::HELLO_BAD_VERSION
            && ack.accepted_version == 2
            && self.version == 3
            && !self.raw_serial
    }

    /// Bring the lines this database owns up to what the gateway assigned
    /// them. A catalogue that cannot be had is logged and leaves its line as
    /// it was: it never costs the session.
    async fn pull(&self, conn: &mut Connection, assignments: &[proto::Assignment]) {
        let mut updates = Vec::new();
        for interface in self.catalogues.owned_by(&self.database) {
            let device = self.devices.iter().find(|d| d.name == interface);
            let assigned = device.and_then(|d| assignments.iter().find(|a| a.bus == d.bus));
            let update = match assigned {
                None => Update::Cleared,
                Some(a) => match self.catalogue(conn, &a.blob_sha).await {
                    Ok(catalogue) => Update::Assigned {
                        sha: blob_sha1_hex(&a.blob_sha),
                        catalogue,
                    },
                    Err(why) => {
                        warn!(
                            "{interface}: cannot take the gateway's catalogue {}: {why}; \
                             keeping what it frames with",
                            blob_sha1_hex(&a.blob_sha)
                        );
                        continue;
                    }
                },
            };
            updates.push((interface.to_owned(), update));
        }
        self.catalogues.apply(updates);
    }

    async fn catalogue(
        &self,
        conn: &mut Connection,
        sha: &[u8; 20],
    ) -> Result<LineCatalogue, String> {
        let hex = blob_sha1_hex(sha);
        if let Ok(cached) = self.catalogues.cached(&hex) {
            return Ok(cached);
        }
        let blob = conn.fetch(sha).await?;
        self.catalogues.store(&hex, &blob)
    }
}

/// Bound any one exchange with the gateway, and name it if it fails.
///
/// The timeout is what stops a gateway that has stopped answering — a NAT that
/// dropped the flow, a host that was powered off — from wedging the batcher
/// instead of being cached around.
async fn with_timeout<T>(
    what: &str,
    op: impl std::future::Future<Output = std::io::Result<T>>,
) -> Result<T, SinkError> {
    match tokio::time::timeout(IO_TIMEOUT, op).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(SinkError(format!("forward: {what} failed: {e}"))),
        Err(_) => Err(SinkError(format!("forward: timed out on {what}"))),
    }
}

impl Connection {
    async fn send(&mut self, bytes: &[u8]) -> SinkResult {
        with_timeout("write", self.stream.write_all(bytes)).await
    }

    /// The next reply that is not a `CATALOG`: one arriving now answers a
    /// fetch that gave up waiting for it.
    async fn recv(&mut self) -> Result<proto::WireFrame, SinkError> {
        loop {
            let frame = self.recv_any().await?;
            if frame.mtype != proto::MSG_CATALOG {
                return Ok(frame);
            }
        }
    }

    /// The `CATALOG` answering `get`.
    async fn recv_catalog(&mut self, get: &proto::CatalogGet) -> Result<proto::Catalog, String> {
        loop {
            let frame = self.recv_any().await.map_err(|e| e.0)?;
            if frame.mtype != proto::MSG_CATALOG {
                return Err(format!("a reply of type {:#04x}", frame.mtype));
            }
            let chunk = proto::parse_catalog(&frame.body)?;
            if (chunk.blob_sha, chunk.offset) == (get.blob_sha, get.offset) {
                return Ok(chunk);
            }
        }
    }

    /// A whole blob, a chunk at a time.
    async fn fetch(&mut self, sha: &[u8; 20]) -> Result<Vec<u8>, String> {
        let mut blob = Vec::new();
        let mut next = Some(proto::CatalogGet {
            blob_sha: *sha,
            offset: 0,
        });
        while let Some(get) = next {
            self.send(&proto::encode_catalog_get(&get))
                .await
                .map_err(|e| e.0)?;
            let chunk = self.recv_catalog(&get).await?;
            match chunk.status {
                proto::CATALOG_OK => {}
                proto::CATALOG_UNKNOWN => return Err("the gateway does not have it".into()),
                proto::CATALOG_UNAVAILABLE => return Err("the gateway cannot read it now".into()),
                status => return Err(format!("CATALOG status={status}")),
            }
            blob.extend_from_slice(&chunk.data);
            next = chunk.next();
        }
        Ok(blob)
    }

    /// Read until one complete, intact message has arrived.
    async fn recv_any(&mut self) -> Result<proto::WireFrame, SinkError> {
        loop {
            match proto::take_frame(&mut self.rx) {
                Err(e) => return Err(SinkError(format!("forward: {e}"))),
                Ok(Some(frame)) if !frame.crc_ok => {
                    return Err(SinkError("forward: bad CRC from gateway".into()));
                }
                Ok(Some(frame)) => return Ok(frame),
                Ok(None) => {}
            }

            let mut buf = [0u8; READ_BUF];
            let n = with_timeout("read", self.stream.read(&mut buf)).await?;
            if n == 0 {
                return Err(SinkError("forward: gateway closed connection".into()));
            }
            self.rx.extend_from_slice(&buf[..n]);
        }
    }
}

impl BatchSink for ForwardSink {
    async fn connect(&mut self) -> SinkResult {
        // Handshaken on a local and stored only once it has worked, so no
        // failure path has to remember to undo it.
        let (mut conn, mut ack) = self.handshake().await?;
        if self.may_fall_back(&ack) {
            info!(
                "the gateway at {}:{} predates catalogue assignment; forwarding db={} with \
                 protocol v2 until this server restarts",
                self.host,
                self.port,
                self.label()
            );
            self.version = 2;
            (conn, ack) = self.handshake().await?;
        }
        if ack.status != proto::HELLO_OK {
            // The version case names both sides: it is the one an operator
            // meets mid-upgrade, and "status=2" does not say which end.
            let why = match ack.status {
                proto::HELLO_BAD_VERSION => format!(
                    ": this server speaks protocol v{}, the gateway v{}; upgrade the gateway first",
                    self.version, ack.accepted_version
                ),
                proto::HELLO_UNAVAILABLE => ": the gateway's database is not available yet".into(),
                _ => String::new(),
            };
            return Err(SinkError(format!(
                "forward: HELLO rejected (status={}){why}",
                ack.status
            )));
        }
        if self.version == 3 {
            self.pull(&mut conn, &ack.assignments).await;
        }
        self.conn = Some(conn);

        info!(
            "connected (forward -> {}:{} db={})",
            self.host,
            self.port,
            self.label()
        );
        Ok(())
    }

    async fn write_batch(&mut self, batch: &[Arc<Sample>]) -> Result<(), WriteError> {
        let mut delivered = 0;
        // A disk cache drain reads `ORDER BY id` across a whole outage, so a
        // chunk on a quiet bus can outspan a `u32` of microseconds, and two bus
        // readers mean its head is not always its earliest frame.
        while delivered < batch.len() {
            let rest = &batch[delivered..];
            let fit = proto::fit_batch(rest.iter().map(|f| wire::fit_input(f)));
            self.send_chunk(&rest[..fit.len], fit.base_ts_us)
                .await
                .map_err(|cause| WriteError { delivered, cause })?;
            delivered += fit.len;
        }
        Ok(())
    }

    /// An idle `PING`, so the gateway's keepalive timer does not drop a server
    /// that is simply on a quiet bus.
    async fn keep_alive(&mut self) -> SinkResult {
        let ping = proto::encode_message(proto::MSG_PING, b"");
        let conn = self.connection()?;
        conn.send(&ping).await?;
        if conn.recv().await?.mtype != proto::MSG_PONG {
            return Err(SinkError("forward: unexpected idle reply".into()));
        }
        Ok(())
    }

    async fn close(&mut self) {
        if let Some(mut conn) = self.conn.take() {
            // Best effort: the far end may already be gone, which is usually
            // why this is being called.
            let _ = conn.stream.shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogues::tests::{sha_of, TempDir, CATALOGUE};
    use crate::catalogues::Rules;
    use crate::settings::Batching;
    use std::ops::RangeInclusive;
    use tokio::net::TcpListener;
    use wiretap_model::{blob_sha1, CanSample, Direction, ModbusSample, SourceId};

    /// What every connection to the fake gateway saw.
    #[derive(Debug, Default)]
    struct Seen {
        hellos: Vec<proto::Hello>,
        catalog_gets: usize,
        batches: Vec<proto::Batch>,
        pings: usize,
    }

    impl Seen {
        fn versions(&self) -> Vec<u8> {
            self.hellos.iter().map(|h| h.version).collect()
        }
    }

    /// How the fake gateway should answer.
    #[derive(Clone)]
    struct Script {
        hello_status: u8,
        /// A HELLO for any other is refused naming the newest, and closed.
        versions: RangeInclusive<u8>,
        /// Served one after another; the gateway's task ends with the last.
        connections: usize,
        assignments: Vec<proto::Assignment>,
        /// Each served under the SHA-1 it is filed with, right or not.
        blobs: Vec<([u8; 20], Vec<u8>)>,
        ack_status: u8,
        /// Batches before this one are ACKed OK, whatever `ack_status` says.
        nack_from: usize,
        /// Hang up rather than answering the first batch.
        close_on_batch: bool,
    }

    impl Default for Script {
        fn default() -> Self {
            Self {
                hello_status: proto::HELLO_OK,
                versions: 2..=3,
                connections: 1,
                assignments: Vec::new(),
                blobs: Vec::new(),
                ack_status: proto::ACK_OK,
                nack_from: 0,
                close_on_batch: false,
            }
        }
    }

    impl Script {
        /// The gateway assigns `blob` to `bus`, and serves it.
        fn assigning(bus: u8, blob: &[u8]) -> Self {
            let blob_sha = blob_sha1(blob);
            Self {
                assignments: vec![proto::Assignment { bus, blob_sha }],
                blobs: vec![(blob_sha, blob.to_vec())],
                ..Self::default()
            }
        }
    }

    /// A gateway that speaks the server half of the protocol from the same
    /// crate the client half comes from — so these tests exercise the real
    /// parser, not a restatement of it.
    async fn fake_gateway(script: Script) -> (u16, tokio::task::JoinHandle<Seen>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let mut seen = Seen::default();
            for _ in 0..script.connections {
                let (mut stream, _) = listener.accept().await.unwrap();
                answer(&mut stream, &script, &mut seen).await;
            }
            seen
        });
        (port, task)
    }

    /// One connection, until either end closes it.
    async fn answer(stream: &mut TcpStream, script: &Script, seen: &mut Seen) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let frame = loop {
                match proto::take_frame(&mut buf) {
                    Ok(Some(f)) => break Some(f),
                    Ok(None) => {}
                    Err(_) => break None,
                }
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            };
            let Some(frame) = frame else { return };
            assert!(frame.crc_ok, "the client sent a bad CRC");

            let reply = match frame.mtype {
                proto::MSG_HELLO => {
                    let hello = proto::parse_hello(&frame.body).expect("a valid HELLO");
                    let version = hello.version;
                    seen.hellos.push(hello);
                    if !script.versions.contains(&version) {
                        let newest = *script.versions.end();
                        let refusal =
                            proto::encode_hello_ack(proto::HELLO_BAD_VERSION, newest, 1_234, &[]);
                        let _ = stream.write_all(&refusal.unwrap()).await;
                        return;
                    }
                    let assignments = &script.assignments;
                    proto::encode_hello_ack(script.hello_status, version, 1_234, assignments)
                        .unwrap()
                }
                proto::MSG_CATALOG_GET => {
                    seen.catalog_gets += 1;
                    let get = proto::parse_catalog_get(&frame.body).unwrap();
                    let blob = script.blobs.iter().find(|(sha, _)| *sha == get.blob_sha);
                    let blob = blob
                        .map(|(_, b)| b.as_slice())
                        .ok_or(proto::CATALOG_UNKNOWN);
                    proto::encode_catalog(&get, blob)
                }
                proto::MSG_BATCH => {
                    if script.close_on_batch {
                        return;
                    }
                    let batch = proto::parse_batch(&frame.body, proto::MAX_BATCH_RECORDS)
                        .expect("carries a seq")
                        .expect("well formed");
                    let seq = batch.seq;
                    seen.batches.push(batch);
                    let status = if seen.batches.len() > script.nack_from {
                        script.ack_status
                    } else {
                        proto::ACK_OK
                    };
                    proto::encode_ack(seq, status, 0)
                }
                proto::MSG_PING => {
                    seen.pings += 1;
                    proto::encode_message(proto::MSG_PONG, b"")
                }
                other => panic!("unexpected message type {other:#x}"),
            };
            if stream.write_all(&reply).await.is_err() {
                return;
            }
        }
    }

    fn sink(port: u16, database: &str) -> ForwardSink {
        ForwardSink::new(&forward(port, database), Arc::default())
    }

    fn forward(port: u16, database: &str) -> Forward {
        Forward {
            host: "127.0.0.1".into(),
            port,
            api_key: Secret::new("sekrit"),
            database: database.into(),
            batching: Batching {
                size: 4,
                flush_interval: 0.02,
                queue_size: 10,
                cache_path: PathBuf::from("unused"),
                cache_max_mb: 1,
                queue_flush_pct: 100,
                cache_origin: None,
                legacy_cache_path: None,
            },
            daemon_id: "bench".into(),
            devices: Vec::new(),
            raw_serial: false,
        }
    }

    fn sink_caching_at(port: u16, cache_path: PathBuf) -> ForwardSink {
        let mut f = forward(port, "");
        f.batching.cache_path = cache_path;
        ForwardSink::new(&f, Arc::default())
    }

    const LINE: &str = "/dev/ttyUSB0";

    /// A sink for `database` whose HELLO names `LINE` on bus 1, with the
    /// catalogues it shares.
    fn line_sink(port: u16, database: &str, catalogues: &Arc<Catalogues>) -> ForwardSink {
        let mut f = forward(port, database);
        f.devices = vec![proto::Device {
            bus: 1,
            name: LINE.into(),
        }];
        ForwardSink::new(&f, Arc::clone(catalogues))
    }

    fn line_catalogues(dir: &TempDir) -> Arc<Catalogues> {
        Catalogues::new(Some(dir.0.clone()), [(LINE.into(), String::new(), None)])
    }

    fn remembered(dir: &TempDir) -> serde_json::Value {
        let json = std::fs::read(dir.0.join("assignments.json")).unwrap();
        serde_json::from_slice(&json).unwrap()
    }

    fn can(ts_us: i64, arb_id: u32) -> CanSample {
        CanSample {
            ts_us,
            arb_id,
            extended: false,
            is_fd: false,
            data: vec![1, 2, 3],
            bus: SourceId(0),
            dir: Direction::Rx,
        }
    }

    fn sample(ts_us: i64, arb_id: u32) -> Arc<Sample> {
        Arc::new(Sample::Can(can(ts_us, arb_id)))
    }

    /// The longest message the wire allows.
    fn modbus(ts_us: i64) -> Arc<Sample> {
        Arc::new(Sample::Modbus(ModbusSample {
            ts_us,
            bus: SourceId(2),
            unit: 1,
            func: 0x04,
            crc_valid: true,
            raw: vec![0x01; proto::RecordKind::Modbus.max_payload()],
        }))
    }

    #[tokio::test]
    async fn a_hello_carries_the_key_and_the_database() {
        let (port, gateway) = fake_gateway(Script::default()).await;
        let mut s = sink(port, "vehicle_1");
        s.connect().await.expect("the gateway accepted it");
        s.close().await;

        let seen = gateway.await.unwrap();
        assert_eq!(seen.hellos[0].token, b"sekrit");
        assert_eq!(seen.hellos[0].database, "vehicle_1");
    }

    #[tokio::test]
    async fn a_hello_is_v3_and_names_the_daemon_and_its_devices() {
        let (port, gateway) = fake_gateway(Script::default()).await;
        let mut s = line_sink(port, "site", &Arc::default());
        s.connect().await.expect("the gateway accepted it");
        s.close().await;

        let hello = &gateway.await.unwrap().hellos[0];
        assert_eq!(hello.version, 3);
        assert_eq!(hello.daemon_id, "bench");
        assert_eq!(
            hello.devices,
            [proto::Device {
                bus: 1,
                name: LINE.into()
            }]
        );
    }

    #[tokio::test]
    async fn a_gateway_that_predates_v3_is_forwarded_to_with_v2_from_then_on() {
        let (port, gateway) = fake_gateway(Script {
            versions: 2..=2,
            connections: 3,
            ..Script::default()
        })
        .await;
        let mut s = sink(port, "");
        s.connect().await.expect("taken with v2");
        s.close().await;
        s.connect().await.expect("taken with v2 again");
        s.close().await;
        assert_eq!(gateway.await.unwrap().versions(), [3, 2, 2]);
    }

    /// The refusal an un-upgraded gateway gives has to say which end is
    /// behind, because the operator reading it is mid-upgrade.
    #[tokio::test]
    async fn a_link_carrying_raw_serial_does_not_fall_back_to_v2() {
        let (port, gateway) = fake_gateway(Script {
            versions: 2..=2,
            ..Script::default()
        })
        .await;
        let mut f = forward(port, "");
        f.raw_serial = true;
        let err = ForwardSink::new(&f, Arc::default())
            .connect()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("status=2"), "{err}");
        assert!(err.contains("speaks protocol v3, the gateway v2"), "{err}");
        assert!(err.contains("upgrade the gateway first"), "{err}");
        assert_eq!(gateway.await.unwrap().versions(), [3]);
    }

    /// CRLF, and longer than a chunk.
    fn long_crlf_catalogue() -> Vec<u8> {
        let padding = "# a vendor's notes, kept as they were written\r\n".repeat(2_000);
        let text = format!("{}{padding}", CATALOGUE.replace('\n', "\r\n"));
        assert!(text.len() > proto::MAX_CATALOG_CHUNK);
        text.into_bytes()
    }

    #[tokio::test]
    async fn an_assigned_catalogue_is_pulled_checked_and_cached_before_any_batch() {
        let dir = TempDir::new("pull");
        let catalogues = line_catalogues(&dir);
        let blob = long_crlf_catalogue();
        let sha = sha_of(&blob);
        let (port, gateway) = fake_gateway(Script {
            connections: 2,
            ..Script::assigning(1, &blob)
        })
        .await;
        let mut s = line_sink(port, "", &catalogues);
        s.connect().await.expect("connected");
        let Some(Rules::Gateway { sha: framing, .. }) = catalogues.effective(LINE) else {
            panic!("not the gateway's catalogue");
        };
        assert_eq!(framing, sha);
        let cached = dir.0.join("catalogs").join(format!("{sha}.toml"));
        assert_eq!(std::fs::read(cached).unwrap(), blob, "byte for byte");
        assert_eq!(remembered(&dir), serde_json::json!({ LINE: sha }));
        s.write_batch(&[sample(1, 1)]).await.expect("acknowledged");
        s.close().await;
        s.connect().await.expect("connected again");
        s.close().await;

        let seen = gateway.await.unwrap();
        assert_eq!(seen.catalog_gets, 2, "two chunks, then the cache");
    }

    #[tokio::test]
    async fn a_blob_that_does_not_hash_to_its_name_is_refused() {
        let dir = TempDir::new("misnamed");
        let catalogues = line_catalogues(&dir);
        let claimed = blob_sha1(b"another catalogue");
        let (port, gateway) = fake_gateway(Script {
            assignments: vec![proto::Assignment {
                bus: 1,
                blob_sha: claimed,
            }],
            blobs: vec![(claimed, CATALOGUE.as_bytes().to_vec())],
            ..Script::default()
        })
        .await;
        let mut s = line_sink(port, "", &catalogues);
        s.connect().await.expect("the session goes on");
        assert_eq!(catalogues.effective(LINE), Some(Rules::None));
        assert!(!dir
            .0
            .join("catalogs")
            .join(format!("{}.toml", blob_sha1_hex(&claimed)))
            .exists());
        s.write_batch(&[sample(1, 1)]).await.expect("acknowledged");
        s.close().await;
        let _ = gateway.await;
    }

    #[tokio::test]
    async fn a_catalogue_the_gateway_does_not_have_leaves_the_line_as_it_was() {
        let dir = TempDir::new("unknown");
        let catalogues = line_catalogues(&dir);
        let (port, gateway) = fake_gateway(Script::assigning(1, CATALOGUE.as_bytes())).await;
        line_sink(port, "", &catalogues).connect().await.unwrap();
        let _ = gateway.await;
        let before = catalogues.effective(LINE);

        let (port, gateway) = fake_gateway(Script {
            blobs: Vec::new(),
            ..Script::assigning(1, b"[meta]\nname = \"unpublished\"\n")
        })
        .await;
        let mut s = line_sink(port, "", &catalogues);
        s.connect().await.expect("the session goes on");
        s.close().await;
        assert_eq!(gateway.await.unwrap().catalog_gets, 1);
        assert_eq!(catalogues.effective(LINE), before);
        let sha = sha_of(CATALOGUE.as_bytes());
        assert_eq!(remembered(&dir), serde_json::json!({ LINE: sha }));
    }

    /// Two databases, each with one line: each session keeps its own line's
    /// assignment in the file they share, and clears only that.
    #[tokio::test]
    async fn each_session_keeps_only_its_own_lines_assignments() {
        let dir = TempDir::new("merge");
        let catalogues = Catalogues::new(
            Some(dir.0.clone()),
            [
                (LINE.into(), "site".into(), None),
                ("/dev/ttyUSB1".into(), "rs485".into(), None),
            ],
        );
        let mut rs485 = forward(0, "rs485");
        rs485.devices = vec![proto::Device {
            bus: 2,
            name: "/dev/ttyUSB1".into(),
        }];
        let both = Script {
            assignments: vec![
                proto::Assignment {
                    bus: 1,
                    blob_sha: blob_sha1(CATALOGUE.as_bytes()),
                },
                proto::Assignment {
                    bus: 2,
                    blob_sha: blob_sha1(CATALOGUE.as_bytes()),
                },
            ],
            ..Script::assigning(1, CATALOGUE.as_bytes())
        };

        let (port, site_gateway) = fake_gateway(both.clone()).await;
        line_sink(port, "site", &catalogues)
            .connect()
            .await
            .unwrap();
        let (port, rs485_gateway) = fake_gateway(both).await;
        rs485.port = port;
        ForwardSink::new(&rs485, Arc::clone(&catalogues))
            .connect()
            .await
            .unwrap();
        let (_, _) = (site_gateway.await, rs485_gateway.await);
        let sha = sha_of(CATALOGUE.as_bytes());
        assert_eq!(
            remembered(&dir),
            serde_json::json!({ LINE: sha, "/dev/ttyUSB1": sha })
        );

        let (port, cleared) = fake_gateway(Script::default()).await;
        line_sink(port, "site", &catalogues)
            .connect()
            .await
            .unwrap();
        let _ = cleared.await;
        assert_eq!(remembered(&dir), serde_json::json!({ "/dev/ttyUSB1": sha }));
    }

    #[tokio::test]
    async fn a_key_too_long_for_a_hello_fails_to_connect() {
        let (port, _gateway) = fake_gateway(Script::default()).await;
        let mut f = forward(port, "");
        f.api_key = Secret::new("k".repeat(256));
        let err = ForwardSink::new(&f, Arc::default())
            .connect()
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cannot send HELLO: token of 256 bytes"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_rejected_hello_names_the_status() {
        let (port, gateway) = fake_gateway(Script {
            hello_status: proto::HELLO_BAD_AUTH,
            ..Script::default()
        })
        .await;
        let err = sink(port, "").connect().await.unwrap_err();
        assert!(err.to_string().contains("HELLO rejected"), "{err}");
        assert!(err.to_string().contains("status=1"), "{err}");
        let _ = gateway.await;
    }

    #[tokio::test]
    async fn a_gateway_whose_database_is_not_ready_says_so() {
        let (port, gateway) = fake_gateway(Script {
            hello_status: proto::HELLO_UNAVAILABLE,
            ..Script::default()
        })
        .await;
        let err = sink(port, "").connect().await.unwrap_err().to_string();
        assert!(err.contains("status=4"), "{err}");
        assert!(err.contains("database is not available yet"), "{err}");
        let _ = gateway.await;
    }

    /// The frames the gateway receives have to be the frames that were
    /// captured — timestamps rebuilt from the base, flags in the ingest
    /// protocol's positions rather than GVRET's.
    #[tokio::test]
    async fn a_batch_arrives_intact() {
        const BASE: i64 = 1_700_000_000_000_000;
        let (port, gateway) = fake_gateway(Script::default()).await;
        let mut s = sink(port, "");
        s.connect().await.unwrap();

        let frames = vec![
            sample(BASE, 0x123),
            Arc::new(Sample::Can(CanSample {
                extended: true,
                is_fd: true,
                dir: Direction::Tx,
                bus: SourceId(3),
                ..can(BASE + 1_500, 0x456)
            })),
            Arc::new(Sample::Modbus(ModbusSample {
                ts_us: BASE + 2_000,
                bus: SourceId(2),
                unit: 1,
                func: 0x20,
                crc_valid: true,
                raw: vec![
                    0x01, 0x20, 0x01, 0xC8, 0x03, 0x11, 0x1A, 0x00, 0x02, 0xE4, 0xCA,
                ],
            })),
        ];
        s.write_batch(&frames).await.expect("acknowledged");
        s.close().await;

        let seen = gateway.await.unwrap();
        assert_eq!(seen.batches.len(), 1);
        let batch = &seen.batches[0];
        assert_eq!(batch.base_ts_us, BASE as u64);
        assert_eq!(batch.records[0].delta_us, 0);
        assert_eq!(batch.records[0].payload, [1, 2, 3]);
        assert_eq!(batch.records[0].id_flags, 0x123, "no flags set");
        assert_eq!(batch.records[0].kind, proto::RecordKind::Can);

        let second = &batch.records[1];
        assert_eq!(second.delta_us, 1_500, "measured from the batch's base");
        assert_eq!(second.bus, 3);
        assert_eq!(second.id_flags & proto::ID_ARB_MASK, 0x456);
        assert!(second.id_flags & proto::ID_EXTENDED != 0);
        assert!(second.id_flags & proto::ID_FD != 0);
        assert!(second.id_flags & proto::ID_TX != 0, "a transmitted frame");

        // A Modbus message goes as kind 1 with the CRC verdict in its flags
        // and the unit and function code packed into the id word.
        let third = &batch.records[2];
        assert_eq!(third.kind, proto::RecordKind::Modbus);
        assert_eq!(third.flags, proto::FLAG_CRC_VALID);
        assert_eq!(third.bus, 2);
        assert_eq!(proto::modbus_unit_func(third.id_flags), (1, 0x20));
        assert_eq!(third.id_flags & proto::ID_TX, 0, "a tap transmits nothing");
        assert_eq!(
            third.payload,
            [0x01, 0x20, 0x01, 0xC8, 0x03, 0x11, 0x1A, 0x00, 0x02, 0xE4, 0xCA],
            "the whole message, CRC included"
        );
    }

    /// Two bus readers feed one queue, so a batch's first frame is not always
    /// its earliest. A base taken from the head gives every older frame a
    /// negative delta, which [`proto::encode_record_into`] saturates to zero —
    /// filing it at the head's time. Found by a live parallel run against the
    /// Python: every other column matched and only the timestamps disagreed.
    #[tokio::test]
    async fn an_out_of_order_chunk_keeps_every_timestamp() {
        const BASE: i64 = 1_700_000_000_000_000;
        // The shape the live capture showed: a bus 1 frame 37 µs older than
        // the bus 0 frame enqueued ahead of it.
        let queued = [
            (BASE + 500, SourceId(0)),
            (BASE + 463, SourceId(1)),
            (BASE + 900, SourceId(0)),
            (BASE + 880, SourceId(1)),
        ];

        let (port, gateway) = fake_gateway(Script::default()).await;
        let mut s = sink(port, "");
        s.connect().await.unwrap();
        let frames: Vec<Arc<Sample>> = queued
            .iter()
            .map(|(t, bus)| {
                Arc::new(Sample::Can(CanSample {
                    bus: *bus,
                    ..can(*t, 0x123)
                }))
            })
            .collect();
        s.write_batch(&frames).await.expect("acknowledged");
        s.close().await;

        let seen = gateway.await.unwrap();
        let batch = &seen.batches[0];
        let rebuilt: Vec<(i64, u8)> = batch
            .records
            .iter()
            .map(|r| (batch.base_ts_us as i64 + i64::from(r.delta_us), r.bus))
            .collect();
        let want: Vec<(i64, u8)> = queued.iter().map(|(t, bus)| (*t, bus.0)).collect();
        assert_eq!(
            rebuilt, want,
            "every frame keeps the time it was captured at"
        );
    }

    /// A delta is a `u32` of microseconds, so a batch cannot span more than
    /// 71.6 minutes without wrapping — and a disk cache drain reads across a
    /// whole outage in insertion order, so on a quiet bus it can. Splitting on
    /// span as well as count is what keeps the recovery path honest.
    #[tokio::test]
    async fn a_batch_spanning_more_than_a_u32_of_microseconds_is_split() {
        const HOUR_US: i64 = 3_600_000_000;
        let ts = [0, HOUR_US, 2 * HOUR_US, 2 * HOUR_US + 1];

        let (port, gateway) = fake_gateway(Script::default()).await;
        let mut s = sink(port, "");
        s.connect().await.unwrap();
        let frames: Vec<Arc<Sample>> = ts.iter().map(|t| sample(*t, 0x123)).collect();
        s.write_batch(&frames).await.expect("acknowledged");
        s.close().await;

        let seen = gateway.await.unwrap();
        let rebuilt: Vec<i64> = seen
            .batches
            .iter()
            .flat_map(|b| {
                b.records
                    .iter()
                    .map(|r| b.base_ts_us as i64 + i64::from(r.delta_us))
            })
            .collect();
        assert_eq!(rebuilt, ts, "no frame is filed 71.6 minutes early");
        assert!(seen.batches.len() > 1, "the span forced a split");
    }

    /// The protocol caps a batch at 256 records and a gateway NACKs one that
    /// claims more, so a bigger batch has to be split rather than sent.
    #[tokio::test]
    async fn an_oversized_batch_is_split() {
        let (port, gateway) = fake_gateway(Script::default()).await;
        let mut s = sink(port, "");
        s.connect().await.unwrap();

        let frames: Vec<Arc<Sample>> = (0..600).map(|i| sample(i64::from(i), i)).collect();
        s.write_batch(&frames).await.unwrap();
        s.close().await;

        let seen = gateway.await.unwrap();
        let sizes: Vec<usize> = seen.batches.iter().map(|b| b.records.len()).collect();
        assert_eq!(sizes, [256, 256, 88]);
        // Each chunk carries its own base, and its own sequence number.
        assert_eq!(seen.batches[1].base_ts_us, 256);
        let seqs: Vec<u32> = seen.batches.iter().map(|b| b.seq).collect();
        assert_eq!(seqs, [1, 2, 3]);
    }

    /// The frame's length field is a `u16`, and 256 full-size Modbus messages
    /// overrun it. A cache drain after an outage on a line full of long read
    /// responses is exactly that batch, so it has to split by bytes as well as
    /// by count — and every message has to arrive whole.
    #[tokio::test]
    async fn a_batch_of_full_size_modbus_messages_is_split_by_bytes() {
        let (port, gateway) = fake_gateway(Script::default()).await;
        let mut s = sink(port, "");
        s.connect().await.unwrap();

        let frames: Vec<Arc<Sample>> = (0..300).map(modbus).collect();
        s.write_batch(&frames)
            .await
            .expect("every chunk fitted its frame");
        s.close().await;

        let seen = gateway.await.unwrap();
        let per_batch = proto::MAX_BODY.saturating_sub(proto::BATCH_HEADER)
            / proto::record_wire_len(proto::RecordKind::Modbus, 256);
        assert!(
            per_batch < proto::MAX_BATCH_RECORDS,
            "the byte bound is the tighter one"
        );
        let sizes: Vec<usize> = seen.batches.iter().map(|b| b.records.len()).collect();
        assert_eq!(sizes[0], per_batch);
        assert_eq!(sizes.iter().sum::<usize>(), 300);
        assert!(seen
            .batches
            .iter()
            .flat_map(|b| &b.records)
            .all(|r| r.payload.len() == proto::RecordKind::Modbus.max_payload()));
    }

    /// A gateway that says it cannot keep up must be believed: failing here is
    /// what puts the frames on disk instead of into a gateway that would drop
    /// them.
    #[tokio::test]
    async fn an_overloaded_gateway_fails_the_write() {
        let (port, gateway) = fake_gateway(Script {
            ack_status: proto::ACK_OVERLOADED,
            ..Script::default()
        })
        .await;
        let mut s = sink(port, "");
        s.connect().await.unwrap();
        let err = s.write_batch(&[sample(1, 1)]).await.unwrap_err().cause;
        assert_eq!(err, SinkError("forward: gateway overloaded".into()));
        s.close().await;
        let _ = gateway.await;
    }

    #[tokio::test]
    async fn a_nacked_batch_names_its_sequence() {
        let (port, gateway) = fake_gateway(Script {
            ack_status: proto::ACK_CRC,
            ..Script::default()
        })
        .await;
        let mut s = sink(port, "");
        s.connect().await.unwrap();
        let err = s.write_batch(&[sample(1, 1)]).await.unwrap_err().cause;
        assert!(err.to_string().contains("seq=1"), "{err}");
        assert!(err.to_string().contains("status=1"), "{err}");
        s.close().await;
        let _ = gateway.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_refused_as_malformed_is_quarantined_and_the_link_moves_on() {
        let dir = TempDir::new("quarantine");
        let (port, gateway) = fake_gateway(Script {
            ack_status: proto::ACK_MALFORMED,
            ..Script::default()
        })
        .await;
        let mut s = sink_caching_at(port, dir.0.join("cache.db"));
        s.connect().await.unwrap();
        let refused = [sample(1, 0x111), sample(2, 0x222)];
        s.write_batch(&refused)
            .await
            .expect("quarantined, not failed");
        s.write_batch(&[sample(3, 0x333)])
            .await
            .expect("the next batch goes on the same connection");
        s.close().await;
        assert_eq!(gateway.await.unwrap().batches.len(), 2);

        let mut dead = SqliteCache::open(dir.0.join("cache.dead-letter.db"), 1).unwrap();
        let kept: Vec<Arc<Sample>> = dead
            .oldest(10)
            .unwrap()
            .into_iter()
            .map(|c| c.sample)
            .collect();
        assert_eq!(kept, [refused.as_slice(), &[sample(3, 0x333)]].concat());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_malformed_batch_that_cannot_be_quarantined_fails_the_write() {
        let dir = TempDir::new("no-quarantine");
        let not_a_directory = dir.0.join("file");
        std::fs::write(&not_a_directory, b"").unwrap();
        let (port, gateway) = fake_gateway(Script {
            ack_status: proto::ACK_MALFORMED,
            ..Script::default()
        })
        .await;
        let mut s = sink_caching_at(port, not_a_directory.join("cache.db"));
        s.connect().await.unwrap();
        let err = s.write_batch(&[sample(1, 1)]).await.unwrap_err().cause;
        assert!(err.to_string().contains("seq=1"), "{err}");
        s.close().await;
        let _ = gateway.await;
    }

    /// Two chunks: the span between the second and third frames forces a split.
    fn two_chunks() -> Vec<Arc<Sample>> {
        const HOUR_US: i64 = 3_600_000_000;
        [0, HOUR_US, 2 * HOUR_US, 2 * HOUR_US + 1]
            .iter()
            .map(|t| sample(*t, 0x123))
            .collect()
    }

    /// Run a batcher over `queued` until it has nothing left it can do.
    async fn run_batcher(port: u16, cache_path: PathBuf, queued: &[Arc<Sample>]) {
        let mut f = forward(port, "");
        f.batching.cache_path = cache_path;
        let cache = SqliteCache::open(&f.batching.cache_path, 1).unwrap();
        let (archive, batcher, _stop) = crate::archive::channel(
            ForwardSink::new(&f, Arc::default()),
            cache,
            &f.batching,
            0.0,
            None,
        );
        for s in queued {
            archive.enqueue(Arc::clone(s));
        }
        drop(archive);
        batcher.run().await;
    }

    fn cached(path: &std::path::Path) -> Vec<Arc<Sample>> {
        let held = SqliteCache::open(path, 1).unwrap().oldest(100).unwrap();
        held.into_iter().map(|c| c.sample).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_split_batch_failing_midway_caches_only_what_the_gateway_did_not_take() {
        let dir = TempDir::new("split-queue");
        let (port, gateway) = fake_gateway(Script {
            ack_status: proto::ACK_OVERLOADED,
            nack_from: 1,
            ..Script::default()
        })
        .await;
        let frames = two_chunks();
        run_batcher(port, dir.0.join("cache.db"), &frames).await;

        assert_eq!(gateway.await.unwrap().batches.len(), 2);
        assert_eq!(cached(&dir.0.join("cache.db")), frames[2..]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cache_drain_failing_midway_keeps_only_what_the_gateway_did_not_take() {
        let dir = TempDir::new("split-drain");
        let cache_path = dir.0.join("cache.db");
        let frames = two_chunks();
        SqliteCache::open(&cache_path, 1)
            .unwrap()
            .append(&frames)
            .unwrap();
        let (port, gateway) = fake_gateway(Script {
            ack_status: proto::ACK_OVERLOADED,
            nack_from: 1,
            ..Script::default()
        })
        .await;
        run_batcher(port, cache_path.clone(), &[]).await;

        assert_eq!(gateway.await.unwrap().batches.len(), 2);
        assert_eq!(cached(&cache_path), frames[2..]);
    }

    #[tokio::test]
    async fn a_gateway_that_hangs_up_is_reported_rather_than_hanging() {
        let (port, gateway) = fake_gateway(Script {
            close_on_batch: true,
            ..Script::default()
        })
        .await;
        let mut s = sink(port, "");
        s.connect().await.unwrap();
        let err = s.write_batch(&[sample(1, 1)]).await.unwrap_err().cause;
        assert_eq!(
            err,
            SinkError("forward: gateway closed connection".into()),
            "and not a ten-second timeout"
        );
        let _ = gateway.await;
    }

    #[tokio::test]
    async fn an_idle_connection_is_pinged() {
        let (port, gateway) = fake_gateway(Script::default()).await;
        let mut s = sink(port, "");
        s.connect().await.unwrap();
        s.keep_alive().await.expect("ponged");
        s.close().await;
        assert_eq!(gateway.await.unwrap().pings, 1);
    }

    #[tokio::test]
    async fn a_gateway_that_is_not_there_fails_to_connect() {
        // Port 1 on loopback: nothing binds it, and connecting is refused
        // immediately rather than timing out.
        let mut s = sink(1, "");
        let err = s.connect().await.unwrap_err();
        assert!(
            err.to_string().starts_with("forward: connect failed"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_ack_carrying_another_batchs_seq_fails_the_exchange() {
        let (port, gateway) = fake_gateway(Script::default()).await;
        let mut s = sink(port, "");
        s.connect().await.unwrap();
        s.send_chunk(&[sample(1, 1)], 1)
            .await
            .expect("acknowledged");
        let stale = proto::encode_ack(1, proto::ACK_OK, 0);
        s.conn.as_mut().unwrap().rx.extend_from_slice(&stale);

        let err = s.send_chunk(&[sample(2, 2)], 2).await.unwrap_err();
        assert!(err.to_string().contains("seq=1"), "{err}");
        assert!(err.to_string().contains("seq=2"), "{err}");
        s.close().await;
        let _ = gateway.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_malformed_nack_for_seq_zero_is_still_quarantined() {
        let dir = TempDir::new("seq-zero");
        let (port, gateway) = fake_gateway(Script::default()).await;
        let mut s = sink_caching_at(port, dir.0.join("cache.db"));
        s.connect().await.unwrap();
        let unread_seq = proto::encode_ack(0, proto::ACK_MALFORMED, 0);
        s.conn.as_mut().unwrap().rx.extend_from_slice(&unread_seq);

        s.send_chunk(&[sample(1, 1)], 1)
            .await
            .expect("quarantined, not failed");
        s.close().await;
        let _ = gateway.await;
    }
}
