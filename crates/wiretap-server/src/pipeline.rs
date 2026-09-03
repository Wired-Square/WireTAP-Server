//! The running server: SocketCAN readers, the GVRET listener, and the fan-out
//! between them.
//!
//! Linux-only, because everything here begins with a CAN socket. The pieces it
//! wires together are not — the codec, the console line and the client tasks
//! are all exercised on any machine — so what is untested until CI puts a
//! `vcan` interface underneath it is this file's wiring, and nothing below it.
//!
//! The shape is the Python's turned inside out. There, one loop `select`ed
//! over the CAN sockets and the listening socket and did every client's write
//! itself. Here each interface has a reader task publishing to a broadcast
//! channel, each client has a task subscribed to it, and transmits travel the
//! other way down an mpsc to a task that owns the sockets. No client can delay
//! a read, and no read can delay a client.

use std::io::{self, Write as _};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};
use wiretap_model::{CanSample, Direction};

use crate::archive;
use crate::cache::SqliteCache;
use crate::console;
use crate::forward::ForwardSink;
use crate::gvret::server;
use crate::settings::{Forward, Settings};
use crate::source::{
    bus_count, bus_for_index, index_for_bus,
    socketcan::{detect_bitrates, CanReader},
    system_time_to_us, Transmit,
};

/// Frames held for a GVRET client that is behind.
///
/// At the ~15k frames a second a busy 1 Mbit/s bus produces, this is about 70
/// ms of grace before a stalled client starts losing frames — long enough to
/// ride out a scheduling hiccup, short enough that the memory behind it is a
/// few tens of kilobytes.
const FRAME_BACKLOG: usize = 1024;

/// Transmits queued for the bus. Small on purpose: a GVRET client transmits
/// occasionally, so a backlog here means the bus or the interface is already
/// in trouble and the useful answer is to say so.
const TRANSMIT_QUEUE: usize = 64;

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
    /// An ingest-only deployment, which needs Stage 4.
    IngestNotPorted,
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
            Self::IngestNotPorted => write!(
                f,
                "an ingest-only deployment has nothing to run yet: the binary ingest listener \
                 is still being ported"
            ),
        }
    }
}

/// Open every interface, start the listener, and run until a signal.
pub async fn run(settings: &Settings) -> Result<(), RunError> {
    if settings.ifaces.is_empty() {
        // The Python idled here when the ingest listener was enabled and
        // exited otherwise. Both are an early exit until Stage 4 gives the
        // first case something to do.
        return Err(if settings.ingest.is_some() {
            RunError::IngestNotPorted
        } else {
            RunError::NothingToDo
        });
    }

    // Sockets first, then the listener, then the banner — the order the Python
    // used, so a permission problem is reported before anything claims to be
    // listening.
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
    let listener = server::Server::bind(
        &settings.host,
        settings.port,
        server::BusInfo {
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

    // The archive, if there is one to send to. Its absence is already warned
    // about at startup, and the capture runs either way — a GVRET bridge with
    // no archive is a legitimate deployment.
    let archive = settings
        .forward
        .as_ref()
        .map(|forward| start_archive(forward, settings.stats_interval))
        .transpose()?;

    for (reader, iface) in readers.iter().cloned().zip(settings.ifaces.clone()) {
        tokio::spawn(read_loop(
            reader,
            iface,
            frames.clone(),
            archive.as_ref().map(|a| a.frames.clone()),
        ));
    }
    if settings.echo_console {
        tokio::spawn(echo_loop(frames.subscribe(), settings.colour));
    }
    tokio::spawn(transmit_loop(
        readers,
        settings.bus_offset,
        transmit_queue,
        archive.as_ref().map(|a| a.frames.clone()),
    ));
    tokio::spawn(listener.run());

    shutdown().await;
    info!("Shutting down");

    // Dropping the last sender is what tells the batcher to flush and stop, so
    // this has to outlive the reader tasks — which the runtime drops when this
    // returns. `TimeoutStopSec` in the unit is what gives the flush room.
    if let Some(archive) = archive {
        drop(archive.frames);
        let _ = archive.worker.await;
    }
    Ok(())
}

/// The queue, the worker task, and the disk cache behind them.
struct RunningArchive {
    frames: archive::Archive,
    worker: tokio::task::JoinHandle<()>,
}

/// Open the cache, drain anything an older install left in `$HOME`, and start
/// the batcher.
fn start_archive(forward: &Forward, stats_interval: f64) -> Result<RunningArchive, RunError> {
    let batching = &forward.batching;
    let mut cache =
        SqliteCache::open(&batching.cache_path, batching.cache_max_mb).map_err(|e| {
            RunError::Cache {
                path: batching.cache_path.display().to_string(),
                err: e.to_string(),
            }
        })?;

    // A failure here is logged rather than fatal: the old cache is left where
    // it is to be tried again next start, and refusing to capture because a
    // previous run's leftovers could not be moved is the worse trade.
    if let Some(legacy) = &batching.legacy_cache_path {
        match cache.adopt(legacy) {
            Ok(0) => {}
            Ok(moved) => info!(
                "adopted {moved} frames from the previous cache at {}",
                legacy.display()
            ),
            Err(e) => error!(
                "cannot adopt the previous cache at {}: {e}; leaving it alone",
                legacy.display()
            ),
        }
    }

    let (frames, batcher) =
        archive::channel(ForwardSink::new(forward), cache, batching, stats_interval);
    Ok(RunningArchive {
        frames,
        worker: tokio::spawn(batcher.run()),
    })
}

fn join<T: std::fmt::Display>(parts: impl Iterator<Item = T>) -> String {
    parts.map(|p| p.to_string()).collect::<Vec<_>>().join(",")
}

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
        if let Err(e) = readers[index].transmit(t.arb_id, t.extended, &t.data).await {
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
