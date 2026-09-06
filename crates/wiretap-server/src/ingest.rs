//! The binary ingest listener: frames pushed *to* this server.
//!
//! The other half of what [`crate::forward`] speaks. A microcontroller too
//! small to hold a bus open to the gateway sends its batches here instead, and
//! they join the same queue the CAN readers feed — so a pushed frame is cached
//! through an outage and drained in order exactly like a captured one.
//!
//! Every batch is acknowledged only after it has been accepted into that queue,
//! and the acknowledgement carries how full the queue is. A client that ignores
//! that signal is refused outright rather than having its frames dropped
//! silently: at-least-once delivery is the device's to complete, and it can only
//! do that if a refusal is visible.
//!
//! The session loop mirrors the gateway's (`wiretap-backend/src/ingest/mod.rs`)
//! because both serve the same protocol. They are deliberately not shared: the
//! codec is, in `wiretap-ingest-proto`, but sharing the *driver* would mean
//! putting tokio into a crate whose whole point is that a client can speak the
//! protocol without one.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};
use wiretap_ingest_proto as proto;
use wiretap_model::{CanSample, Direction, Secret, SourceId};

use crate::archive::Archive;
use crate::settings::Ingest;
use crate::source::system_time_to_us;

/// One read from a client. The Python's `recv(65536)`, and large on purpose: a
/// full batch is ~19 kB and arrives as one burst.
const READ_BUF: usize = 65536;

/// Queue occupancy at which a batch is refused rather than accepted.
///
/// Refusing at *almost* full rather than full is the point: the client retries
/// the same sequence number later, so the frames wait on the device — which has
/// its own buffer — instead of being dropped here.
const REFUSE_ABOVE_PCT: u8 = 99;

/// How long a silent client is kept. The Python's `keepalive_secs * 3`, and the
/// gateway's too, so a device tuned for one is not dropped by the other.
const IDLE_MULTIPLIER: u32 = 3;

/// What every session needs, shared across all of them.
struct Config {
    /// Empty disables authentication, which is the Python's behaviour and what
    /// a closed private network deployment relies on.
    token: Secret,
    idle_limit: Duration,
    max_batch_frames: usize,
}

pub struct Server {
    listener: TcpListener,
    config: Arc<Config>,
    archive: Archive,
}

