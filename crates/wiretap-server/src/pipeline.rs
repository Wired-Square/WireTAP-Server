//! The running server: the archives, the SocketCAN readers, the serial taps,
//! the GVRET listener, and the fan-out between them.
//!
//! Only the device half is Linux-only, and it is marked as such. Everything
//! else — the archives, the ingest listener, the shutdown — runs anywhere,
//! which is what lets an ingest-only deployment (a server with no local
//! hardware, fed by devices that push to it) be started and tested off a Pi.
//!
//! The shape is the Python's turned inside out. There, one loop `select`ed
//! over the CAN sockets and the listening socket and did every client's write
//! itself. Here each interface has a reader task publishing to a broadcast
//! channel, each client has a task subscribed to it, and transmits travel the
//! other way down an mpsc to a task that holds the sockets' writers. No client
//! can delay a read, and no read can delay a client.

use std::io;
#[cfg(target_os = "linux")]
use std::io::Write as _;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Instant;

#[cfg(target_os = "linux")]
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;
#[cfg(target_os = "linux")]
use tracing::error;
use tracing::{info, warn};
#[cfg(target_os = "linux")]
use wiretap_io::can::{self, CanError, CanEvent, CanFrame, CanTask, CanWriter};
#[cfg(target_os = "linux")]
use wiretap_model::{Direction, Sample, SourceId};

#[cfg(target_os = "linux")]
use crate::archive::Archive;
use crate::archive::Archives;
#[cfg(target_os = "linux")]
use crate::console;
#[cfg(target_os = "linux")]
use crate::gvret;
use crate::ingest;
#[cfg(target_os = "linux")]
use crate::settings::Device;
use crate::settings::{Mode, Settings, TestPattern};
#[cfg(target_os = "linux")]
use crate::source::{
    bus_count, index_for_bus, modbus::RtuTap, raw::RawTap, serial_tap, socketcan, Transmit,
};
#[cfg(target_os = "linux")]
use crate::testpattern;

#[cfg(target_os = "linux")]
/// Frames held for a GVRET client that is behind.
///
/// At the ~15k frames a second a busy 1 Mbit/s bus produces, this is about 70
/// ms of grace before a stalled client starts losing frames — long enough to
/// ride out a scheduling hiccup, short enough that the memory behind it is a
/// few tens of kilobytes.
const FRAME_BACKLOG: usize = 1024;

#[cfg(target_os = "linux")]
/// Transmits queued for the bus. Small on purpose: a GVRET client transmits
/// occasionally, so a backlog here means the bus or the interface is already
/// in trouble and the useful answer is to say so.
const TRANSMIT_QUEUE: usize = 64;

/// Why the server stopped, or would not start.
#[derive(Debug)]
pub enum RunError {
    OpenCan {
        iface: String,
        err: io::Error,
    },
    OpenSerial {
        path: String,
        err: io::Error,
    },
    Bind {
        addr: String,
        err: io::Error,
    },
    Cache {
        path: String,
        err: String,
    },
    /// No devices and no ingest listener.
    NothingToDo,
    /// The ingest listener is on, but there is nowhere for pushed frames to go.
    IngestNeedsForward,
    /// Devices were configured on a platform this build does not capture on.
    NoDeviceCapture,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        /// What an operator can grant, when permission is what refused it.
        fn hint(err: &io::Error, grant: &'static str) -> &'static str {
            if err.kind() == io::ErrorKind::PermissionDenied {
                grant
            } else {
                ""
            }
        }

        match self {
            // No capability hint: an AF_CAN raw socket needs none.
            Self::OpenCan { iface, err } => {
                write!(f, "cannot open {iface}: {err}")?;
                match err.raw_os_error() {
                    Some(libc::ENODEV) => write!(f, ". Check `ip link show {iface}`"),
                    Some(libc::EAFNOSUPPORT) => write!(
                        f,
                        ". Is the can_raw module loaded, and AF_CAN in the unit's \
                         RestrictAddressFamilies=?"
                    ),
                    _ => Ok(()),
                }
            }
            Self::OpenSerial { path, err } => write!(
                f,
                "cannot open {path}: {err}{}",
                hint(
                    err,
                    ". Add the user to dialout; under the packaged unit, DeviceAllow= names it"
                )
            ),
            Self::Bind { addr, err } => write!(
                f,
                "cannot listen on {addr}: {err}{}",
                hint(err, ". A port below 1024 needs CAP_NET_BIND_SERVICE")
            ),
            // Refusing to start is right: the cache is what stands between a
            // gateway outage and lost frames, and capturing without one would
            // look identical until the outage came.
            Self::Cache { path, err } => {
                write!(f, "cannot open the disk cache at {path}: {err}")
            }
            Self::NothingToDo => write!(
                f,
                "no devices configured and the ingest listener is disabled; nothing to do"
            ),
            // The Python refused the same combination, naming its own sink.
            // Accepting frames from a device and having nowhere to put them
            // would acknowledge them and then drop them, which is the one
            // thing at-least-once delivery must never do.
            Self::NoDeviceCapture => write!(
                f,
                "capturing from a device needs Linux; this build can still run an ingest-only \
                 server, which needs no local hardware"
            ),
            Self::IngestNeedsForward => write!(
                f,
                "the ingest listener is enabled but [forward] is not: frames pushed by a \
                 device would be acknowledged and then dropped. Configure a gateway."
            ),
        }
    }
}

