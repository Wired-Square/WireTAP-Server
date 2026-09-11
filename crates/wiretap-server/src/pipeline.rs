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
//! other way down an mpsc to a task that owns the sockets. No client can delay
//! a read, and no read can delay a client.

use std::io;
#[cfg(target_os = "linux")]
use std::io::Write as _;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant, SystemTime};

#[cfg(target_os = "linux")]
use tokio::sync::{broadcast, mpsc};
#[cfg(target_os = "linux")]
use tracing::error;
use tracing::{info, warn};
#[cfg(target_os = "linux")]
use wiretap_model::{CanSample, Direction, Sample};

#[cfg(target_os = "linux")]
use crate::archive::Archive;
use crate::archive::Archives;
#[cfg(target_os = "linux")]
use crate::console;
#[cfg(target_os = "linux")]
use crate::gvret;
use crate::ingest;
use crate::settings::Settings;
#[cfg(target_os = "linux")]
use crate::settings::{Device, Mode, SerialSettings};
#[cfg(target_os = "linux")]
use crate::source::{
    bus_count, index_for_bus,
    modbus::RtuTap,
    serial::SerialLine,
    socketcan::{detect_bitrates, CanReader},
    system_time_to_us, Transmit,
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

#[cfg(target_os = "linux")]
/// How long a reader waits after a failed read before trying again.
const READ_BACKOFF: Duration = Duration::from_secs(1);

#[cfg(target_os = "linux")]
/// Read buffer for a serial line: four seconds of 9600 baud, though a read
/// returns only what has arrived.
const SERIAL_READ: usize = 4096;

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
        /// The capability an operator is missing, when that is what went wrong.
        /// Both are the first thing a hand-run server hits, and both are what
        /// the unit will have to grant.
        fn hint(err: &io::Error, capability: &'static str) -> &'static str {
            if err.kind() == io::ErrorKind::PermissionDenied {
                capability
            } else {
                ""
            }
        }

        match self {
            Self::OpenCan { iface, err } => write!(
                f,
                "cannot open {iface}: {err}{}",
                hint(err, ". Run as root, or grant CAP_NET_RAW")
            ),
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
    let archives = match &settings.forward {
        Some(forward) => {
            Archives::start_all(forward, &settings.databases(), settings.stats_interval).map_err(
                |(path, err)| RunError::Cache {
                    path: path.display().to_string(),
                    err: err.to_string(),
                },
            )?
        }
        None => Archives::none(),
    };

    if settings.devices.is_empty() {
        // No local hardware, so no sockets, no lines and no GVRET listener.
        info!("No devices configured; running ingest-only");
    } else {
        start_devices(settings, &archives).await?;
    }

    if let Some(ingest) = &settings.ingest {
        let Some(archive) = archives.handle(settings.default_database()) else {
            return Err(RunError::IngestNeedsForward);
        };
        let server = ingest::Server::bind(ingest, archive)
            .await
            .map_err(|err| RunError::Bind {
                addr: format!("{}:{}", ingest.host, ingest.port),
                err,
            })?;
        tokio::spawn(server.run());
    }

    shutdown().await;
    info!("Shutting down");

    // Closing the queues is what tells the batchers to flush, so this has to
    // outlive the tasks that hold clones of the handles — which the runtime
    // drops when this returns. `TimeoutStopSec` in the unit is what gives the
    // flush room.
    archives.shutdown().await;
    Ok(())
}

#[cfg(target_os = "linux")]
/// Open every device, and start what reads them.
///
/// One broadcast for all of them: the GVRET bridge takes the CAN frames off
/// it and the console echo takes everything.
async fn start_devices(settings: &Settings, archives: &Archives) -> Result<(), RunError> {
    let (frames, _) = broadcast::channel(FRAME_BACKLOG);
    if !settings.can_devices().is_empty() {
        start_capture(settings, archives, frames.clone()).await?;
    }
    for (d, s) in settings.serial_devices() {
        let line =
            SerialLine::open_readonly(&d.interface, s).map_err(|err| RunError::OpenSerial {
                path: d.interface.clone(),
                err,
            })?;
        info!("Tapping {}[{}]  {s}  read-only", d.interface, d.bus.0);
        tokio::spawn(tap_loop(
            d.interface.clone(),
            s.clone(),
            line,
            RtuTap::new(d.bus),
            archives.handle(&d.database),
            frames.clone(),
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
async fn start_devices(_settings: &Settings, _archives: &Archives) -> Result<(), RunError> {
    Err(RunError::NoDeviceCapture)
}

#[cfg(target_os = "linux")]
/// A CAN device with its socket open and its archive found.
struct Opened {
    device: Device,
    reader: Arc<CanReader>,
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
) -> Result<(), RunError> {
    let mut opened = Vec::new();
    let mut rates = Vec::new();
    for d in settings.can_devices() {
        let reader =
            CanReader::open(&d.interface, d.bus, settings.default_dir, d.fd()).map_err(|err| {
                RunError::OpenCan {
                    iface: d.interface.clone(),
                    err,
                }
            })?;
        rates.push(detect_bitrates(&d.interface));
        opened.push(Opened {
            device: d.clone(),
            reader: Arc::new(reader),
            archive: archives.handle(&d.database),
        });
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

    for o in &opened {
        tokio::spawn(read_loop(
            o.reader.clone(),
            o.device.interface.clone(),
            frames.clone(),
            o.archive.clone(),
        ));
    }
    if let Some(tp) = &settings.test_pattern {
        let armed = settings.armed_indices(tp);
        // A name that matched nothing means an operator believes a bus is armed
        // that is not, and finds out from a validation run that fails for no
        // visible reason.
        for name in &tp.ifaces {
            match opened.iter().find(|o| &o.device.interface == name) {
                None => warn!("Test Pattern: no interface named {name} is being captured"),
                Some(o) if o.device.mode == Mode::Passive => {
                    warn!("Test Pattern: {name} is passive, so it is not armed")
                }
                Some(_) => {}
            }
        }
        if armed.is_empty() {
            // Saying ARMED here, with an empty list, would be the loudest line
            // in the journal contradicting the one above it.
            warn!("Test Pattern: enabled, but no configured interface matched; nothing is armed");
        } else {
            // WARN, not INFO: this is the one part of the server that puts
            // frames on a bus nobody asked it to, and a capture host where it
            // was armed by accident should say so in the journal's first screen.
            warn!(
                "Test Pattern responder ARMED on {} — this transmits on the bus",
                join(armed.iter().map(|&i| &opened[i].device.interface))
            );
            if !any_fd {
                warn!(
                    "Test Pattern: no device has fd on, so only the classic sweep can be answered"
                );
            }
        }
        for &i in &armed {
            let o = &opened[i];
            tokio::spawn(testpattern::responder_loop(
                o.reader.clone(),
                o.device.bus,
                o.device.fd(),
                frames.subscribe(),
                o.archive.clone(),
            ));
        }
    }
    tokio::spawn(transmit_loop(opened, settings.bus_offset, transmit_queue));
    tokio::spawn(listener.run());
    Ok(())
}

#[cfg(target_os = "linux")]
fn join<T: std::fmt::Display>(parts: impl Iterator<Item = T>) -> String {
    parts.map(|p| p.to_string()).collect::<Vec<_>>().join(",")
}

#[cfg(target_os = "linux")]
/// Publish one interface's frames to everything downstream.
async fn read_loop(
    reader: Arc<CanReader>,
    iface: String,
    frames: broadcast::Sender<Arc<Sample>>,
    archive: Option<Archive>,
) {
    loop {
        match reader.recv().await {
            Ok(sample) => publish(Sample::Can(sample), archive.as_ref(), &frames),
            Err(e) => {
                // An interface that goes down fails every read, and the Python
                // spun through them in silence. Say so, once a second.
                error!("{iface}: read failed: {e}");
                tokio::time::sleep(READ_BACKOFF).await;
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
/// Read one serial line for as long as the server runs, reopening it when it
/// goes away — a USB adapter unplugged and plugged back.
async fn tap_loop(
    interface: String,
    settings: SerialSettings,
    mut line: SerialLine,
    mut tap: RtuTap,
    archive: Option<Archive>,
    frames: broadcast::Sender<Arc<Sample>>,
) {
    let mut buf = [0u8; SERIAL_READ];
    loop {
        // Read until the line goes away.
        loop {
            match line.read(&mut buf).await {
                Ok(0) => {
                    error!("{interface}: line closed; reopening");
                    break;
                }
                Ok(n) => {
                    // One time for the read: a message is stamped as its last
                    // byte arrives, and every message a read completed shares it.
                    let ts_us = system_time_to_us(SystemTime::now());
                    for m in tap.push(&buf[..n], ts_us) {
                        publish(Sample::Modbus(m), archive.as_ref(), &frames);
                    }
                }
                Err(e) => {
                    error!("{interface}: read failed: {e}; reopening");
                    break;
                }
            }
        }
        // The half-message on either side of the gap does not join. The
        // reopen attempts are quiet until one works.
        tap.reset();
        line = loop {
            tokio::time::sleep(READ_BACKOFF).await;
            if let Ok(reopened) = SerialLine::open_readonly(&interface, &settings) {
                break reopened;
            }
        };
        info!("{interface}: reopened");
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
        if let Err(e) = o
            .reader
            .transmit(t.arb_id, t.extended, false, &t.data)
            .await
        {
            warn!("transmit on bus {} failed: {e}", t.bus.0);
            continue;
        }
        // Archived as `tx`, so a request this server made can be told apart
        // from the traffic it was answering. The Python did the same, and the
        // frame is timestamped here rather than on the wire — nothing reads a
        // frame back from a socket it wrote it to.
        if let Some(archive) = &o.archive {
            archive.enqueue(Arc::new(Sample::Can(CanSample {
                ts_us: system_time_to_us(SystemTime::now()),
                arb_id: t.arb_id,
                extended: t.extended,
                is_fd: false,
                data: t.data,
                bus: t.bus,
                dir: Direction::Tx,
            })));
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