impl Server {
    pub async fn bind(ingest: &Ingest, archive: Archive) -> std::io::Result<Self> {
        let listener = TcpListener::bind((ingest.host.as_str(), ingest.port)).await?;
        info!(
            "Ingest listening on {}:{} (auth {})",
            ingest.host,
            ingest.port,
            if ingest.token.is_empty() {
                "disabled"
            } else {
                "required"
            }
        );
        Ok(Self {
            listener,
            config: Arc::new(Config {
                token: ingest.token.clone(),
                idle_limit: Duration::from_secs_f64(
                    ingest.keepalive_secs.max(0.0) * f64::from(IDLE_MULTIPLIER),
                ),
                max_batch_frames: ingest.max_batch_frames.max(1),
            }),
            archive,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept forever, one task per device.
    pub async fn run(self) {
        loop {
            let (stream, peer) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                // As in the GVRET listener: a transient accept failure must not
                // take the capture down with it.
                Err(e) => {
                    warn!("Ingest accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            info!("Ingest client {peer} connected");
            let session = Session {
                peer,
                config: self.config.clone(),
                archive: self.archive.clone(),
                authed: false,
                time_relative: false,
            };
            tokio::spawn(async move {
                session.serve(stream).await;
                info!("Ingest client {peer} disconnected");
            });
        }
    }
}

/// One connected device.
struct Session {
    peer: SocketAddr,
    config: Arc<Config>,
    archive: Archive,
    authed: bool,
    /// The client's deltas are from an epoch of its own — usually its boot —
    /// so the batch's own base is meaningless and is replaced on arrival.
    time_relative: bool,
}

/// What to do with the connection after handling one message.
enum Next {
    Continue,
    /// Close it. A device that has said something this server cannot act on
    /// gets its connection dropped, so it reconnects and starts again.
    Drop,
}

impl Session {
    async fn serve(mut self, mut stream: TcpStream) {
        let mut buf: Vec<u8> = Vec::new();
        let mut read_buf = [0u8; READ_BUF];

        loop {
            // Any traffic counts as a keepalive, which is what lets a device on
            // a quiet bus hold the connection open with `PING` alone.
            let read = tokio::time::timeout(self.config.idle_limit, stream.read(&mut read_buf));
            let n = match read.await {
                Ok(Ok(0)) | Ok(Err(_)) => return,
                Ok(Ok(n)) => n,
                Err(_) => {
                    warn!("Ingest client {} timed out", self.peer);
                    return;
                }
            };
            buf.extend_from_slice(&read_buf[..n]);

            loop {
                let frame = match proto::take_frame(&mut buf) {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break,
                    // A length no message can have: the stream is not this
                    // protocol, and nothing later in it can be trusted.
                    Err(_) => {
                        warn!("Ingest client {} sent bogus length, dropping", self.peer);
                        return;
                    }
                };
                let reply = self.handle(&frame);
                if let Some(bytes) = reply.0 {
                    if stream.write_all(&bytes).await.is_err() {
                        return;
                    }
                }
                if matches!(reply.1, Next::Drop) {
                    return;
                }
            }
        }
    }

    /// Answer one message: what to send back, and whether to stay connected.
    fn handle(&mut self, frame: &proto::WireFrame) -> (Option<Vec<u8>>, Next) {
        if !frame.crc_ok {
            // Best effort, and worth the trouble: naming the sequence number
            // lets the device resend that batch rather than time out waiting
            // for an acknowledgement that is never coming.
            if frame.mtype == proto::MSG_BATCH && frame.body.len() >= 4 {
                let seq = u32::from_le_bytes(frame.body[0..4].try_into().unwrap());
                return (Some(self.ack(seq, proto::ACK_CRC)), Next::Continue);
            }
            return (None, Next::Continue);
        }

        match frame.mtype {
            proto::MSG_HELLO => self.hello(&frame.body),
            proto::MSG_PING => (
                Some(proto::encode_message(proto::MSG_PONG, b"")),
                Next::Continue,
            ),
            proto::MSG_BATCH if !self.authed => (None, Next::Drop),
            proto::MSG_BATCH => self.batch(&frame.body),
            // Ignored rather than refused, which is what lets a client speak a
            // later version of this protocol to an older server: it announces
            // 1, reads the version back, and only then sends anything newer.
            _ => (None, Next::Continue),
        }
    }

    fn hello(&mut self, body: &[u8]) -> (Option<Vec<u8>>, Next) {
        let now_us = system_time_to_us(SystemTime::now());
        let ack = |status| Some(proto::encode_hello_ack(status, now_us.max(0) as u64));

        let Ok(hello) = proto::parse_hello(body) else {
            return (None, Next::Drop);
        };
        // Strict equality, as both server implementations use: it is what makes
        // `accepted_version` in the reply the way to discover a newer protocol,
        // rather than announcing one and hoping.
        if hello.version != proto::PROTO_VERSION {
            return (ack(proto::HELLO_BAD_VERSION), Next::Drop);
        }
        if !self.config.token.is_empty() {
            let expected = self.config.token.expose().as_bytes();
            // Constant time, and length-independent: a plain `==` would leak
            // the token a byte at a time to anything that can reconnect and
            // measure.
            if !bool::from(hello.token.ct_eq(expected)) {
                warn!("Ingest client {} failed auth", self.peer);
                return (ack(proto::HELLO_BAD_AUTH), Next::Drop);
            }
        }

        self.authed = true;
        self.time_relative = hello.time_relative;
        if !hello.database.is_empty() {
            // Recorded, not honoured: this server forwards to one gateway, and
            // the gateway is what routes a database. Saying so beats a device
            // silently writing somewhere else than it asked for.
            info!(
                "Ingest client {} requested database '{}' (this server forwards to its \
                 configured gateway, which decides)",
                self.peer, hello.database
            );
        }
        (ack(proto::HELLO_OK), Next::Continue)
    }

    fn batch(&mut self, body: &[u8]) -> (Option<Vec<u8>>, Next) {
        let Some(parsed) = proto::parse_batch(body, self.config.max_batch_frames) else {
            // Too short to even carry a sequence number, so there is nothing to
            // address a refusal to.
            return (None, Next::Drop);
        };
        let batch = match parsed {
            Ok(batch) => batch,
            Err(seq) => return (Some(self.ack(seq, proto::ACK_MALFORMED)), Next::Continue),
        };

        // Refused before anything is enqueued: accepting a batch this server
        // will only drop would tell the device its frames are safe.
        if self.archive.occupancy_pct() >= REFUSE_ABOVE_PCT {
            return (
                Some(self.ack(batch.seq, proto::ACK_OVERLOADED)),
                Next::Continue,
            );
        }

        // A relative batch's base is an epoch this server knows nothing about,
        // so the newest record is stamped with its arrival and the rest are
        // back-dated by their distance from it. The newest is the *largest*
        // delta, not the last one: a sender interleaving two buses can hand
        // over a batch whose last record is not its newest, and taking the
        // last would then stamp the real newest ahead of its own arrival.
        let base_ts_us = if self.time_relative {
            let newest = batch.records.iter().map(|r| r.delta_us).max().unwrap_or(0);
            system_time_to_us(SystemTime::now()) - i64::from(newest)
        } else {
            batch.base_ts_us as i64
        };

        for record in &batch.records {
            self.archive.enqueue(Arc::new(CanSample {
                ts_us: base_ts_us + i64::from(record.delta_us),
                arb_id: record.id_flags & proto::ID_ARB_MASK,
                extended: record.id_flags & proto::ID_EXTENDED != 0,
                is_fd: record.id_flags & proto::ID_FD != 0,
                data: record.payload.clone(),
                bus: SourceId(record.bus),
                dir: if record.id_flags & proto::ID_TX != 0 {
                    Direction::Tx
                } else {
                    Direction::Rx
                },
            }));
        }
        (Some(self.ack(batch.seq, proto::ACK_OK)), Next::Continue)
    }

    fn ack(&self, seq: u32, status: u8) -> Vec<u8> {
        proto::encode_ack(seq, status, self.archive.occupancy_pct())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{BatchSink, SinkResult};
    use crate::cache::{Cached, FrameCache};
    use crate::settings::Batching;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tokio::sync::watch;

    /// What a test reads back: every frame the archive handed to the sink.
    type Seen = Arc<Mutex<Vec<Arc<CanSample>>>>;

    /// Stands in for the gateway, keeping what it was given so a test can see
    /// what the listener put into the archive.
    struct RecordingSink(Seen);

    impl BatchSink for RecordingSink {
        async fn connect(&mut self) -> SinkResult {
            Ok(())
        }
        async fn write_batch(&mut self, batch: &[Arc<CanSample>]) -> SinkResult {
            self.0.lock().unwrap().extend(batch.iter().cloned());
            Ok(())
        }
        async fn keep_alive(&mut self) -> SinkResult {
            Ok(())
        }
        async fn close(&mut self) {}
    }

    #[derive(Default)]
    struct NullCache;

    impl FrameCache for NullCache {
        fn append(&mut self, frames: &[Arc<CanSample>]) -> crate::cache::Result<usize> {
            Ok(frames.len())
        }
        fn oldest(&mut self, _: usize) -> crate::cache::Result<Vec<Cached>> {
            Ok(Vec::new())
        }
        fn remove(&mut self, _: &[Cached]) -> crate::cache::Result<()> {
            Ok(())
        }
        fn count(&mut self) -> crate::cache::Result<u64> {
            Ok(0)
        }
        fn size_bytes(&self) -> u64 {
            0
        }
        fn is_full(&self) -> bool {
            false
        }
        fn reset(&mut self) -> crate::cache::Result<()> {
            Ok(())
        }
    }

    /// A real archive with a recording sink, so what a test observes is what
    /// came out of the queue rather than a shortcut around it.
    fn archive_capturing(queue_size: usize) -> (Archive, Seen, watch::Sender<bool>) {
        let batching = Batching {
            // One frame per batch, sent almost immediately: a test wants each
            // enqueue visible, not the throughput the defaults are tuned for.
            size: 1,
            flush_interval: 0.01,
            queue_size,
            cache_path: PathBuf::from("unused"),
            cache_max_mb: 1,
            queue_flush_pct: 100,
            cache_origin: None,
            legacy_cache_path: None,
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (archive, batcher, stop) =
            crate::archive::channel(RecordingSink(seen.clone()), NullCache, &batching, 0.0);
        tokio::spawn(batcher.run());
        // The stop signal is handed back rather than dropped here: dropping it
        // would close the queue immediately and these tests would observe an
        // archive that takes nothing.
        (archive, seen, stop)
    }

    /// Wait for the archive to have `n` frames, rather than assuming the
    /// batcher has run by the time the acknowledgement arrived.
    async fn wait_for(seen: &Mutex<Vec<Arc<CanSample>>>, n: usize) -> Vec<Arc<CanSample>> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let frames = seen.lock().unwrap().clone();
            if frames.len() >= n {
                return frames;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "waited for {n} frames, saw {}",
                frames.len()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn ingest_settings(token: &str) -> Ingest {
        Ingest {
            host: "127.0.0.1".into(),
            port: 0,
            token: Secret::new(token),
            keepalive_secs: 30.0,
            max_batch_frames: 256,
        }
    }

    async fn listener(token: &str, queue_size: usize) -> (SocketAddr, Seen, watch::Sender<bool>) {
        let (archive, seen, stop) = archive_capturing(queue_size);
        let server = Server::bind(&ingest_settings(token), archive)
            .await
            .expect("bind");
        let addr = server.local_addr().expect("bound");
        tokio::spawn(server.run());
        (addr, seen, stop)
    }

    /// Read one framed reply.
    async fn reply(stream: &mut TcpStream) -> proto::WireFrame {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            if let Ok(Some(frame)) = proto::take_frame(&mut buf) {
                return frame;
            }
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
                .await
                .expect("a reply in time")
                .expect("a readable stream");
            assert!(n > 0, "the server closed without replying");
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn one_record(delta_us: u32, arb_id: u32) -> Vec<u8> {
        let mut records = Vec::new();
        proto::encode_record_into(
            &mut records,
            0,
            u64::from(delta_us),
            proto::record_id_flags(arb_id, false, false, false),
            0,
            &[1, 2, 3],
        );
        records
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_device_hands_over_frames() {
        let (addr, seen, _stop) = listener("sekrit", 100).await;
        let mut c = TcpStream::connect(addr).await.unwrap();

        c.write_all(&proto::encode_hello(b"sekrit", "", false))
            .await
            .unwrap();
        let ack = proto::parse_hello_ack(&reply(&mut c).await.body).unwrap();
        assert_eq!(ack.status, proto::HELLO_OK);
        assert_eq!(ack.accepted_version, proto::PROTO_VERSION);

        const BASE: u64 = 1_700_000_000_000_000;
        c.write_all(&proto::encode_batch(7, BASE, 1, &one_record(250, 0x123)))
            .await
            .unwrap();
        let ack = proto::parse_ack(&reply(&mut c).await.body).unwrap();
        assert_eq!((ack.seq, ack.status), (7, proto::ACK_OK));

        let frames = wait_for(&seen, 1).await;
        assert_eq!(frames[0].arb_id, 0x123);
        assert_eq!(
            frames[0].ts_us,
            BASE as i64 + 250,
            "the record's delta, on the batch's base"
        );
        assert_eq!(frames[0].data, [1, 2, 3]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_token_is_refused_and_the_connection_closed() {
        let (addr, _, _stop) = listener("sekrit", 100).await;
        let mut c = TcpStream::connect(addr).await.unwrap();

        c.write_all(&proto::encode_hello(b"wrong", "", false))
            .await
            .unwrap();
        let ack = proto::parse_hello_ack(&reply(&mut c).await.body).unwrap();
        assert_eq!(ack.status, proto::HELLO_BAD_AUTH);

        // And the connection goes, so a guessing client pays a reconnect per
        // attempt.
        let mut buf = [0u8; 8];
        assert_eq!(c.read(&mut buf).await.unwrap(), 0, "closed after bad auth");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_token_disables_authentication() {
        let (addr, _, _stop) = listener("", 100).await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(&proto::encode_hello(b"anything at all", "", false))
            .await
            .unwrap();
        let ack = proto::parse_hello_ack(&reply(&mut c).await.body).unwrap();
        assert_eq!(ack.status, proto::HELLO_OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_before_hello_drops_the_connection() {
        let (addr, seen, _stop) = listener("sekrit", 100).await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(&proto::encode_batch(1, 0, 1, &one_record(0, 0x1)))
            .await
            .unwrap();

        let mut buf = [0u8; 8];
        assert_eq!(c.read(&mut buf).await.unwrap(), 0, "closed, unacknowledged");
        assert!(seen.lock().unwrap().is_empty(), "and nothing was archived");
        let _ = seen;
    }

    /// A corrupt batch is nacked *by sequence number*, which is what lets the
    /// device resend it rather than wait for an acknowledgement forever.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_corrupt_batch_is_nacked_then_resent() {
        let (addr, seen, _stop) = listener("sekrit", 100).await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(&proto::encode_hello(b"sekrit", "", false))
            .await
            .unwrap();
        reply(&mut c).await;

        let good = proto::encode_batch(9, 0, 1, &one_record(0, 0x321));
        let mut corrupt = good.clone();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xFF;
        c.write_all(&corrupt).await.unwrap();
        let ack = proto::parse_ack(&reply(&mut c).await.body).unwrap();
        assert_eq!((ack.seq, ack.status), (9, proto::ACK_CRC));
        assert!(seen.lock().unwrap().is_empty(), "nothing was archived");

        c.write_all(&good).await.unwrap();
        let ack = proto::parse_ack(&reply(&mut c).await.body).unwrap();
        assert_eq!((ack.seq, ack.status), (9, proto::ACK_OK));
        assert_eq!(wait_for(&seen, 1).await.len(), 1, "the resend landed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_claiming_more_than_the_limit_is_nacked() {
        let (addr, _, _stop) = listener("sekrit", 100).await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(&proto::encode_hello(b"sekrit", "", false))
            .await
            .unwrap();
        reply(&mut c).await;

        // One record, but a count claiming five thousand.
        let mut body = 11u32.to_le_bytes().to_vec();
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&5_000u16.to_le_bytes());
        body.extend_from_slice(&one_record(0, 0x1));
        c.write_all(&proto::encode_message(proto::MSG_BATCH, &body))
            .await
            .unwrap();

        let ack = proto::parse_ack(&reply(&mut c).await.body).unwrap();
        assert_eq!((ack.seq, ack.status), (11, proto::ACK_MALFORMED));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_older_protocol_is_refused_by_version() {
        let (addr, _, _stop) = listener("", 100).await;
        let mut c = TcpStream::connect(addr).await.unwrap();

        // A HELLO announcing version 99, built by hand: the codec only ever
        // writes the version it was compiled with.
        let mut body = proto::MAGIC.to_vec();
        body.extend_from_slice(&[99, 0, 0, 0]);
        c.write_all(&proto::encode_message(proto::MSG_HELLO, &body))
            .await
            .unwrap();

        let ack = proto::parse_hello_ack(&reply(&mut c).await.body).unwrap();
        assert_eq!(ack.status, proto::HELLO_BAD_VERSION);
        assert_eq!(
            ack.accepted_version,
            proto::PROTO_VERSION,
            "and says what it does speak, so a client can decide"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_quiet_device_holds_the_connection_with_pings() {
        let (addr, _, _stop) = listener("", 100).await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(&proto::encode_message(proto::MSG_PING, b""))
            .await
            .unwrap();
        assert_eq!(reply(&mut c).await.mtype, proto::MSG_PONG);
    }

    /// `TIME_RELATIVE` deltas are from the device's own epoch, so the newest
    /// record is stamped with its arrival and the rest are back-dated.
    #[tokio::test(flavor = "multi_thread")]
    async fn relative_timestamps_are_back_dated_from_arrival() {
        let (addr, seen, _stop) = listener("", 100).await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(&proto::encode_hello(b"", "", true))
            .await
            .unwrap();
        reply(&mut c).await;

        // Deltas after some boot the server knows nothing about, and a base
        // that is nonsense on this server's clock. Deliberately not in order:
        // a sender interleaving two buses hands over a batch whose last record
        // is not its newest, and taking the last would stamp the real newest
        // ahead of the arrival it is supposed to be pinned to.
        let mut records = Vec::new();
        for delta in [1_000_000u64, 3_000_000, 2_000_000] {
            proto::encode_record_into(&mut records, 0, delta, 0x100, 0, &[7]);
        }
        let before = system_time_to_us(SystemTime::now());
        c.write_all(&proto::encode_batch(1, 42, 3, &records))
            .await
            .unwrap();
        let ack = proto::parse_ack(&reply(&mut c).await.body).unwrap();
        assert_eq!(ack.status, proto::ACK_OK);
        let after = system_time_to_us(SystemTime::now());

        let ts: Vec<i64> = wait_for(&seen, 3).await.iter().map(|f| f.ts_us).collect();
        assert!(
            (before..=after).contains(&ts[1]),
            "the newest record is the largest delta, and is stamped on its \
             arrival: {} not in {before}..={after}",
            ts[1]
        );
        assert_eq!(ts[1] - ts[0], 2_000_000, "the others keep their distance");
        assert_eq!(ts[1] - ts[2], 1_000_000);
        assert!(ts[1] > 42, "the client's own base is discarded");
    }
}
