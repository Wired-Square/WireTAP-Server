//! Forwarding frames to a WireTAP gateway over the binary ingest protocol.
//!
//! The only [`BatchSink`] there is. It speaks the client half of
//! `wiretap-ingest-proto`, whose server half the gateway parses — one codec,
//! both ends, so a change to the wire format cannot land on one side only.
//!
//! Every batch is acknowledged **after** the gateway has written it, so a slow
//! or failing archive is felt here as a failed write rather than an accepted
//! one, and the batcher puts the frames on disk instead of losing them. That is
//! the whole reason this is a stream protocol with ACKs rather than a POST.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::info;
use wiretap_ingest_proto as proto;
use wiretap_model::{CanSample, Direction, Secret};

use crate::archive::{BatchSink, SinkError, SinkResult};
use crate::settings::Forward;

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
    conn: Option<Connection>,
    /// Wraps with the protocol's `u32`, as the Python's `& 0xFFFFFFFF` did. It
    /// identifies an ACK against its batch, so it only has to be unique among
    /// those in flight — and there is only ever one.
    seq: u32,
    /// Reused across batches: a batch is up to 256 records of up to 74 bytes,
    /// and this runs for every batch for the life of the process.
    records: Vec<u8>,
}

/// A connection and whatever of a reply has arrived so far.
struct Connection {
    stream: TcpStream,
    rx: Vec<u8>,
}

impl ForwardSink {
    pub fn new(forward: &Forward) -> Self {
        Self {
            host: forward.host.clone(),
            port: forward.port,
            api_key: forward.api_key.clone(),
            database: forward.database.clone(),
            conn: None,
            seq: 0,
            records: Vec::new(),
        }
    }

    fn connection(&mut self) -> Result<&mut Connection, SinkError> {
        self.conn
            .as_mut()
            .ok_or_else(|| SinkError("forward: not connected".into()))
    }

