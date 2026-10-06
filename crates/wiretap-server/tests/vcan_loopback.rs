//! The capture path, against a virtual CAN interface.
//!
//! This is the drill for the half of the server that no development machine
//! can run. `source/socketcan.rs` and the CAN half of `pipeline.rs` are
//! `#[cfg(target_os = "linux")]`, there is no `vcan` on macOS and none in
//! Docker Desktop's kernel either — `ip link add dev vcan0 type vcan` answers
//! `Not supported` — so until this ran, opening a socket, reading a frame,
//! putting one on the bus and asking the kernel for a bitrate had never
//! executed anywhere. The socket itself is `wiretap-io`'s, with drills of its
//! own; these are about what the server does with it.
//!
//! Ignored by default, because it needs an interface only root can create:
//!
//! ```sh
//! sudo modprobe vcan
//! sudo ip link add dev vcan0 type vcan && sudo ip link set up vcan0
//! cargo test -p wiretap-server --test vcan_loopback -- --ignored --test-threads=1
//! ```
//!
//! The drills that take an interface down or delete it make their own,
//! [`Scratch`], with `ip` as root or `sudo -n ip` otherwise, as CI's runner
//! allows. Where neither works they are skipped, and under CI they fail.
//!
//! **`--test-threads=1` is not decoration.** Every test here shares one bus,
//! and vcan is multicast: run in parallel, one test's frames arrive in
//! another's reader. `WIRETAP_VCAN` names a different interface if `vcan0` is
//! taken.
//!
//! The other end of the bus is a plain `socketcan` socket rather than a second
//! `wiretap-io` task, so what a test asserts about a frame is never encoded and
//! decoded by the same code under test.

#![cfg(target_os = "linux")]