/// Start whatever this configuration asks for, and run until a signal.
pub async fn run(settings: &Settings) -> Result<(), RunError> {
    if settings.devices.is_empty() && settings.ingest.is_none() {
        return Err(RunError::NothingToDo);
    }

    // The archives first: every device and every pushing device feeds one,
    // and none should start before there is somewhere for frames to go. Their
    // absence is warned about at startup and is a legitimate deployment — a
    // GVRET bridge that archives nothing.
    let archives = Archives::start_all(&settings.forwards(), settings.stats_interval).map_err(
        |(path, err)| RunError::Cache {
            path: path.display().to_string(),
            err: err.to_string(),
        },
    )?;

    let mut readers = JoinSet::new();
    if settings.devices.is_empty() {
        // No local hardware, so no sockets, no lines and no GVRET listener.
        info!("No devices configured; running ingest-only");
    } else {
        start_devices(settings, &archives, &mut readers).await?;
    }

    if let Some(ingest) = &settings.ingest {
        let Some(archive) = archives.handle(settings.default_database()) else {
            return Err(RunError::IngestNeedsForward);
        };
        let relays_raw_serial = settings.carries_raw_serial(settings.default_database());
        let server = ingest::Server::bind(ingest, archive, relays_raw_serial)
            .await
            .map_err(|err| RunError::Bind {
                addr: format!("{}:{}", ingest.host, ingest.port),
                err,
            })?;
        tokio::spawn(server.run());
    }

    shutdown().await;
    info!("Shutting down");

    // `TimeoutStopSec` in the unit is what gives the flush room.
    archives.shutdown(readers).await;
    Ok(())
}

#[cfg(target_os = "linux")]
/// Open every device, and start what reads them.
///
/// One broadcast for all of them: the GVRET bridge takes the CAN frames off
/// it and the console echo takes everything.
async fn start_devices(
    settings: &Settings,
    archives: &Archives,
    readers: &mut JoinSet<()>,
) -> Result<(), RunError> {
    let (frames, _) = broadcast::channel(FRAME_BACKLOG);
    if !settings.can_devices().is_empty() {
        start_capture(settings, archives, frames.clone(), readers).await?;
    }
    for (d, s) in settings.serial_devices() {
        let task = serial_tap::open(&d.interface, s).map_err(|err| RunError::OpenSerial {
            path: d.interface.clone(),
            err,
        })?;
        let catalogue = s
            .catalogue
            .as_ref()
            .map(|c| format!(", catalogue {c}"))
            .unwrap_or_default();
        info!(
            "Tapping {}[{}]  {s}  read-only{catalogue}",
            d.interface, d.bus.0
        );
        let framed_archive = archives.handle(&d.database);
        let raw_archive = s.raw_database.as_deref().and_then(|db| archives.handle(db));
        let frames = frames.clone();
        readers.spawn(serial_tap::drain(
            d.interface.clone(),
            task,
            s.framing.map(|_| RtuTap::new(d.bus, s)),
            s.raw_database.as_ref().map(|_| RawTap::new(d.bus, s.line)),
            move |sample| {
                let archive = match sample {
                    Sample::Serial(_) => &raw_archive,
                    _ => &framed_archive,
                };
                publish(sample, archive.as_ref(), &frames)
            },
        ));
    }
    if settings.echo_console {
        tokio::spawn(echo_loop(frames.subscribe(), settings.colour));
    }
    Ok(())
}