    /// Send one `BATCH` and wait for its acknowledgement.
    ///
    /// `chunk` is at most [`proto::MAX_BATCH_RECORDS`]; a gateway NACKs a batch
    /// that claims more, rather than accepting a truncated one.
    async fn send_chunk(&mut self, chunk: &[Arc<CanSample>]) -> SinkResult {
        // Absolute timestamps: the base is the first frame's, and every record
        // is a delta from it. The `[forward]` client does not set
        // `TIME_RELATIVE`, so the gateway takes these at face value rather than
        // re-basing them on its own clock.
        let base_ts_us = chunk[0].ts_us.max(0) as u64;
        self.records.clear();
        for f in chunk {
            proto::encode_record_into(
                &mut self.records,
                base_ts_us,
                f.ts_us.max(0) as u64,
                proto::record_id_flags(f.arb_id, f.extended, f.is_fd, f.dir == Direction::Tx),
                f.bus.0,
                &f.data,
            );
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
        match ack.status {
            proto::ACK_OK => Ok(()),
            // Back-pressure, and the reason this protocol has an ACK at all:
            // failing here caches the frames rather than dropping them into a
            // gateway that has said it cannot take them.
            proto::ACK_OVERLOADED => Err(SinkError("forward: gateway overloaded".into())),
            status => Err(SinkError(format!(
                "forward: batch nacked (seq={} status={status})",
                ack.seq
            ))),
        }
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

    /// Read until one complete, intact message has arrived.
    async fn recv(&mut self) -> Result<proto::WireFrame, SinkError> {
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
        let stream = with_timeout(
            "connect",
            TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await?;
        // Handshaken on a local and stored only once it has worked, so no
        // failure path has to remember to undo it.
        let mut conn = Connection {
            stream,
            rx: Vec::new(),
        };

        let hello = proto::encode_hello(self.api_key.expose().as_bytes(), &self.database, false);
        conn.send(&hello).await?;
        let frame = conn.recv().await?;
        if frame.mtype != proto::MSG_HELLO_ACK {
            return Err(SinkError("forward: no HELLO_ACK from gateway".into()));
        }
        let ack =
            proto::parse_hello_ack(&frame.body).map_err(|e| SinkError(format!("forward: {e}")))?;
        if ack.status != proto::HELLO_OK {
            return Err(SinkError(format!(
                "forward: HELLO rejected (status={})",
                ack.status
            )));
        }
        self.conn = Some(conn);

        info!(
            "connected (forward -> {}:{} db={})",
            self.host,
            self.port,
            if self.database.is_empty() {
                "<default>"
            } else {
                &self.database
            }
        );
        Ok(())
    }

    async fn write_batch(&mut self, batch: &[Arc<CanSample>]) -> SinkResult {
        for chunk in batch.chunks(proto::MAX_BATCH_RECORDS) {
            self.send_chunk(chunk).await?;
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
    use crate::settings::Batching;
    use std::path::PathBuf;
    use tokio::net::TcpListener;
    use wiretap_model::SourceId;

    /// What one connection to the fake gateway saw.
    #[derive(Debug, Default)]
    struct Seen {
        token: Vec<u8>,
        database: String,
        batches: Vec<proto::Batch>,
        pings: usize,
    }

    /// How the fake gateway should answer.
    #[derive(Clone, Copy)]
    struct Script {
        hello_status: u8,
        ack_status: u8,
        /// Hang up rather than answering the first batch.
        close_on_batch: bool,
    }

    impl Default for Script {
        fn default() -> Self {
            Self {
                hello_status: proto::HELLO_OK,
                ack_status: proto::ACK_OK,
                close_on_batch: false,
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
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut seen = Seen::default();
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
                let Some(frame) = frame else { return seen };
                assert!(frame.crc_ok, "the client sent a bad CRC");

                let reply = match frame.mtype {
                    proto::MSG_HELLO => {
                        let hello = proto::parse_hello(&frame.body).expect("a valid HELLO");
                        seen.token = hello.token;
                        seen.database = hello.database;
                        proto::encode_hello_ack(script.hello_status, 1_234)
                    }
                    proto::MSG_BATCH => {
                        if script.close_on_batch {
                            return seen;
                        }
                        let batch = proto::parse_batch(&frame.body, proto::MAX_BATCH_RECORDS)
                            .expect("carries a seq")
                            .expect("well formed");
                        let seq = batch.seq;
                        seen.batches.push(batch);
                        proto::encode_ack(seq, script.ack_status, 0)
                    }
                    proto::MSG_PING => {
                        seen.pings += 1;
                        proto::encode_message(proto::MSG_PONG, b"")
                    }
                    other => panic!("unexpected message type {other:#x}"),
                };
                if stream.write_all(&reply).await.is_err() {
                    return seen;
                }
            }
        });
        (port, task)
    }

    fn sink(port: u16, database: &str) -> ForwardSink {
        ForwardSink::new(&Forward {
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
                legacy_cache_path: None,
            },
        })
    }

    fn sample(ts_us: i64, arb_id: u32) -> Arc<CanSample> {
        Arc::new(CanSample {
            ts_us,
            arb_id,
            extended: false,
            is_fd: false,
            data: vec![1, 2, 3],
            bus: SourceId(0),
            dir: Direction::Rx,
        })
    }

    #[tokio::test]
    async fn a_hello_carries_the_key_and_the_database() {
        let (port, gateway) = fake_gateway(Script::default()).await;
        let mut s = sink(port, "vehicle_1");
        s.connect().await.expect("the gateway accepted it");
        s.close().await;

        let seen = gateway.await.unwrap();
        assert_eq!(seen.token, b"sekrit");
        assert_eq!(seen.database, "vehicle_1");
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
            Arc::new(CanSample {
                extended: true,
                is_fd: true,
                dir: Direction::Tx,
                bus: SourceId(3),
                ..(*sample(BASE + 1_500, 0x456)).clone()
            }),
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

        let second = &batch.records[1];
        assert_eq!(second.delta_us, 1_500, "measured from the batch's base");
        assert_eq!(second.bus, 3);
        assert_eq!(second.id_flags & proto::ID_ARB_MASK, 0x456);
        assert!(second.id_flags & proto::ID_EXTENDED != 0);
        assert!(second.id_flags & proto::ID_FD != 0);
        assert!(second.id_flags & proto::ID_TX != 0, "a transmitted frame");
    }

    /// The protocol caps a batch at 256 records and a gateway NACKs one that
    /// claims more, so a bigger batch has to be split rather than sent.
    #[tokio::test]
    async fn an_oversized_batch_is_split() {
        let (port, gateway) = fake_gateway(Script::default()).await;
        let mut s = sink(port, "");
        s.connect().await.unwrap();

        let frames: Vec<Arc<CanSample>> = (0..600).map(|i| sample(i64::from(i), i)).collect();
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
        let err = s.write_batch(&[sample(1, 1)]).await.unwrap_err();
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
        let err = s.write_batch(&[sample(1, 1)]).await.unwrap_err();
        assert!(err.to_string().contains("seq=1"), "{err}");
        assert!(err.to_string().contains("status=1"), "{err}");
        s.close().await;
        let _ = gateway.await;
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
        let err = s.write_batch(&[sample(1, 1)]).await.unwrap_err();
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
}
