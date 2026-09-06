//! The running server: the archive, the SocketCAN readers, the GVRET listener,
//! and the fan-out between them.
//!
//! Only the CAN half is Linux-only, and it is marked as such. Everything else —
//! the archive, the ingest listener, the shutdown — runs anywhere, which is
//! what lets an ingest-only deployment (a server with no local CAN hardware,
//! fed by devices that push to it) be started and tested off a Pi.
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
use wiretap_model::{CanSample, Direction};

use crate::archive;
#[cfg(target_os = "linux")]
use crate::console;
#[cfg(target_os = "linux")]
use crate::gvret;
use crate::ingest;
use crate::settings::Settings;
#[cfg(target_os = "linux")]
use crate::source::{
    bus_count, bus_for_index, index_for_bus,
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

/// Why the server stopped, or would not start.
#[derive(Debug)]
pub enum RunError {
    OpenCan {
        iface: String,
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
    /// No CAN interfaces and no ingest listener.
    NothingToDo,
    /// The ingest listener is on, but there is nowhere for pushed frames to go.
    IngestNeedsForward,
    /// CAN interfaces were configured on a platform that has no SocketCAN.
    NoCanCapture,
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
                "no CAN interfaces configured and the ingest listener is disabled; nothing to do"
            ),
            // The Python refused the same combination, naming its own sink.
            // Accepting frames from a device and having nowhere to put them
            // would acknowledge them and then drop them, which is the one
            // thing at-least-once delivery must never do.
            Self::NoCanCapture => write!(
                f,
                "capturing CAN frames needs Linux and its SocketCAN stack; this build can \
                 still run an ingest-only server, which needs no local CAN hardware"
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
    if settings.ifaces.is_empty() && settings.ingest.is_none() {
        return Err(RunError::NothingToDo);
    }

    // The archive first: both a CAN reader and a pushing device feed it, and
    // neither should start before there is somewhere for frames to go. Its
    // absence is warned about at startup and is a legitimate deployment — a
    // GVRET bridge that archives nothing.
    let archive = settings
        .forward
        .as_ref()
        .map(|forward| {
            archive::start(forward, settings.stats_interval).map_err(|e| RunError::Cache {
                path: forward.batching.cache_path.display().to_string(),
                err: e.to_string(),
            })
        })
        .transpose()?;

    if settings.ifaces.is_empty() {
        // An ingest-only deployment, as the Python had: no local CAN hardware,
        // so no sockets and no GVRET listener either.
        info!("No CAN interfaces configured; running ingest-only");
    } else {
        start_capture(settings, archive.as_ref().map(|a| a.frames.clone())).await?;
    }

    if let Some(ingest) = &settings.ingest {
        let Some(running) = &archive else {
            return Err(RunError::IngestNeedsForward);
        };
        let server = ingest::Server::bind(ingest, running.frames.clone())
            .await
            .map_err(|err| RunError::Bind {
                addr: format!("{}:{}", ingest.host, ingest.port),
                err,
            })?;
        tokio::spawn(server.run());
    }

    shutdown().await;
    info!("Shutting down");

    // Dropping the producer is what tells the batcher to flush, so this has to
    // outlive the tasks that hold clones of it — which the runtime drops when
    // this returns. `TimeoutStopSec` in the unit is what gives the flush room.
    if let Some(archive) = archive {
        let _ = archive.shutdown().await;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
/// Open the CAN interfaces, bridge them to GVRET clients, and feed the archive.
///
/// Sockets first, then the listener, then the banner — the order the Python
/// used, so a permission problem is reported before anything claims to be
/// listening.
async fn start_capture(
    settings: &Settings,
    archive: Option<archive::Archive>,
) -> Result<(), RunError> {
    let mut readers = Vec::with_capacity(settings.ifaces.len());
    let mut buses = Vec::with_capacity(settings.ifaces.len());
    let mut rates = Vec::with_capacity(settings.ifaces.len());
    for (index, iface) in settings.ifaces.iter().enumerate() {
        let bus = bus_for_index(index, settings.bus_offset).ok_or_else(|| RunError::OpenCan {
            iface: iface.clone(),
            err: io::Error::new(io::ErrorKind::InvalidInput, "bus number past 255"),
        })?;
        let reader =
            CanReader::open(iface, bus, settings.default_dir, settings.can_fd).map_err(|err| {
                RunError::OpenCan {
                    iface: iface.clone(),
                    err,
                }
            })?;
        readers.push(Arc::new(reader));
        buses.push(bus);
        rates.push(detect_bitrates(iface));
    }

    let (frames, _) = broadcast::channel(FRAME_BACKLOG);
    let (transmits, transmit_queue) = mpsc::channel(TRANSMIT_QUEUE);
    let listener = gvret::Server::bind(
        &settings.host,
        settings.port,
        gvret::BusInfo {
            count: bus_count(readers.len(), settings.bus_offset),
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
        if settings.can_fd { "GVRET+FD" } else { "GVRET" },
        join(
            settings
                .ifaces
                .iter()
                .zip(&buses)
                .map(|(n, b)| format!("{n}[{}]", b.0))
        ),
        join(rates.iter().map(|r| r.nominal)),
        if rates.iter().any(|r| r.data != 0) {
            format!("  drates={}", join(rates.iter().map(|r| r.data)))
        } else {
            String::new()
        },
    );

    for (reader, iface) in readers.iter().cloned().zip(settings.ifaces.clone()) {
        tokio::spawn(read_loop(reader, iface, frames.clone(), archive.clone()));
    }
    if settings.echo_console {
        tokio::spawn(echo_loop(frames.subscribe(), settings.colour));
    }
    if let Some(tp) = &settings.test_pattern {
        // No names arms every interface: a `--test-pattern-enable` that armed
        // nothing would be a silent no-op.
        let armed: Vec<usize> = (0..settings.ifaces.len())
            .filter(|&i| tp.ifaces.is_empty() || tp.ifaces.contains(&settings.ifaces[i]))
            .collect();
        // A name that matched nothing means an operator believes a bus is armed
        // that is not, and finds out from a validation run that fails for no
        // visible reason.
        for name in tp.ifaces.iter().filter(|n| !settings.ifaces.contains(n)) {
            warn!("Test Pattern: no interface named {name} is being captured");
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
                join(armed.iter().map(|&i| &settings.ifaces[i]))
            );
            if !settings.can_fd {
                warn!("Test Pattern: --can-fd is off, so only the classic sweep can be answered");
            }
        }
        for &i in &armed {
            tokio::spawn(testpattern::responder_loop(
                readers[i].clone(),
                buses[i],
                settings.can_fd,
                frames.subscribe(),
                archive.clone(),
            ));
        }
    }
    tokio::spawn(transmit_loop(
        readers,
        settings.bus_offset,
        transmit_queue,
        archive,
    ));
    tokio::spawn(listener.run());
    Ok(())
}

/// Without SocketCAN there is nothing to capture from, but the rest of the
/// server still runs — which is what an ingest-only deployment is.
#[cfg(not(target_os = "linux"))]
async fn start_capture(
    _settings: &Settings,
    _archive: Option<archive::Archive>,
) -> Result<(), RunError> {
    Err(RunError::NoCanCapture)
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
    frames: broadcast::Sender<Arc<CanSample>>,
    archive: Option<archive::Archive>,
) {
    loop {
        match reader.recv().await {
            Ok(sample) => {
                let sample = Arc::new(sample);
                // Two consumers, two disciplines: the archive's queue is
                // bounded and spills to disk, while a send error on the
                // broadcast only means no GVRET client is watching.
                if let Some(archive) = &archive {
                    archive.enqueue(Arc::clone(&sample));
                }
                let _ = frames.send(sample);
            }
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
async fn echo_loop(mut frames: broadcast::Receiver<Arc<CanSample>>, colour: bool) {
    use broadcast::error::RecvError;

    let t0 = Instant::now();
    let mut line = String::new();
    loop {
        match frames.recv().await {
            Ok(sample) => {
                line.clear();
                console::format_line(&mut line, &sample, colour, t0.elapsed().as_micros() as u64);
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
/// Put what GVRET clients ask for onto the bus they named.
async fn transmit_loop(
    readers: Vec<Arc<CanReader>>,
    bus_offset: u8,
    mut queue: mpsc::Receiver<Transmit>,
    archive: Option<archive::Archive>,
) {
    while let Some(t) = queue.recv().await {
        // A bus this server does not have is dropped in silence, as the Python
        // dropped it: a client is free to address a device with more buses.
        let Some(index) = index_for_bus(t.bus, bus_offset, readers.len()) else {
            continue;
        };
        // Classic: a GVRET `F1 00` carries no FD flag, so a client cannot ask
        // for one. The Test Pattern responder owns the FD path.
        if let Err(e) = readers[index]
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
        if let Some(archive) = &archive {
            archive.enqueue(Arc::new(CanSample {
                ts_us: system_time_to_us(SystemTime::now()),
                arb_id: t.arb_id,
                extended: t.extended,
                is_fd: false,
                data: t.data,
                bus: t.bus,
                dir: Direction::Tx,
            }));
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