/// Off Linux there is nothing to capture from, but the rest of the server
/// still runs — which is what an ingest-only deployment is.
#[cfg(not(target_os = "linux"))]
async fn start_devices(
    _settings: &Settings,
    _archives: &Archives,
    _readers: &mut JoinSet<()>,
) -> Result<(), RunError> {
    Err(RunError::NoDeviceCapture)
}

#[cfg(target_os = "linux")]
/// A CAN device with its socket open and its archive found.
struct Opened {
    device: Device,
    writer: CanWriter,
    archive: Option<Archive>,
}

#[cfg(target_os = "linux")]
/// Open the CAN interfaces, bridge them to GVRET clients, and feed the archive.
///
/// Sockets first, then the listener, then the banner — the order the Python
/// used, so a permission problem is reported before anything claims to be
/// listening.
async fn start_capture(
    settings: &Settings,
    archives: &Archives,
    frames: broadcast::Sender<Arc<Sample>>,
    readers: &mut JoinSet<()>,
) -> Result<(), RunError> {
    let mut opened = Vec::new();
    let mut tasks = Vec::new();
    let mut rates = Vec::new();
    for d in settings.can_devices() {
        let task = socketcan::open(&d.interface, d.fd(), d.mode == Mode::Passive)
            .await
            .map_err(|err| RunError::OpenCan {
                iface: d.interface.clone(),
                err,
            })?;
        rates.push(socketcan::bitrates(&d.interface));
        opened.push(Opened {
            device: d.clone(),
            writer: task.writer(),
            archive: archives.handle(&d.database),
        });
        tasks.push(task);
    }
    let any_fd = settings.any_fd();

    let (transmits, transmit_queue) = mpsc::channel(TRANSMIT_QUEUE);
    let listener = gvret::Server::bind(
        &settings.host,
        settings.port,
        gvret::BusInfo {
            count: bus_count(opened.len(), settings.bus_offset),
            speeds: rates.iter().map(|r| r.nominal).collect(),
        },
        frames.clone(),
        transmits,
    )
    .await
    .map_err(|err| RunError::Bind {
        addr: format!("{}:{}", settings.host, settings.port),
        err,
    })?;

    info!(
        "Listening on {}:{}  mode={}  ifaces={}  rates={}{}",
        settings.host,
        settings.port,
        if any_fd { "GVRET+FD" } else { "GVRET" },
        join(
            opened
                .iter()
                .map(|o| format!("{}[{}]", o.device.interface, o.device.bus.0))
        ),
        join(rates.iter().map(|r| r.nominal)),
        if rates.iter().any(|r| r.data != 0) {
            format!("  drates={}", join(rates.iter().map(|r| r.data)))
        } else {
            String::new()
        },
    );

    for (o, task) in opened.iter().zip(tasks) {
        readers.spawn(read_loop(
            task,
            o.device.interface.clone(),
            o.device.bus,
            settings.default_dir,
            frames.clone(),
            o.archive.clone(),
        ));
    }
    if let Some(tp) = &settings.test_pattern {
        for line in test_pattern_warnings(settings, tp) {
            warn!("{line}");
        }
        for i in settings.armed_indices(tp) {
            let o = &opened[i];
            tokio::spawn(testpattern::responder_loop(
                o.writer.clone(),
                o.device.bus,
                o.device.fd(),
                frames.subscribe(),
            ));
        }
    }
    tokio::spawn(transmit_loop(opened, settings.bus_offset, transmit_queue));
    tokio::spawn(listener.run());
    Ok(())
}

/// What `start_capture` logs about arming the Test Pattern, at WARN, in order.
///
/// WARN throughout: this is the one part of the server that puts frames on a
/// bus nobody asked it to, and a box armed by accident, or one an operator
/// believes armed and is not, should say so in the journal's first screen.
pub fn test_pattern_warnings(settings: &Settings, tp: &TestPattern) -> Vec<String> {
    let can = settings.can_devices();
    let mut lines: Vec<String> = tp
        .ifaces
        .iter()
        .filter_map(|name| match can.iter().find(|d| &d.interface == name) {
            None => Some(format!(
                "Test Pattern: no interface named {name} is being captured"
            )),
            Some(d) if d.mode == Mode::Passive => Some(format!(
                "Test Pattern: {name} is passive, so it is not armed"
            )),
            Some(_) => None,
        })
        .collect();
    let armed = settings.armed_indices(tp);
    if armed.is_empty() {
        lines.push(
            "Test Pattern: enabled, but no configured interface matched; nothing is armed".into(),
        );
    } else {
        lines.push(format!(
            "Test Pattern responder ARMED on {} — this transmits on the bus",
            join(armed.iter().map(|&i| &can[i].interface))
        ));
        if !settings.any_fd() {
            lines.push(
                "Test Pattern: no device has fd on, so only the classic sweep can be answered"
                    .into(),
            );
        }
    }
    lines
}

