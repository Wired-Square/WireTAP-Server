//! The capture path, against a virtual CAN interface.
//!
//! This is the drill for the half of the server that no development machine
//! can run. `source/socketcan.rs` and the CAN half of `pipeline.rs` are
//! `#[cfg(target_os = "linux")]`, there is no `vcan` on macOS and none in
//! Docker Desktop's kernel either — `ip link add dev vcan0 type vcan` answers
//! `Not supported` — so until this ran, opening a socket, reading a frame,
//! putting one on the bus and asking the kernel for a bitrate had never
//! executed anywhere.
//!
//! Ignored by default, because it needs an interface only root can create:
//!
//! ```sh
//! sudo modprobe vcan
//! sudo ip link add dev vcan0 type vcan && sudo ip link set up vcan0
//! cargo test -p wiretap-server --test vcan_loopback -- --ignored --test-threads=1
//! ```
//!
//! **`--test-threads=1` is not decoration.** Every test here shares one bus,
//! and vcan is multicast: run in parallel, one test's frames arrive in
//! another's reader. `WIRETAP_VCAN` names a different interface if `vcan0` is
//! taken.
//!
//! The other end of the bus is a plain `socketcan` socket rather than a second
//! [`CanReader`], so what a test asserts about a frame is never encoded and
//! decoded by the same code under test.

#![cfg(target_os = "linux")]

use std::time::{Duration, SystemTime};

