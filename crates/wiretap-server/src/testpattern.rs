//! Answering a Test Pattern run on the bus.
//!
//! A capture tool's own transport is the one thing its captures cannot check: a
//! codec that truncates a payload or downgrades a CAN FD frame to classic
//! produces a capture that looks entirely healthy. Test Pattern proves the link
//! instead — one end measures, the other answers. This is the answering end.
//!
//! **Every byte of the protocol lives in `wiretap_protocol::testpattern`**, and
//! deliberately not here. Hand-written copies of these codecs across the
//! WireTAP repositories are what extracting that crate existed to end — its own
//! `dlc` module records the sixteen-entry length table alone having been
//! written out nine times over. What this module owns is what the crate refuses
//! to: a socket, a clock and a configuration. If a tag constant or a length-code
//! table starts to appear below, it belongs upstream.
//!
//! The loop subscribes to the capture broadcast, as `pipeline::echo_loop` does,
//! and filters it, as nothing else does — a responder must ignore both other
//! buses and ordinary traffic. Reading from the broadcast rather than from the
//! reader is what keeps it unable to stall the capture, which matters because
//! it transmits and capture must not wait on it.

use std::io;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::broadcast;
use tracing::warn;
use wiretap_model::{CanSample, Direction, SourceId};
use wiretap_protocol::testpattern::{capability, is_test_pattern_frame, Reply, Responder};

use crate::archive;
use crate::source::system_time_to_us;

/// Where a reply goes once the state machine has produced one.
///
/// A trait for the same reason [`crate::archive::BatchSink`] is one: the socket
/// is Linux-only, so without it every assertion about this module's own
/// decisions — the capability claim, the bus filter, the Test Pattern id gate —
/// would live in the `#[ignore]`d `vcan` suite that nothing runs on a laptop.
/// The socket's own behaviour still needs that suite; this covers what is
/// decided here. `Send` because the loop is spawned as a task.
pub trait ReplySink: Send {
    fn send(&self, reply: &Reply) -> impl std::future::Future<Output = io::Result<()>> + Send;
}

#[cfg(target_os = "linux")]
impl ReplySink for Arc<crate::source::socketcan::CanReader> {
    async fn send(&self, reply: &Reply) -> io::Result<()> {
        self.transmit(reply.arb_id, reply.extended, reply.fd, &reply.data)
            .await
    }
}