fn join<T: std::fmt::Display>(parts: impl Iterator<Item = T>) -> String {
    parts.map(|p| p.to_string()).collect::<Vec<_>>().join(",")
}

#[cfg(target_os = "linux")]
/// Publish one interface's frames to everything downstream.
///
/// A loss is logged once and its end once, as the serial tap does, however
/// many retries lie between: "reopened" for a new socket, else "reading again"
/// at the first read.
async fn read_loop(
    mut task: CanTask,
    iface: String,
    bus: SourceId,
    dir: Direction,
    frames: broadcast::Sender<Arc<Sample>>,
    archive: Option<Archive>,
) {
    let mut reopening = false;
    let mut lost = false;
    while let Some(event) = task.next_event().await {
        match event {
            CanEvent::Connected(_) => {
                if std::mem::take(&mut reopening) {
                    info!("{iface}: reopened");
                    lost = false;
                }
            }
            CanEvent::Read(reads) => {
                if std::mem::take(&mut lost) {
                    info!("{iface}: reading again");
                }
                for read in reads {
                    // An own transmit goes to the archive alone: broadcast, it
                    // would echo to every GVRET client, and a Test Pattern
                    // responder would hear its own replies.
                    let own = read.direction == can::Direction::Tx;
                    let Some(sample) = socketcan::sample(read, bus, dir) else {
                        continue;
                    };
                    if !own {
                        publish(Sample::Can(sample), archive.as_ref(), &frames);
                    } else if let Some(archive) = &archive {
                        archive.enqueue(Arc::new(Sample::Can(sample)));
                    }
                }
            }
            CanEvent::Disconnected {
                error, consecutive, ..
            } => {
                if consecutive == 1 {
                    error!("{iface}: {}", socketcan::loss(&error));
                }
                // A read error keeps the socket, so nothing is reopened.
                reopening = !matches!(error, CanError::Read(_));
                lost = true;
            }
        }
    }
}

#[cfg(target_os = "linux")]
/// `--echo-console`, as a consumer like any other.
///
/// A subscriber rather than a call inside `read_loop`, for the same reason the
/// GVRET clients are: writing to stdout can block — a terminal over SSH, a pipe
/// into `less` — and the reader's job is to not miss frames. This way a slow
/// console drops its own lines and says how many, instead of stalling a runtime
/// worker and everything queued behind it.
async fn echo_loop(mut frames: broadcast::Receiver<Arc<Sample>>, colour: bool) {
    use broadcast::error::RecvError;

    let t0 = Instant::now();
    let mut line = String::new();
    loop {
        match frames.recv().await {
            Ok(sample) => {
                line.clear();
                let rel_us = t0.elapsed().as_micros() as u64;
                match &*sample {
                    Sample::Can(c) => console::format_line(&mut line, c, colour, rel_us),
                    Sample::Modbus(m) => console::format_modbus_line(&mut line, m, colour, rel_us),
                    Sample::Serial(r) => console::format_serial_line(&mut line, r, colour, rel_us),
                }
                // Ignored, as the Python ignored it: a console that has gone
                // away must not stop a capture. `Stdout` is line buffered and
                // the line ends in a newline, so this is already flushed.
                let _ = std::io::stdout().write_all(line.as_bytes());
            }
            Err(RecvError::Lagged(n)) => warn!("console echo dropped {n} frames"),
            Err(RecvError::Closed) => return,
        }
    }
}

#[cfg(target_os = "linux")]
/// Hand a sample to both consumers. Two disciplines: the archive's queue is
/// bounded and spills to disk, while a send error on the broadcast only means
/// nobody is watching.
fn publish(sample: Sample, archive: Option<&Archive>, frames: &broadcast::Sender<Arc<Sample>>) {
    let sample = Arc::new(sample);
    if let Some(archive) = archive {
        archive.enqueue(Arc::clone(&sample));
    }
    let _ = frames.send(sample);
}