use socketcan::{tokio::CanSocket, CanFrame, EmbeddedFrame, ExtendedId, Frame, StandardId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use wiretap_model::{Direction, SourceId};
use wiretap_server::pipeline;
use wiretap_server::settings::{LogLevel, Settings};
use wiretap_server::source::socketcan::{detect_bitrates, CanReader};
use wiretap_server::source::{system_time_to_us, Bitrates};

/// Long enough to absorb a loaded CI runner, short enough that a wedged read
/// fails the test instead of hanging the job.
const PATIENCE: Duration = Duration::from_secs(10);

fn iface() -> String {
    std::env::var("WIRETAP_VCAN").unwrap_or_else(|_| "vcan0".to_string())
}

fn standard(id: u32) -> StandardId {
    StandardId::new(id as u16).expect("a standard id")
}

/// The other end of the bus: what a test uses to put frames on the wire and to
/// see what the server put there.
struct Bus(CanSocket);

impl Bus {
    /// Open before the code under test starts reading. vcan delivers a frame
    /// only to the sockets that were already open when it was written, so a
    /// socket opened afterwards sees nothing and the test hangs.
    fn open() -> Self {
        Self(CanSocket::open(&iface()).expect("open the bus; is vcan0 up?"))
    }

    async fn send(&self, frame: CanFrame) {
        self.0.write_frame(frame).await.expect("put it on the bus");
    }

    /// The next frame on the bus, or a failed test.
    async fn next(&self) -> CanFrame {
        tokio::time::timeout(PATIENCE, self.0.read_frame())
            .await
            .expect("a frame reached the bus in time")
            .expect("read it back")
    }
}

/// Read until the frame with `arb_id` arrives, failing if it never does.
///
/// Scanning rather than taking the first frame keeps a test honest on a bus
/// that carries anything else — which a `vcan0` left over from a previous run
/// may well do.
async fn read_until(reader: &CanReader, arb_id: u32) -> wiretap_model::CanSample {
    let scan = async {
        loop {
            let sample = reader.recv().await.expect("read a frame");
            if sample.arb_id == arb_id {
                return sample;
            }
        }
    };
    tokio::time::timeout(PATIENCE, scan)
        .await
        .unwrap_or_else(|_| panic!("no frame with id {arb_id:#x} arrived"))
}

#[tokio::test]
#[ignore = "needs a vcan interface; see the module docs"]
async fn a_frame_on_the_bus_becomes_a_sample() {
    let bus = Bus::open();
    let reader = CanReader::open(&iface(), SourceId(3), Direction::Rx, false).expect("open");

    let before = system_time_to_us(SystemTime::now());
    bus.send(CanFrame::new(standard(0x123), &[0xDE, 0xAD, 0xBE, 0xEF]).expect("a frame"))
        .await;
    let sample = read_until(&reader, 0x123).await;

    assert_eq!(sample.data, [0xDE, 0xAD, 0xBE, 0xEF]);
    assert!(!sample.extended);
    assert!(!sample.is_fd);
    assert_eq!(
        sample.bus,
        SourceId(3),
        "the bus it was opened as, not can0"
    );
    assert_eq!(sample.dir, Direction::Rx);
    // The kernel's receive time, converted: the point is that it is a real
    // clock reading rather than the zero an unset `SO_TIMESTAMP` would give.
    assert!(
        sample.ts_us >= before && sample.ts_us < before + 60_000_000,
        "timestamped at capture: {} against {before}",
        sample.ts_us
    );
}

#[tokio::test]
#[ignore = "needs a vcan interface; see the module docs"]
async fn an_extended_id_keeps_its_flag_and_all_29_bits() {
    let bus = Bus::open();
    let reader = CanReader::open(&iface(), SourceId(0), Direction::Rx, false).expect("open");

    let id = 0x1AB_CDEF;
    bus.send(
        CanFrame::new(ExtendedId::new(id).expect("an extended id"), &[0x01]).expect("a frame"),
    )
    .await;
    let sample = read_until(&reader, id).await;

    // `raw_id` has to have stripped the EFF flag for the id to match at all,
    // which is the half of this that a bug would break silently.
    assert!(sample.extended, "the flag survives the read");
    assert_eq!(sample.data, [0x01]);
}

/// The one deliberate difference from the Python on this path, recorded in
/// `docs/porting-notes.md`: it passed remote frames through as zero-length
/// data frames, and this drops them.
#[tokio::test]
#[ignore = "needs a vcan interface; see the module docs"]
async fn a_remote_frame_is_skipped_and_does_not_stall_the_reader() {
    let bus = Bus::open();
    let reader = CanReader::open(&iface(), SourceId(0), Direction::Rx, false).expect("open");

    bus.send(CanFrame::new_remote(standard(0x7FE), 8).expect("a remote frame"))
        .await;
    bus.send(CanFrame::new(standard(0x7FF), &[0x55]).expect("a frame"))
        .await;

    // Scanning for the data frame proves both halves at once: the remote frame
    // was not surfaced, and skipping it did not swallow what followed. This
    // cannot delegate to `read_until` — that skips every id it is not looking
    // for, which is exactly the thing being checked for here.
    let scan = async {
        loop {
            let sample = reader.recv().await.expect("read a frame");
            assert_ne!(sample.arb_id, 0x7FE, "a remote frame reached the archive");
            if sample.arb_id == 0x7FF {
                return sample;
            }
        }
    };
    let sample = tokio::time::timeout(PATIENCE, scan)
        .await
        .expect("the data frame behind the remote frame arrived");
    assert_eq!(sample.data, [0x55]);
}

/// `transmit` is the only path where a bug puts a frame on a *physical* bus
/// that nobody asked for, which is why `index_for_bus` exists at all.
#[tokio::test]
#[ignore = "needs a vcan interface; see the module docs"]
async fn a_transmit_reaches_the_bus_as_the_client_asked() {
    let bus = Bus::open();
    let reader = CanReader::open(&iface(), SourceId(0), Direction::Rx, false).expect("open");

    reader
        .transmit(0x321, false, &[0xAA, 0xBB, 0xCC])
        .await
        .expect("transmit");
    let frame = bus.next().await;
    assert_eq!(frame.raw_id(), 0x321);
    assert!(!frame.is_extended());
    assert_eq!(frame.data(), [0xAA, 0xBB, 0xCC]);

    reader
        .transmit(0x1AB_CDEF, true, &[0x01])
        .await
        .expect("transmit an extended frame");
    let frame = bus.next().await;
    assert_eq!(frame.raw_id(), 0x1AB_CDEF);
    assert!(frame.is_extended());

    // The clamp the doc comment calls a local guarantee rather than a remote
    // one: the decoder already bounds a client's payload, and this is what
    // makes that true regardless of the decoder.
    reader
        .transmit(0x100, false, &[0xFF; 12])
        .await
        .expect("a long payload is truncated, not rejected");
    assert_eq!(bus.next().await.data(), [0xFF; 8]);
}

#[tokio::test]
#[ignore = "needs a vcan interface; see the module docs"]
async fn an_unrepresentable_id_is_refused_rather_than_truncated() {
    let reader = CanReader::open(&iface(), SourceId(0), Direction::Rx, false).expect("open");

    // 0x800 needs 12 bits; a standard id has 11. Truncating would transmit
    // 0x000 — a frame on the bus with the wrong identifier.
    let err = reader
        .transmit(0x800, false, &[0x00])
        .await
        .expect_err("refused");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    let err = reader
        .transmit(1 << 29, true, &[0x00])
        .await
        .expect_err("refused");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

/// A vcan interface has no bit timing to report, which is exactly the fallback
/// the Python produced for an interface it could not read — so this asserts
/// the path that a real `can0` will not take.
#[tokio::test]
#[ignore = "needs a vcan interface; see the module docs"]
async fn an_interface_with_no_timing_reports_the_fallback_bitrate() {
    assert_eq!(detect_bitrates(&iface()), Bitrates::FALLBACK);
    assert_eq!(
        detect_bitrates("wiretap-no-such-iface"),
        Bitrates::FALLBACK,
        "an interface that does not exist is the netlink error path"
    );
}

/// The whole daemon, wired as it ships: a frame on the bus reaches a GVRET
/// client as protocol bytes, and a client's `F1 00` reaches the bus.
///
/// This is the only test that runs `pipeline`'s CAN half — `start_capture`,
/// `read_loop`, `transmit_loop` and, because `echo_console` is on, `echo_loop`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a vcan interface; see the module docs"]
async fn the_pipeline_bridges_the_bus_to_a_gvret_client_and_back() {
    let bus = Bus::open();
    let port = free_port().await;
    let settings = Settings {
        ifaces: vec![iface()],
        host: "127.0.0.1".to_string(),
        port,
        bus_offset: 0,
        // On, so the console echo is exercised too: it is a subscriber like
        // any other and has never run either.
        echo_console: true,
        colour: false,
        default_dir: Direction::Rx,
        can_fd: false,
        log_level: LogLevel::Info,
        // No periodic stats: this test is over in under a second.
        stats_interval: 0.0,
        ingest: None,
        // No gateway. What frames do after the fan-out is the outage drill's
        // subject; this one is about the bus and the socket.
        forward: None,
    };
    // `run` returns only on a signal, so the task outlives this body and the
    // runtime drops it. The handle is held rather than detached because every
    // way this can fail to start — vcan0 down, the port taken — would
    // otherwise surface as `connect` timing out, which names the wrong thing.
    let mut server = tokio::spawn(async move { pipeline::run(&settings).await });
    let mut client = tokio::select! {
        stopped = &mut server => panic!("the server stopped before listening: {stopped:?}"),
        client = connect(port) => client,
    };
    // What SavvyCAN opens with: the binary-mode latch, then "who are you".
    client
        .write_all(&[0xE7, 0xE7, 0xF1, 0x07])
        .await
        .expect("write the handshake");
    assert_eq!(
        read_exactly(&mut client, 8).await,
        [0xF1, 0x07, 0x90, 0x01, 0x01, 0x00, 0x00, 0x00],
        "device info, which is also the proof the handshake latched"
    );

    // Bus to client.
    bus.send(CanFrame::new(standard(0x2A0), &[0x11, 0x22]).expect("a frame"))
        .await;
    let frame = read_exactly(&mut client, 12 + 2).await;
    assert_eq!(frame[0..2], [0xF1, 0x00], "a frame, not a reply");
    assert_eq!(
        u32::from_le_bytes(frame[6..10].try_into().unwrap()),
        0x2A0,
        "the id it was sent with"
    );
    // The high nibble is the bus and the low nibble the DLC code.
    assert_eq!(frame[10], 0x02, "bus 0, two bytes");
    assert_eq!(&frame[11..13], &[0x11, 0x22]);

    // Client to bus: `F1 00`, id 0x321 little-endian, bus 0, two bytes.
    client
        .write_all(&[0xF1, 0x00, 0x21, 0x03, 0x00, 0x00, 0x00, 0x02, 0xC0, 0xDE])
        .await
        .expect("write a transmit");
    // Reaching this bus at all is the assertion: the client addressed bus 0,
    // and `index_for_bus` had to resolve that to the interface behind it.
    let sent = bus.next().await;
    assert_eq!(sent.raw_id(), 0x321);
    assert_eq!(sent.data(), [0xC0, 0xDE]);
}

/// A port nothing else on this machine is using. The usual bind-and-release
/// race, taken deliberately: a fixed port collides with the previous run of
/// this suite, which is the failure that actually happens.
async fn free_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("bound")
        .port()
}

/// Connect once the listener is up. `run` binds it from a spawned task, so the
/// first attempt can legitimately arrive first.
async fn connect(port: u16) -> TcpStream {
    let dial = async {
        loop {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(stream) => return stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    };
    tokio::time::timeout(PATIENCE, dial)
        .await
        .expect("the GVRET listener came up")
}

async fn read_exactly(stream: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    tokio::time::timeout(PATIENCE, stream.read_exact(&mut buf))
        .await
        .expect("the server answered in time")
        .expect("it sent enough bytes");
    buf
}