/// Answer Test Pattern frames arriving on `bus` until the capture stops.
///
/// One responder per answering interface, because a run binds to a bus: two
/// interfaces sharing one would count each other's frames as their own.
///
/// **`can_fd` decides whether CAN FD is claimed**, because `CanReader::recv`
/// drops FD frames unless it was opened with `accept_fd`. A responder claiming
/// FD on a server started without `--can-fd` would answer the whole FD sweep
/// with silence, and the initiator would report a link that cannot carry CAN
/// FD. It can; this server was not listening for it. Saying so in the
/// capability bits is what lets the initiator skip the sweep rather than fail
/// it.
pub async fn responder_loop<S: ReplySink>(
    sink: S,
    bus: SourceId,
    can_fd: bool,
    mut frames: broadcast::Receiver<Arc<CanSample>>,
    archive: Option<archive::Archive>,
) {
    // Extended is unconditional: `transmit` builds either id width and nothing
    // filters one out on the way in.
    let capabilities = capability::EXTENDED | if can_fd { capability::FD } else { 0 };
    let mut responder = Responder::new(capabilities, bus.0);
    loop {
        let sample = match frames.recv().await {
            Ok(sample) => sample,
            // Frames this responder never saw are frames it never answered,
            // which the initiator counts as drops. Saying so is the difference
            // between "the link is bad" and "this server fell behind".
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("test pattern: bus {} missed {n} frames", bus.0);
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if sample.bus != bus || !is_test_pattern_frame(sample.arb_id) {
            continue;
        }
        // The capture timestamp rather than a fresh clock reading: the crate
        // uses it only to derive a frame rate, and the frame's own arrival time
        // is a better answer than the moment this task got round to it.
        for reply in responder.on_frame(
            sample.arb_id,
            sample.extended,
            sample.is_fd,
            &sample.data,
            sample.ts_us.max(0) as u64,
        ) {
            if let Err(e) = sink.send(&reply).await {
                warn!("test pattern: reply on bus {} failed: {e}", bus.0);
                continue;
            }
            // Archived as `tx`, exactly as `transmit_loop` archives a GVRET
            // client's frames. Without this a captured validation run shows the
            // initiator's requests and this server saying nothing — which is
            // what a broken responder looks like. Nothing reads a frame back
            // from the socket it wrote it to, so this is the only record.
            if let Some(archive) = &archive {
                archive.enqueue(Arc::new(CanSample {
                    ts_us: system_time_to_us(SystemTime::now()),
                    arb_id: reply.arb_id,
                    extended: reply.extended,
                    is_fd: reply.fd,
                    data: reply.data,
                    bus,
                    dir: Direction::Tx,
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use wiretap_model::Direction;
    use wiretap_protocol::testpattern::{
        encode, Command, Flags, Message, SWEEP_ECHO_BASE, SWEEP_REQUEST_BASE,
    };

    /// The bytes that would have gone on the wire.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<Reply>>>);

    impl ReplySink for Recorder {
        async fn send(&self, reply: &Reply) -> io::Result<()> {
            self.0.lock().unwrap().push(reply.clone());
            Ok(())
        }
    }

    impl Recorder {
        fn taken(&self) -> Vec<Reply> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    const BUS: SourceId = SourceId(0);
    const RUN: u8 = 3;

    fn sample(arb_id: u32, is_fd: bool, data: Vec<u8>) -> Arc<CanSample> {
        Arc::new(CanSample {
            ts_us: 1_700_000_000_000_000,
            arb_id,
            extended: false,
            is_fd,
            data,
            bus: BUS,
            dir: Direction::Rx,
        })
    }

    fn framed(msg: Message) -> Arc<CanSample> {
        let data = encode(msg, Flags::new(0, RUN)).to_vec();
        sample(msg.arb_id(), false, data)
    }

    /// Drive the loop over a fixed script and return what it transmitted.
    async fn run_with(can_fd: bool, script: Vec<Arc<CanSample>>) -> Vec<Reply> {
        let (frames, rx) = broadcast::channel(64);
        for s in script {
            frames.send(s).unwrap();
        }
        drop(frames);
        let sink = Recorder::default();
        // The loop returns when the channel closes, so it needs no shutdown.
        responder_loop(sink.clone(), BUS, can_fd, rx, None).await;
        sink.taken()
    }

    async fn run(script: Vec<Arc<CanSample>>) -> Vec<Reply> {
        run_with(true, script).await
    }

    fn hello_capabilities(replies: &[Reply]) -> u8 {
        let (msg, _) = wiretap_protocol::testpattern::decode(&replies[0].data)
            .expect("a well-formed framed message");
        match msg {
            Message::Control(Command::HelloReply { capabilities, bus }) => {
                assert_eq!(bus, BUS.0, "it answers for the bus it was given");
                capabilities
            }
            other => panic!("expected a Hello reply, got {other:?}"),
        }
    }

    /// A server started without `--can-fd` never sees an FD frame, so claiming
    /// the capability would promise a sweep it would answer with silence.
    #[tokio::test]
    async fn fd_is_claimed_only_when_the_capture_accepts_fd() {
        let hello = || vec![framed(Message::Control(Command::Hello))];

        let with = hello_capabilities(&run_with(true, hello()).await);
        assert_eq!(with, capability::FD | capability::EXTENDED);

        let without = hello_capabilities(&run_with(false, hello()).await);
        assert!(without & capability::FD == 0, "not offered, not promised");
        assert!(
            without & capability::EXTENDED != 0,
            "extended ids are unaffected by --can-fd"
        );
    }

    /// The filter is the whole of this module's routing: a Test Pattern id is
    /// handed to the state machine, and anything else on the same bus is not.
    #[tokio::test]
    async fn a_test_pattern_frame_is_answered_and_ordinary_traffic_is_not() {
        let replies = run(vec![
            sample(0x123, false, vec![0xAA; 8]),
            framed(Message::Control(Command::Hello)),
            sample(0x7FF, false, vec![0; 8]),
        ])
        .await;
        // Adjacent to the framed ids and deliberately outside them, so this is
        // a boundary rather than an arbitrary id. Stated, because the whole
        // test rests on it.
        assert!(!is_test_pattern_frame(0x7FF));

        assert_eq!(replies.len(), 1, "only the Hello is ours to answer");
        assert_eq!(
            hello_capabilities(&replies),
            capability::FD | capability::EXTENDED,
            "spelled out rather than recomputed: an assertion against the same \
             expression the code uses would pass however that expression changed"
        );
    }

    /// **The regression this work exists to prevent.** `transmit` clamped every
    /// payload to 8 bytes, so before the FD path an echo of code 15 would have
    /// gone out as 8 bytes — and an initiator comparing the echo against the
    /// length the code names would have blamed the link.
    #[tokio::test]
    async fn an_fd_sweep_is_echoed_at_its_full_length() {
        let payload: Vec<u8> = (0..64).map(|i| i as u8).collect();
        let replies = run(vec![
            framed(Message::Control(Command::Start { mode: 0, run: RUN })),
            sample(SWEEP_REQUEST_BASE + 15, true, payload.clone()),
        ])
        .await;

        assert_eq!(replies.len(), 1);
        let echo = &replies[0];
        assert_eq!(echo.arb_id, SWEEP_ECHO_BASE + 15);
        assert!(echo.fd, "an FD request is echoed as an FD frame");
        assert_eq!(echo.data, payload, "all 64 bytes, not the first 8");
    }

    /// A sweep outside a run is not answered, which is what stops two servers
    /// on one bus from both replying to a run neither was told about.
    #[tokio::test]
    async fn a_sweep_before_start_is_ignored() {
        let replies = run(vec![sample(SWEEP_REQUEST_BASE + 15, true, vec![0xFF; 64])]).await;
        assert!(replies.is_empty());
    }

    /// A frame from the interface next door is not this responder's, even
    /// though every responder subscribes to the same broadcast.
    #[tokio::test]
    async fn another_bus_is_not_answered() {
        let mut s = (*framed(Message::Control(Command::Hello))).clone();
        s.bus = SourceId(1);
        let replies = run(vec![Arc::new(s)]).await;
        assert!(replies.is_empty());
    }
}