#[cfg(target_os = "linux")]
/// Put what GVRET clients ask for onto the bus they named.
async fn transmit_loop(opened: Vec<Opened>, bus_offset: u8, mut queue: mpsc::Receiver<Transmit>) {
    while let Some(t) = queue.recv().await {
        // A bus this server does not have is dropped in silence, as the Python
        // dropped it: a client is free to address a device with more buses.
        let Some(o) = index_for_bus(t.bus, bus_offset, opened.len()).map(|i| &opened[i]) else {
            continue;
        };
        // Passive is a promise the operator made about the bus; a client
        // asking otherwise is told, once per attempt, and refused.
        if o.device.mode == Mode::Passive {
            warn!(
                "transmit on bus {} refused: {} is passive",
                t.bus.0, o.device.interface
            );
            continue;
        }
        // Classic: a GVRET `F1 00` carries no FD flag, so a client cannot ask
        // for one. The Test Pattern responder owns the FD path.
        let frame = CanFrame::data(0, t.arb_id, t.extended, false, false, t.data);
        match async { o.writer.send_when_ready(frame).await?.await }.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("transmit on bus {} failed: {e}", t.bus.0),
            Err(refused) => warn!("transmit on bus {} refused: {refused}", t.bus.0),
        }
    }
}

/// Wait for the signals systemd and a terminal send.
async fn shutdown() {
    use tokio::signal::unix::{signal, SignalKind};

    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        Err(e) => {
            warn!("cannot listen for SIGTERM, only Ctrl-C will stop this: {e}");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn test_pattern_log(args: &[&str], toml: Option<&str>) -> Vec<String> {
        let cli = crate::cli::Cli::parse_from(["wiretap-server"].iter().chain(args));
        let file = toml.map(|t| wiretap_model::config::FileConfig::parse(t).unwrap());
        let settings = Settings::resolve(&cli, file.as_ref(), &Default::default())
            .unwrap()
            .settings;
        test_pattern_warnings(&settings, settings.test_pattern.as_ref().unwrap())
    }

    #[test]
    fn the_test_pattern_log_names_what_it_armed() {
        assert_eq!(
            test_pattern_log(
                &["-i", "can0,can1", "--test-pattern-enable", "--can-fd"],
                None
            ),
            ["Test Pattern responder ARMED on can0,can1 — this transmits on the bus"]
        );
        assert_eq!(
            test_pattern_log(&["-i", "can0", "--test-pattern-enable"], None),
            [
                "Test Pattern responder ARMED on can0 — this transmits on the bus",
                "Test Pattern: no device has fd on, so only the classic sweep can be answered",
            ]
        );
    }

    #[test]
    fn the_test_pattern_log_says_when_a_named_interface_is_not_armed() {
        assert_eq!(
            test_pattern_log(
                &[
                    "-i",
                    "can0",
                    "--can-fd",
                    "--test-pattern-enable",
                    "--test-pattern-ifaces",
                    "can0,can9",
                ],
                None
            ),
            [
                "Test Pattern: no interface named can9 is being captured",
                "Test Pattern responder ARMED on can0 — this transmits on the bus",
            ]
        );
        assert_eq!(
            test_pattern_log(
                &["--test-pattern-enable", "--test-pattern-ifaces", "can0"],
                Some("[[device]]\nkind = \"can\"\ninterface = \"can0\"\nmode = \"passive\"\n")
            ),
            [
                "Test Pattern: can0 is passive, so it is not armed",
                "Test Pattern: enabled, but no configured interface matched; nothing is armed",
            ]
        );
    }

    #[test]
    fn the_test_pattern_log_says_when_nothing_is_armed() {
        assert_eq!(
            test_pattern_log(
                &[
                    "-i",
                    "can0",
                    "--test-pattern-enable",
                    "--test-pattern-ifaces",
                    "can9"
                ],
                None
            ),
            [
                "Test Pattern: no interface named can9 is being captured",
                "Test Pattern: enabled, but no configured interface matched; nothing is armed",
            ]
        );
    }

    fn open_can(err: io::Error) -> String {
        RunError::OpenCan {
            iface: "can0".to_owned(),
            err,
        }
        .to_string()
    }

    #[test]
    fn a_can_open_hints_at_the_fix_for_its_error_and_never_at_a_capability() {
        let missing = open_can(io::Error::from_raw_os_error(libc::ENODEV));
        assert!(
            missing.ends_with(". Check `ip link show can0`"),
            "{missing}"
        );

        let refused = open_can(io::Error::from_raw_os_error(libc::EAFNOSUPPORT));
        assert!(
            refused.contains("can_raw") && refused.contains("RestrictAddressFamilies="),
            "{refused}"
        );

        let denied = open_can(io::ErrorKind::PermissionDenied.into());
        assert_eq!(denied, "cannot open can0: permission denied");
    }
}