use std::io;
use std::process::Command as Shell;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use socketcan::{
    tokio::CanFdSocket, CanAnyFrame, CanFdFrame, CanFrame, EmbeddedFrame, Frame, SocketOptions,
    StandardId,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use wiretap_io::can::{CanEvent, CanRead, CanTask};
use wiretap_model::{Direction, Sample, Secret, SourceId};
use wiretap_protocol::testpattern::{
    encode, sweep_payload, Command, Flags, Message, ID_CONTROL, SWEEP_ECHO_BASE, SWEEP_REQUEST_BASE,
};
use wiretap_server::cache::{FrameCache, SqliteCache};
use wiretap_server::pipeline;
use wiretap_server::settings::{
    Batching, Device, DeviceKind, Forward, LogLevel, Settings, TestPattern,
};
use wiretap_server::source::{socketcan as server_can, system_time_to_us, Bitrates};

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
///
/// An FD socket even for the classic tests, because a classic `CAN_RAW` socket
/// is never delivered an FD frame at all: an FD assertion through one fails on
/// the read timeout, blaming the sender for a frame the socket was never going
/// to be given. `CanFdSocket::open` sets `CAN_RAW_FD_FRAMES`, so this receives
/// both widths, and `CanAnyFrame` answers `raw_id`, `data` and `is_extended`
/// exactly as `CanFrame` did.
///
/// **This socket never receives its own transmissions.**
/// `CAN_RAW_RECV_OWN_MSGS` defaults off and only the server's socket sets it,
/// while `CAN_RAW_LOOPBACK` defaults on — so what this reads is always some
/// *other* socket's frame, never the one it just sent. Every test here depends
/// on that in one direction or the other.
struct Bus(CanFdSocket);

impl Bus {
    /// Open before the code under test starts reading. vcan delivers a frame
    /// only to the sockets that were already open when it was written, so a
    /// socket opened afterwards sees nothing and the test hangs.
    fn open() -> Self {
        Self::on(&iface())
    }

    fn on(iface: &str) -> Self {
        Self(CanFdSocket::open(iface).expect("open the bus; is the interface up?"))
    }

    async fn send(&self, frame: CanFrame) {
        self.0.write_frame(&frame).await.expect("put it on the bus");
    }

    /// A CAN FD frame, which `CanFrame` cannot represent.
    async fn send_fd(&self, arb_id: u32, data: &[u8]) {
        let frame = CanFdFrame::new(standard(arb_id), data).expect("an FD frame");
        self.0.write_frame(&frame).await.expect("put it on the bus");
    }

    /// The next frame on the bus, or a failed test.
    async fn next(&self) -> CanAnyFrame {
        tokio::time::timeout(PATIENCE, self.0.read_frame())
            .await
            .expect("a frame reached the bus in time")
            .expect("read it back")
    }

    /// Whatever broke the silence within `d`, or `None` if nothing did.
    ///
    /// Separate from [`Bus::next`] because a test that *wants* silence must not
    /// treat the timeout as a failure.
    async fn interruption(&self, d: Duration) -> Option<CanAnyFrame> {
        tokio::time::timeout(d, self.0.read_frame())
            .await
            .ok()
            .map(|r| r.expect("read it back"))
    }
}

/// A server on `iface` with no gateway: what frames do after the fan-out is
/// the outage drill's subject, and these tests are about the bus and the socket.
fn settings(iface: &str, port: u16, test_pattern: Option<TestPattern>) -> Settings {
    Settings {
        devices: vec![Device::can(iface, false, "")],
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
        // No periodic stats: these tests are over in under a second.
        stats_interval: 0.0,
        ingest: None,
        forward: None,
        test_pattern,
    }
}

/// Start a server and return only once its CAN sockets are certainly open.
///
/// **The barrier is the point.** vcan delivers a frame only to sockets that
/// were already open when it was written, so a test that writes to the bus
/// straight after `tokio::spawn` is racing the socket's open — and losing the
/// race looks like the code under test saying nothing. The GVRET handshake is
/// the proxy: the listener is spawned *after* the readers in `start_capture`,
/// so a device-info reply proves both are up. `ci.yml` waits on the same port
/// for the same reason.
///
/// The join handle comes back so a caller can tell "the server refused to
/// start" from "the server said nothing", which are the same symptom otherwise.
/// The client comes back latched, for a drill that watches what it is sent.
async fn server_listening(
    settings: Settings,
) -> (
    tokio::task::JoinHandle<Result<(), pipeline::RunError>>,
    TcpStream,
) {
    let port = settings.port;
    let mut server = tokio::spawn(async move { pipeline::run(&settings).await });
    let mut client = tokio::select! {
        stopped = &mut server => panic!("the server stopped before listening: {stopped:?}"),
        client = connect(port) => client,
    };
    client
        .write_all(&[0xE7, 0xE7, 0xF1, 0x07])
        .await
        .expect("write the handshake");
    assert_eq!(
        read_exactly(&mut client, 8).await,
        [0xF1, 0x07, 0x90, 0x01, 0x01, 0x00, 0x00, 0x00],
        "device info, which is also the proof the readers are open"
    );
    (server, client)
}

/// Read until a frame with each of `arb_ids` has arrived, failing if one never
/// does, and return them in that order.
///
/// Scanning rather than taking the first frame keeps a test honest on a bus
/// that carries anything else — which a `vcan0` left over from a previous run
/// may well do. Frames sent back to back can share one `Read`.
async fn read_until(task: &mut CanTask, arb_ids: &[u32]) -> Vec<CanRead> {
    let mut found: Vec<Option<CanRead>> = arb_ids.iter().map(|_| None).collect();
    let scan = async {
        while found.iter().any(Option::is_none) {
            if let Some(CanEvent::Read(reads)) = task.next_event().await {
                for read in reads {
                    if let Some(i) = arb_ids.iter().position(|&id| id == read.frame.arb_id) {
                        found[i].get_or_insert(read);
                    }
                }
            }
        }
    };
    tokio::time::timeout(PATIENCE, scan)
        .await
        .unwrap_or_else(|_| panic!("not every one of {arb_ids:#x?} arrived"));
    found.into_iter().flatten().collect()
}

#[tokio::test]
#[ignore = "needs a vcan interface; see the module docs"]
async fn a_frame_on_the_bus_becomes_a_sample() {
    let bus = Bus::open();
    let mut task = server_can::open(&iface(), false, false)
        .await
        .expect("open");

    let before = system_time_to_us(SystemTime::now());
    bus.send(CanFrame::new(standard(0x123), &[0xDE, 0xAD, 0xBE, 0xEF]).expect("a frame"))
        .await;
    let [read] = read_until(&mut task, &[0x123])
        .await
        .try_into()
        .expect("one read");
    let sample = server_can::sample(read, SourceId(3), Direction::Rx);

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
async fn a_remote_frame_is_a_sample_with_the_code_it_requests() {
    let bus = Bus::open();
    let mut task = server_can::open(&iface(), false, false)
        .await
        .expect("open");

    bus.send(CanFrame::new_remote(standard(0x7FE), 8).expect("a remote frame"))
        .await;
    bus.send(CanFrame::new(standard(0x7FF), &[0x55]).expect("a frame"))
        .await;

    let [remote, data] = read_until(&mut task, &[0x7FE, 0x7FF])
        .await
        .try_into()
        .expect("two reads");
    let remote = server_can::sample(remote, SourceId(0), Direction::Rx);
    assert_eq!((remote.rtr, remote.data.len()), (Some(8), 0));
    let sample = server_can::sample(data, SourceId(0), Direction::Rx);
    assert_eq!((sample.rtr, sample.data), (None, vec![0x55]));
}

/// A vcan interface has no bit timing to report, which is exactly the fallback
/// the Python produced for an interface it could not read — so this asserts
/// the path that a real `can0` will not take.
#[tokio::test]
#[ignore = "needs a vcan interface; see the module docs"]
async fn an_interface_with_no_timing_reports_the_fallback_bitrate() {
    assert_eq!(server_can::bitrates(&iface()), Bitrates::FALLBACK);
    assert_eq!(
        server_can::bitrates("wiretap-no-such-iface"),
        Bitrates::FALLBACK,
        "an interface that does not exist is the netlink error path"
    );
}

/// The whole daemon, wired as it ships: a frame on the bus reaches a GVRET
/// client as protocol bytes, and a client's `F1 00` reaches the bus.
///
/// With the drills below, this is what runs `pipeline`'s CAN half —
/// `start_capture`, `read_loop`, `transmit_loop` and, because `echo_console`
/// is on, `echo_loop`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a vcan interface; see the module docs"]
async fn the_pipeline_bridges_the_bus_to_a_gvret_client_and_back() {
    let bus = Bus::open();
    let port = free_port().await;
    let settings = settings(&iface(), port, None);
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

/// A GVRET client's frame, as the kernel hands it back: archived once as `tx`,
/// and never broadcast, which would echo it to the client that sent it.
///
/// The archive is read from its disk cache: the gateway is a port nothing
/// listens on, and the queue spills at its first frame.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a vcan interface; see the module docs"]
async fn a_gvret_transmit_is_archived_once_as_tx_and_not_broadcast() {
    let bus = Bus::open();
    let dir = std::env::temp_dir().join(format!("wiretap-vcan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cache_path = dir.join("cache.db");
    let mut settings = settings(&iface(), free_port().await, None);
    settings.forward = Some(Forward {
        host: "127.0.0.1".to_string(),
        port: free_port().await,
        api_key: Secret::new("unused"),
        database: String::new(),
        batching: Batching {
            size: 500,
            flush_interval: 0.1,
            queue_size: 100,
            cache_path: cache_path.clone(),
            cache_origin: None,
            cache_max_mb: 10,
            queue_flush_pct: 1,
            legacy_cache_path: None,
        },
        daemon_id: "vcan".into(),
        devices: Vec::new(),
        raw_serial: false,
    });
    let (_server, mut client) = server_listening(settings).await;

    client
        .write_all(&[0xF1, 0x00, 0x21, 0x03, 0x00, 0x00, 0x00, 0x02, 0xC0, 0xDE])
        .await
        .expect("write a transmit");
    assert_eq!(bus.next().await.raw_id(), 0x321);
    let echoed = tokio::time::timeout(Duration::from_millis(500), client.read_u8()).await;
    assert!(echoed.is_err(), "the client was sent {echoed:?}");

    let archived = || {
        let mut cache = SqliteCache::open(&cache_path, 10).expect("the cache");
        cache
            .oldest(1000)
            .expect("read the cache")
            .into_iter()
            .filter_map(|c| match &*c.sample {
                Sample::Can(s) if s.arb_id == 0x321 => Some(s.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let spilled = async {
        while archived().is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::time::timeout(PATIENCE, spilled)
        .await
        .expect("the transmit reached the archive");
    tokio::time::sleep(RETRIES).await;
    let archived = archived();
    assert_eq!(archived.len(), 1, "{archived:?}");
    assert_eq!(archived[0].dir, Direction::Tx);
    assert_eq!(archived[0].data, [0xC0, 0xDE]);
    let _ = std::fs::remove_dir_all(&dir);
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

/// The responder end to end, on the half that matters: a **CAN FD sweep**.
///
/// The unit tests drive the same loop against a recording sink and
/// `an_fd_transmit_keeps_its_whole_payload` drives the socket directly. Only
/// this wires them together — broadcast, state machine, `ReplySink`, socket —
/// and so only this would catch the reply's `fd` flag being dropped on the way
/// to `transmit`, which every other test in the tree passes without.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a vcan interface; see the module docs"]
async fn an_armed_responder_echoes_an_fd_sweep_on_the_bus() {
    let bus = Bus::open();
    let mut settings = settings(
        &iface(),
        free_port().await,
        Some(TestPattern {
            ifaces: vec![iface()],
        }),
    );
    // Without this the reader drops every FD frame before the broadcast, so
    // the sweep below would never reach the responder at all.
    settings.devices[0].kind = DeviceKind::Can { fd: true };
    let _server = server_listening(settings).await;

    // Bind a run first: a responder echoes sweeps only inside one.
    let start = encode(
        Message::Control(Command::Start { mode: 0, run: 1 }),
        Flags::new(0, 1),
    );
    bus.send(CanFrame::new(standard(ID_CONTROL), &start).expect("a control frame"))
        .await;

    // Code 15 is 64 bytes — the longest thing the protocol can ask for, and the
    // one the old 8-byte clamp would have answered with eight.
    let payload = sweep_payload(15, true);
    assert_eq!(payload.len(), 64, "the crate's own idea of code 15");
    bus.send_fd(SWEEP_REQUEST_BASE + 15, &payload).await;

    let echo = bus.next().await;
    assert_eq!(echo.raw_id(), SWEEP_ECHO_BASE + 15, "the echo id");
    assert_eq!(echo.data(), &payload[..], "all 64 bytes came back");
    let CanAnyFrame::Fd(echo) = echo else {
        panic!("the echo came back downgraded to classic");
    };
    assert!(!echo.is_brs(), "and at one bitrate, as it always went out");
}

/// **Disabled is silent.** The responder is the only thing here that transmits
/// unbidden, so "off" has to mean nothing reaches the bus at all.
///
/// This assertion is only worth anything because [`server_listening`] has
/// already proved the reader is open: without that barrier the frame would be
/// written before anything could hear it, and the test would pass just as
/// happily with the responder armed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a vcan interface; see the module docs"]
async fn a_disabled_responder_transmits_nothing() {
    let bus = Bus::open();
    let _server = server_listening(settings(&iface(), free_port().await, None)).await;

    let hello = encode(Message::Control(Command::Hello), Flags::new(0, 0));
    bus.send(CanFrame::new(standard(ID_CONTROL), &hello).expect("a control frame"))
        .await;

    let heard = bus.interruption(Duration::from_millis(500)).await;
    assert!(
        heard.is_none(),
        "a disabled responder put {:?} on the bus",
        heard.map(|f| f.raw_id())
    );
}

/// `ip`, as root or through `sudo -n`.
fn ip(args: &[&str]) -> bool {
    let ran = |command: &mut Shell| command.status().is_ok_and(|s| s.success());
    ran(Shell::new("ip").args(args)) || ran(Shell::new("sudo").arg("-n").arg("ip").args(args))
}

/// A vcan interface of a drill's own, so taking it down or deleting it
/// disturbs no other test's bus. Deleted when dropped.
struct Scratch;

impl Scratch {
    const NAME: &'static str = "wiretap-drill";

    /// `None`, and the drill skipped, where this host won't let it make one.
    fn new() -> Option<Self> {
        Self::delete();
        if Self::add() {
            return Some(Self);
        }
        assert!(
            std::env::var_os("CI").is_none(),
            "cannot add {}: CI needs the vcan module and passwordless sudo",
            Self::NAME
        );
        eprintln!(
            "skipped: adding {} needs root or passwordless sudo",
            Self::NAME
        );
        None
    }

    fn add() -> bool {
        ip(&["link", "add", "dev", Self::NAME, "type", "vcan"]) && Self::set("up")
    }

    fn set(state: &str) -> bool {
        ip(&["link", "set", state, Self::NAME])
    }

    fn delete() -> bool {
        ip(&["link", "del", Self::NAME])
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        Self::delete();
    }
}

/// What the server logs, for a drill about what an operator is told. It
/// captures this thread only, so those drills run on a current-thread runtime,
/// where every task the server spawns runs on it too.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Log {
    fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
        let log = Self::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        (log, tracing::subscriber::set_default(subscriber))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    async fn wait_for(&self, line: &str) {
        let wait = async {
            while !self.text().contains(line) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(PATIENCE, wait)
            .await
            .unwrap_or_else(|_| panic!("never logged {line:?}:\n{}", self.text()));
    }

    fn errors(&self) -> usize {
        self.text().matches("ERROR").count()
    }
}

/// Long enough for anything retried about once a second to have repeated.
const RETRIES: Duration = Duration::from_secs(3);

/// The id of the next two-byte frame a GVRET client is sent.
async fn next_frame_id(client: &mut TcpStream) -> u32 {
    let frame = read_exactly(client, 12 + 2).await;
    assert_eq!(frame[0..2], [0xF1, 0x00], "a frame, not a reply");
    u32::from_le_bytes(frame[6..10].try_into().unwrap())
}

/// A downed interface keeps its socket, and frames come back through it once
/// the interface is up. The loss is logged once, not once a retry, and so is
/// its end.
#[tokio::test]
#[ignore = "needs a vcan interface; see the module docs"]
async fn a_downed_interface_is_logged_once_and_capture_resumes_once_it_is_up() {
    let Some(_scratch) = Scratch::new() else {
        return;
    };
    let (log, _guard) = Log::capture();
    let (_server, mut client) =
        server_listening(settings(Scratch::NAME, free_port().await, None)).await;

    assert!(Scratch::set("down"));
    log.wait_for("wiretap-drill: interface is down").await;
    tokio::time::sleep(RETRIES).await;
    assert!(Scratch::set("up"));

    // Opened only now: a socket open across the down holds its error for the
    // next write.
    Bus::on(Scratch::NAME)
        .send(CanFrame::new(standard(0x2A1), &[1, 2]).expect("a frame"))
        .await;
    assert_eq!(next_frame_id(&mut client).await, 0x2A1);
    log.wait_for("wiretap-drill: reading again").await;
    assert_eq!(log.errors(), 1, "{}", log.text());
    assert_eq!(log.text().matches("reading again").count(), 1);
    assert!(!log.text().contains("reopened"), "{}", log.text());
}

/// An adapter unplugged and plugged back is an interface deleted and made
/// again under a new index, which the old socket never reads again: the
/// server reopens it by name.
#[tokio::test]
#[ignore = "needs a vcan interface; see the module docs"]
async fn a_deleted_interface_is_reopened_by_name_when_it_comes_back() {
    let Some(_scratch) = Scratch::new() else {
        return;
    };
    let (log, _guard) = Log::capture();
    let (_server, mut client) =
        server_listening(settings(Scratch::NAME, free_port().await, None)).await;

    assert!(Scratch::delete());
    log.wait_for("wiretap-drill: interface is gone").await;
    tokio::time::sleep(RETRIES).await;
    assert!(Scratch::add());
    log.wait_for("wiretap-drill: reopened").await;

    Bus::on(Scratch::NAME)
        .send(CanFrame::new(standard(0x2A2), &[1, 2]).expect("a frame"))
        .await;
    assert_eq!(next_frame_id(&mut client).await, 0x2A2);
    assert_eq!(log.errors(), 1, "{}", log.text());
    assert!(!log.text().contains("reading again"), "{}", log.text());
}

/// Evidence for why the reader was replaced, not a guard on this server: the
/// old one read through socketcan's tokio socket, which waits on readability
/// alone. A downed interface raises only an error, so it heard of the loss
/// only when a frame arrived after the interface was back up.
#[tokio::test]
#[ignore = "needs a vcan interface; see the module docs"]
async fn the_replaced_reader_heard_of_a_downed_interface_only_from_the_next_frame() {
    let Some(_scratch) = Scratch::new() else {
        return;
    };
    let old = CanFdSocket::open(Scratch::NAME).expect("open");
    old.set_recv_timestamp(true).expect("kernel stamps");

    assert!(Scratch::set("down"));
    let heard = tokio::time::timeout(RETRIES, old.read_frame_with_timestamp()).await;
    assert!(heard.is_err(), "it heard the loss after all: {heard:?}");

    assert!(Scratch::set("up"));
    Bus::on(Scratch::NAME)
        .send(CanFrame::new(standard(0x2A3), &[1, 2]).expect("a frame"))
        .await;
    let late = tokio::time::timeout(PATIENCE, old.read_frame_with_timestamp())
        .await
        .expect("woken by the frame");
    assert_eq!(
        late.expect_err("the loss, before the frame").kind(),
        io::ErrorKind::NetworkDown
    );
}
