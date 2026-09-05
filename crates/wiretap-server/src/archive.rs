//! Getting captured frames to durable storage, and holding them when that is
//! not possible.
//!
//! One bounded queue, one worker. The worker batches, writes, and on failure
//! spills to a [`FrameCache`] and backs off — then drains the cache *before*
//! the queue when the sink returns, so frames come out in the order they went
//! in across an outage boundary.
//!
//! **The log messages here are a compatibility surface.** The shipped
//! `wiretap-server.toml` tells operators to grep for them, so every one at
//! `info` and above is the Python's string verbatim — including the ones that
//! call the gateway "database", where renaming it would break the greps that
//! are the whole reason anyone reads these lines.
//!
//! The two exceptions are the disk cache's own `drained`/`flushed` lines, which
//! the Python counted off the queue rather than out of the cache and so printed
//! after a write that had failed. They carry the same words and a truthful
//! number; `docs/porting-notes.md` has the case that found it.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, warn};
use wiretap_model::CanSample;

use crate::cache::{CacheError, FrameCache, SqliteCache};
use crate::forward::ForwardSink;
use crate::settings::Forward;

/// Reconnect backoff: doubling from here, capped below.
const BACKOFF_START: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// How often the "queue FULL" line may repeat. Frames are dropped one at a
/// time, so without this a full queue would out-log the capture.
const FULL_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// How many drops pass before the clock is consulted about the line above. At
/// the rate a full queue drops, a thousand of them take a fraction of the
/// interval, so nothing is delayed by waiting for the count to come round.
const DROPS_PER_CLOCK_CHECK: u64 = 1024;

/// Why a sink write failed. A string for the same reason [`crate::cache`]'s is:
/// the caller's response is to cache the batch and back off, whatever went
/// wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkError(pub String);

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type SinkResult = Result<(), SinkError>;

/// Somewhere batches of frames go to be stored.
///
/// The Python spelled this as a subclass: `ForwardSink(PostgresWriter)`
/// overrode exactly `_connect`, `_write_batch`, `_keep_alive` and `_close_conn`
/// and inherited everything else. The abstraction was already there, so it is
/// kept even with one implementation — it is what the batcher and the disk
/// cache are generic over, and what lets the batcher be tested without a
/// gateway.
///
/// The futures are `Send` because a `Batcher` is spawned as a task; declaring
/// it here is what lets the batcher stay generic rather than boxed.
pub trait BatchSink: Send {
    /// Establish the connection. Called before the first write and after every
    /// failure.
    fn connect(&mut self) -> impl std::future::Future<Output = SinkResult> + Send;

    /// Store a batch, returning only once it is durable. The gateway ACKs
    /// after writing, so a slow archive back-pressures into the disk cache
    /// rather than being acknowledged and lost.
    fn write_batch(
        &mut self,
        batch: &[Arc<CanSample>],
    ) -> impl std::future::Future<Output = SinkResult> + Send;

    /// Called when there is nothing to write, to keep an idle connection from
    /// being dropped by the far end.
    fn keep_alive(&mut self) -> impl std::future::Future<Output = SinkResult> + Send;

    fn close(&mut self) -> impl std::future::Future<Output = ()> + Send;
}

/// What the server has done with frames since it started, for the stats line
/// and the shutdown summary.
///
/// Shared because the producer counts what it accepted and dropped while the
/// worker counts what it wrote and cached, and the stats line reports both.
#[derive(Debug, Default)]
pub struct Counters {
    pub enqueued: AtomicU64,
    pub written: AtomicU64,
    pub dropped: AtomicU64,
    pub cached: AtomicU64,
    pub cache_recovered: AtomicU64,
}

/// Bump a counter, returning what it held before. Relaxed throughout: these
/// are reported, never decided on.
fn add(field: &AtomicU64, n: u64) -> u64 {
    field.fetch_add(n, Ordering::Relaxed)
}

fn get(field: &AtomicU64) -> u64 {
    field.load(Ordering::Relaxed)
}

/// The producer end: where a capture hands frames over.
///
/// Dropping a frame here is the last resort and the only lossy step in the
/// archive path, which is why it is counted and logged rather than silent.
/// Cloneable, and the reporting state is shared rather than copied: there is
/// one queue however many interfaces feed it, so its high-water mark should be
/// reported once and not once per reader.
#[derive(Clone)]
pub struct Archive {
    tx: mpsc::Sender<Arc<CanSample>>,
    counters: Arc<Counters>,
    /// Which of the 80/95/100 buckets the queue was last reported in; -1 for
    /// "below 80", so a recovery is reported once rather than every frame.
    last_bucket: Arc<AtomicI64>,
    last_full_log: Arc<Mutex<Option<Instant>>>,
}

impl Archive {
    /// Hand over a frame, or drop it if the queue is full.
    ///
    /// Never blocks and never awaits: this is called from the capture path,
    /// where waiting on the archive is what the whole design exists to avoid.
    pub fn enqueue(&self, sample: Arc<CanSample>) {
        match self.tx.try_send(sample) {
            Ok(()) => {
                add(&self.counters.enqueued, 1);
                self.warn_thresholds();
            }
            Err(_) => self.log_queue_full(add(&self.counters.dropped, 1)),
        }
    }

    /// The running totals, for a stats line or a test that wants to know
    /// whether an outage was actually survived.
    pub fn counters(&self) -> Arc<Counters> {
        Arc::clone(&self.counters)
    }

    /// How full the queue is, as a percentage.
    ///
    /// Every ingest acknowledgement carries this, and it is the only
    /// back-pressure signal a pushing device gets: a client that sees it
    /// climbing can slow down before the batch it sends next is refused.
    pub fn occupancy_pct(&self) -> u8 {
        let (size, cap) = self.depth();
        u8::try_from(size * 100 / cap).unwrap_or(100)
    }

    /// Frames in the queue, and the size it was given.
    fn depth(&self) -> (usize, usize) {
        let cap = self.tx.max_capacity();
        (cap - self.tx.capacity(), cap)
    }

    /// Report each 80/95/100 threshold once as the queue fills, and once more
    /// when it falls back below 80.
    ///
    /// Integer comparisons rather than a ratio, and a load before the swap:
    /// this runs on every accepted frame, on every reader task, and the answer
    /// is the same as last time for all but a handful of them. A blind
    /// read-modify-write would put 30–60k contended writes a second on one
    /// cache line to say nothing had changed.
    fn warn_thresholds(&self) {
        let (size, cap) = self.depth();
        let bucket: i64 = if size >= cap {
            100
        } else if size * 100 >= cap * 95 {
            95
        } else if size * 100 >= cap * 80 {
            80
        } else {
            -1
        };
        if self.last_bucket.load(Ordering::Relaxed) == bucket {
            return;
        }
        // The swap is what settles a race between two readers crossing the
        // same threshold together: only the one that changed the value reports.
        if self.last_bucket.swap(bucket, Ordering::Relaxed) == bucket {
            return;
        }
        if bucket == -1 {
            info!("queue recovered: size={size} cap={cap}");
        } else {
            warn!("queue high water mark: {bucket}% (size={size} cap={cap})");
        }
    }

    /// The queue is full and a frame has been dropped.
    ///
    /// `prior` is the drop count before this one. Consulting the clock is
    /// itself a syscall, on the path that is by definition already overloaded,
    /// so all but one drop in `DROPS_PER_CLOCK_CHECK` returns without taking
    /// the lock or reading the time.
    fn log_queue_full(&self, prior: u64) {
        if prior % DROPS_PER_CLOCK_CHECK != 0 {
            return;
        }
        let now = Instant::now();
        let mut last = self.last_full_log.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| now.duration_since(t) < FULL_LOG_INTERVAL) {
            return;
        }
        *last = Some(now);
        drop(last);

        let (size, cap) = self.depth();
        error!(
            "queue FULL: size={size} cap={cap} dropped_total={}",
            get(&self.counters.dropped)
        );
    }
}

/// The worker: batches from the queue, writes to the sink, spills to the cache.
pub struct Batcher<S: BatchSink, C: FrameCache> {
    rx: mpsc::Receiver<Arc<CanSample>>,
    /// Set once, by [`Running::shutdown`]. Level-triggered on purpose: a
    /// signal that arrives while this is mid-write or mid-backoff is still
    /// there when it next looks, which an edge-triggered notify would lose.
    stop: watch::Receiver<bool>,
    sink: S,
    cache: C,
    counters: Arc<Counters>,
    batch_size: usize,
    flush_interval: Duration,
    /// Queue occupancy at which frames are moved to disk pre-emptively.
    flush_threshold: f64,
    connected: bool,
    /// Whether the sink has been reported down, so it is said once per outage.
    sink_down: bool,
    draining_cache: bool,
    /// Zero disables the periodic stats line.
    stats_interval: Duration,
    last_stats: Instant,
}

/// A running archive: the handle a capture enqueues to, and the worker behind
/// it.
pub struct Running {
    pub frames: Archive,
    stop: watch::Sender<bool>,
    pub worker: tokio::task::JoinHandle<()>,
}

impl Running {
    /// Stop the archive and wait for it to flush what it still holds.
    ///
    /// The signal is explicit rather than "drop the last `Archive`", which is
    /// what this used to be. That contract could not be honoured by any
    /// caller: the sender is cloned into every reader, into the transmit loop,
    /// and into every connected ingest session — and a session is spawned at
    /// accept time, so the set is not even known at startup. The one caller
    /// that tried held the flush open waiting for clones that only dropped
    /// when *it* returned, so every `SIGTERM` hung until systemd's
    /// `TimeoutStopSec` turned into `SIGKILL` and the flush never ran.
    ///
    /// Closing the queue is still what stops the worker; this just makes the
    /// receiver do it, where it takes no cooperation from anyone holding a
    /// sender. Callers may now hold as many clones as they like.
    pub async fn shutdown(self) -> Result<(), tokio::task::JoinError> {
        let _ = self.stop.send(true);
        self.worker.await
    }
}

/// Open the disk cache, adopt anything an older install left behind, and start
/// forwarding to the gateway.
///
/// This is the archive the server actually ships, so it names [`ForwardSink`]
/// and [`SqliteCache`] where the rest of the module is generic over both. It
/// lives here rather than in `pipeline` because it touches no CAN socket, and
/// `pipeline` is Linux-only — which had left the one assembly worth drilling
/// unreachable from a test, and the drill rebuilding it by hand.
pub fn start(forward: &Forward, stats_interval: f64) -> Result<Running, CacheError> {
    let batching = &forward.batching;
    let mut cache = SqliteCache::open(&batching.cache_path, batching.cache_max_mb)?;

    // Logged rather than fatal: the old cache is left where it is, to be tried
    // again next start, and refusing to capture because a previous run's
    // leftovers could not be moved is the worse trade.
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

    let (frames, batcher, stop) =
        channel(ForwardSink::new(forward), cache, batching, stats_interval);
    Ok(Running {
        frames,
        stop,
        worker: tokio::spawn(batcher.run()),
    })
}

/// Build a queue and the two ends that work it.
///
/// `stats_interval` is the `[logging]` setting rather than a batching one, but
/// the numbers it reports are all here: the queue depth, the counters, and what
/// is still on disk.
pub fn channel<S: BatchSink, C: FrameCache>(
    sink: S,
    cache: C,
    batching: &crate::settings::Batching,
    stats_interval: f64,
) -> (Archive, Batcher<S, C>, watch::Sender<bool>) {
    let counters = Arc::new(Counters::default());
    let (tx, rx) = mpsc::channel(batching.queue_size.max(1));
    let (stop_tx, stop) = watch::channel(false);
    (
        Archive {
            tx,
            counters: counters.clone(),
            last_bucket: Arc::new(AtomicI64::new(-1)),
            last_full_log: Arc::new(Mutex::new(None)),
        },
        Batcher {
            rx,
            stop,
            sink,
            cache,
            counters,
            batch_size: batching.size.max(1),
            flush_interval: Duration::from_secs_f64(batching.flush_interval.max(0.0)),
            flush_threshold: f64::from(batching.queue_flush_pct) / 100.0,
            connected: false,
            sink_down: false,
            draining_cache: false,
            stats_interval: Duration::from_secs_f64(stats_interval.max(0.0)),
            last_stats: Instant::now(),
        },
        stop_tx,
    )
}

impl<S: BatchSink, C: FrameCache> Batcher<S, C> {
    /// Work until the queue closes, then flush what is left.
    ///
    /// **Must run on a multi-threaded runtime.** Cache operations are SQLite,
    /// so they block; they go through [`tokio::task::block_in_place`] rather
    /// than stalling a worker that a reader task is sharing.
    pub async fn run(mut self) {
        let mut backoff = BACKOFF_START;
        loop {
            self.maybe_log_stats();
            self.spill_if_queue_filling();

            if !self.connected {
                match self.sink.connect().await {
                    Ok(()) => {
                        self.connected = true;
                        backoff = BACKOFF_START;
                        if self.sink_down {
                            self.sink_down = false;
                            info!("database connection restored");
                        }
                    }
                    Err(e) => {
                        self.fail(e, Vec::new()).await;
                        if !self.wait_to_retry(&mut backoff).await {
                            break;
                        }
                        continue;
                    }
                }
            }

            // Priority one: what an outage left on disk, so the archive's
            // order matches the bus's across the boundary. Reached only while
            // the sink is up, which is why a shutdown mid-outage leaves the
            // cache alone rather than trying to empty it on the way out.
            match self.drain_cache().await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    self.fail(e, Vec::new()).await;
                    if !self.wait_to_retry(&mut backoff).await {
                        break;
                    }
                    continue;
                }
            }

            let batch = self.next_batch().await;
            if batch.is_empty() {
                // Nothing queued, nothing cached, and nothing more coming.
                if self.queue_finished() {
                    break;
                }
                if let Err(e) = self.sink.keep_alive().await {
                    self.fail(e, Vec::new()).await;
                    if !self.wait_to_retry(&mut backoff).await {
                        break;
                    }
                }
                continue;
            }

            if let Err(e) = self.sink.write_batch(&batch).await {
                self.fail(e, batch).await;
                if !self.wait_to_retry(&mut backoff).await {
                    break;
                }
                continue;
            }
            add(&self.counters.written, batch.len() as u64);
        }

        self.shutdown_flush().await;
    }

    /// The periodic health line, from inside the loop rather than on a timer of
    /// its own — every number in it is owned here, and the loop comes round at
    /// least once per flush interval.
    ///
    /// The one visible consequence: during an outage the loop is sleeping out
    /// its backoff, so the line can be up to that late. It is a health report,
    /// and "the sink is down" is already in the log above it.
    fn maybe_log_stats(&mut self) {
        if self.stats_interval.is_zero() || self.last_stats.elapsed() < self.stats_interval {
            return;
        }
        self.last_stats = Instant::now();

        let (size, cap) = (self.rx.len(), self.rx.max_capacity());
        let occupancy = (size as f64) / (cap as f64) * 100.0;
        let c = &self.counters;
        let mut line = format!(
            "stats queued={size}/{cap} ({occupancy:.0}%) enq={} wrote={} dropped={} conn={}",
            get(&c.enqueued),
            get(&c.written),
            get(&c.dropped),
            if self.connected { "up" } else { "down" },
        );
        let pending = self.cache.count().unwrap_or(0);
        if pending > 0 || get(&c.cached) > 0 {
            line.push_str(&format!(
                " cached={} cache_recovered={} cache_pending={pending}",
                get(&c.cached),
                get(&c.cache_recovered),
            ));
        }
        info!("{line}");
    }

    /// Nothing more will ever be enqueued, and nothing is waiting.
    fn queue_finished(&self) -> bool {
        self.rx.is_closed() && self.rx.is_empty()
    }

    /// Back off before retrying a failed sink, or report that there is no
    /// longer any point.
    ///
    /// Without the check, a server told to stop *during* an outage would never
    /// stop: every path back to the top of the loop goes through a failing
    /// connect. What is on disk stays there for the next run, which is what the
    /// cache is for.
    async fn wait_to_retry(&mut self, delay: &mut Duration) -> bool {
        if self.queue_finished() {
            return false;
        }
        {
            let Self { rx, stop, .. } = self;
            tokio::select! {
                biased;
                // Without this a stop during an outage waits out the backoff,
                // which is up to `BACKOFF_MAX`. What is queued is not lost —
                // `shutdown_flush` still takes it, and with the sink down that
                // means the disk cache, for the next run to drain.
                _ = stop.changed() => {
                    rx.close();
                    return false;
                }
                _ = tokio::time::sleep(*delay) => {}
            }
        }
        *delay = (*delay * 2).min(BACKOFF_MAX);
        true
    }

    /// Collect up to `batch_size` frames, waiting `flush_interval` for the
    /// first and taking whatever else is already there.
    async fn next_batch(&mut self) -> Vec<Arc<CanSample>> {
        let mut batch: Vec<Arc<CanSample>> = Vec::with_capacity(self.batch_size);
        let first = {
            // Disjoint field borrows, so the queue and the stop signal can be
            // raced against each other.
            let Self {
                rx,
                stop,
                flush_interval,
                ..
            } = self;
            tokio::select! {
                // Stop first: a shutdown arriving on a busy queue should not
                // wait out another flush interval to be noticed.
                biased;
                // Closing the queue *is* the mechanism. `recv` then yields what
                // is still buffered and finally `None`, so `queue_finished`
                // and `wait_to_retry` below need no changes and no sender has
                // to be dropped by anyone. An `Err` here is the sender gone,
                // which means the archive was abandoned — same treatment.
                _ = stop.changed() => {
                    rx.close();
                    rx.recv().await
                }
                r = tokio::time::timeout(*flush_interval, rx.recv()) => r.unwrap_or(None),
            }
        };
        match first {
            Some(s) => batch.push(s),
            // Closed, or nothing arrived in time.
            None => return batch,
        }
        while batch.len() < self.batch_size {
            match self.rx.try_recv() {
                Ok(s) => batch.push(s),
                Err(_) => break,
            }
        }
        batch
    }

    /// Write one cache batch onward. `Ok(true)` means there may be more.
    ///
    /// A *cache* failure is reported and swallowed rather than returned: the
    /// error type is the sink's, and treating an unreadable cache as a dead
    /// gateway would close a healthy connection, log `database unavailable`,
    /// and empty the live queue into the very store that just failed. A failing
    /// cache must never stop a capture; frames keep flowing to the gateway
    /// while it is unhappy.
    async fn drain_cache(&mut self) -> Result<bool, SinkError> {
        // A field read, where `oldest` is a query and a batch-sized
        // allocation — thirty times a second on a server that has never cached
        // anything. Mid-drain it falls through, so the empty read below still
        // runs the cleanup that ends the drain.
        let pending = self.cache.count().unwrap_or(1);
        if !self.draining_cache && pending == 0 {
            return Ok(false);
        }
        let batch_size = self.batch_size;
        let cached = match blocking(|| self.cache.oldest(batch_size)) {
            Ok(cached) => cached,
            Err(e) => {
                error!("disk cache read error: {e}");
                return Ok(false);
            }
        };

        if cached.is_empty() {
            if self.draining_cache {
                self.draining_cache = false;
                info!("cache drain complete, deleting cache file");
                if let Err(e) = blocking(|| self.cache.reset()) {
                    error!("disk cache reset error: {e}");
                }
            }
            return Ok(false);
        }
        if !self.draining_cache {
            self.draining_cache = true;
            info!("draining {pending} cached frames to database");
        }

        let frames: Vec<Arc<CanSample>> = cached.iter().map(|c| Arc::clone(&c.sample)).collect();
        self.sink.write_batch(&frames).await?;
        if let Err(e) = blocking(|| self.cache.remove(&cached)) {
            // The frames are safe; the cache now holds duplicates of them.
            // Saying so is better than silently re-sending on the next pass.
            error!("disk cache delete error: {e}");
        }
        let n = frames.len() as u64;
        add(&self.counters.written, n);
        add(&self.counters.cache_recovered, n);
        debug!(
            "batch committed, cache_recovered={}",
            get(&self.counters.cache_recovered)
        );
        Ok(true)
    }

    /// The sink failed: say so once, put `batch` somewhere durable, and empty
    /// the queue behind it so the capture keeps its own path clear.
    async fn fail(&mut self, e: SinkError, batch: Vec<Arc<CanSample>>) {
        error!("write error: {e}");
        self.sink.close().await;
        self.connected = false;
        if !self.sink_down {
            self.sink_down = true;
            warn!("database unavailable, caching frames to disk");
        }
        if !batch.is_empty() {
            self.cache_batch(&batch);
        }
        self.drain_queue_to_cache();
    }

    /// Store a batch on disk, or count it dropped and say why. Returns how
    /// many were stored.
    fn cache_batch(&mut self, batch: &[Arc<CanSample>]) -> usize {
        // One region, because `is_full` stats three files and `append` writes:
        // handing the runtime two separate blocking hints for one logical
        // operation buys nothing.
        let stored = blocking(|| {
            if self.cache.is_full() {
                return Err(format!(
                    "disk cache full ({} MB)",
                    self.cache.size_bytes() / (1024 * 1024)
                ));
            }
            self.cache
                .append(batch)
                .map_err(|e| format!("disk cache write error: {e}"))
        });
        match stored {
            Ok(written) => {
                add(&self.counters.cached, written as u64);
                written
            }
            Err(why) => {
                // The count belongs to the failure, not to either way of
                // failing: the Python's cache-full line named it and its
                // write-error line did not, and the write error is the one an
                // operator meets when an upgrade leaves the cache unwritable.
                error!("{why}, dropping {} frames", batch.len());
                add(&self.counters.dropped, batch.len() as u64);
                0
            }
        }
    }

    /// Everything waiting in the queue, taken at once. Bounded by the queue.
    fn take_queued(&mut self) -> Vec<Arc<CanSample>> {
        let mut queued = Vec::new();
        while let Ok(s) = self.rx.try_recv() {
            queued.push(s);
        }
        queued
    }

    /// Empty the in-memory queue onto disk, in cache-sized batches.
    fn drain_queue_to_cache(&mut self) {
        let queued = self.take_queued();
        // What landed, where the Python logged what was taken off the queue. A
        // cache that is full drops every batch and `cache_batch` says so at
        // `error`; following that with an `info` line claiming the same frames
        // were drained is the one reassurance an operator reading the journal
        // after a gap must not be given.
        let cached: usize = queued
            .chunks(self.batch_size)
            .map(|chunk| self.cache_batch(chunk))
            .sum();
        if cached > 0 {
            info!("drained {cached} frames from queue to disk cache");
        }
    }

    /// Move frames to disk *before* the queue fills, so a burst that outruns
    /// the gateway costs disk rather than frames.
    fn spill_if_queue_filling(&mut self) {
        if self.flush_threshold <= 0.0 {
            return;
        }
        let (size, cap) = (self.rx.len(), self.rx.max_capacity());
        let ratio = (size as f64) / (cap as f64);
        if ratio >= self.flush_threshold {
            warn!(
                "queue at {}% ({size}/{cap}), flushing to disk cache",
                (ratio * 100.0) as u32
            );
            self.drain_queue_to_cache();
        }
    }

    /// On the way out: everything still queued goes to the sink if it is
    /// there, and to disk if it is not.
    async fn shutdown_flush(&mut self) {
        let remaining = self.take_queued();
        if !remaining.is_empty() {
            let flushed = self.connected
                && match self.sink.write_batch(&remaining).await {
                    Ok(()) => true,
                    Err(e) => {
                        error!("shutdown flush to DB failed: {e}");
                        false
                    }
                };
            if flushed {
                add(&self.counters.written, remaining.len() as u64);
                info!("shutdown: flushed {} frames to database", remaining.len());
            } else {
                // Same rule as the drain above.
                let cached = self.cache_batch(&remaining);
                if cached > 0 {
                    info!("shutdown: flushed {cached} frames to disk cache");
                }
            }
        }
        self.sink.close().await;

        let pending = self.cache.count().unwrap_or(0);
        let c = &self.counters;
        info!(
            "closed: wrote={} cached={} recovered={} dropped={} pending_in_cache={pending}",
            get(&c.written),
            get(&c.cached),
            get(&c.cache_recovered),
            get(&c.dropped),
        );
    }
}

/// Run a cache operation without holding a runtime worker hostage.
///
/// SQLite is synchronous, and on an SD card a drain can take tens of
/// milliseconds — long enough that a reader task sharing the worker would miss
/// frames. This is the one place that is allowed to block, and it tells the
/// runtime so.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cached;
    use crate::settings::Batching;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize};
    use std::sync::Mutex;
    use wiretap_model::{Direction, SourceId};

    fn sample(arb_id: u32) -> Arc<CanSample> {
        Arc::new(CanSample {
            ts_us: i64::from(arb_id),
            arb_id,
            extended: false,
            is_fd: false,
            data: vec![1],
            bus: SourceId(0),
            dir: Direction::Rx,
        })
    }

    /// Small and quick: a batch that fills fast, a queue that is easy to fill,
    /// and no pre-emptive spill unless a test asks for one.
    fn batching(queue_size: usize) -> Batching {
        Batching {
            size: 4,
            flush_interval: 0.02,
            queue_size,
            cache_path: PathBuf::from("unused"),
            cache_max_mb: 100,
            queue_flush_pct: 100,
            cache_origin: None,
            legacy_cache_path: None,
        }
    }

    // --- a sink a test can break and mend ---------------------------------

    #[derive(Clone, Default)]
    struct SinkState {
        frames: Arc<Mutex<Vec<u32>>>,
        fault: Arc<Mutex<Option<String>>>,
        connects: Arc<AtomicUsize>,
        keepalives: Arc<AtomicUsize>,
    }

    impl SinkState {
        fn fail(&self, why: Option<&str>) {
            *self.fault.lock().unwrap() = why.map(str::to_string);
        }

        fn written(&self) -> Vec<u32> {
            self.frames.lock().unwrap().clone()
        }

        fn check(&self) -> SinkResult {
            match self.fault.lock().unwrap().clone() {
                Some(e) => Err(SinkError(e)),
                None => Ok(()),
            }
        }
    }

    struct FakeSink(SinkState);

    impl BatchSink for FakeSink {
        async fn connect(&mut self) -> SinkResult {
            self.0.connects.fetch_add(1, Ordering::Relaxed);
            self.0.check()
        }

        async fn write_batch(&mut self, batch: &[Arc<CanSample>]) -> SinkResult {
            self.0.check()?;
            self.0
                .frames
                .lock()
                .unwrap()
                .extend(batch.iter().map(|f| f.arb_id));
            Ok(())
        }

        async fn keep_alive(&mut self) -> SinkResult {
            self.0.keepalives.fetch_add(1, Ordering::Relaxed);
            self.0.check()
        }

        async fn close(&mut self) {}
    }

    // --- a cache a test can inspect and fill -------------------------------

    #[derive(Clone, Default)]
    struct CacheState {
        frames: Arc<Mutex<Vec<Cached>>>,
        next_id: Arc<AtomicI64>,
        full: Arc<AtomicBool>,
        /// A cache that opens and then refuses the write, which `full` cannot
        /// stand in for — it returns before `append` is reached. Deliberately
        /// the same shape and spelling as [`SinkState`]'s, so breaking either
        /// double reads the same way; they stay separate because the two
        /// traits return different error types.
        fault: Arc<Mutex<Option<String>>>,
        resets: Arc<AtomicUsize>,
    }

    impl CacheState {
        fn ids(&self) -> Vec<u32> {
            self.frames
                .lock()
                .unwrap()
                .iter()
                .map(|c| c.sample.arb_id)
                .collect()
        }

        fn fail(&self, why: Option<&str>) {
            *self.fault.lock().unwrap() = why.map(str::to_string);
        }

        fn check(&self) -> crate::cache::Result<()> {
            match self.fault.lock().unwrap().clone() {
                Some(e) => Err(CacheError(e)),
                None => Ok(()),
            }
        }
    }

    struct FakeCache(CacheState);

    impl FrameCache for FakeCache {
        fn append(&mut self, frames: &[Arc<CanSample>]) -> crate::cache::Result<usize> {
            self.0.check()?;
            let mut held = self.0.frames.lock().unwrap();
            for f in frames {
                held.push(Cached {
                    id: self.0.next_id.fetch_add(1, Ordering::Relaxed),
                    sample: Arc::clone(f),
                });
            }
            Ok(frames.len())
        }

        fn oldest(&mut self, limit: usize) -> crate::cache::Result<Vec<Cached>> {
            Ok(self
                .0
                .frames
                .lock()
                .unwrap()
                .iter()
                .take(limit)
                .cloned()
                .collect())
        }

        fn remove(&mut self, frames: &[Cached]) -> crate::cache::Result<()> {
            let gone: Vec<i64> = frames.iter().map(|f| f.id).collect();
            self.0
                .frames
                .lock()
                .unwrap()
                .retain(|c| !gone.contains(&c.id));
            Ok(())
        }

        fn count(&mut self) -> crate::cache::Result<u64> {
            Ok(self.0.frames.lock().unwrap().len() as u64)
        }

        fn size_bytes(&self) -> u64 {
            self.0.frames.lock().unwrap().len() as u64 * 100
        }

        fn is_full(&self) -> bool {
            self.0.full.load(Ordering::Relaxed)
        }

        fn reset(&mut self) -> crate::cache::Result<()> {
            self.0.resets.fetch_add(1, Ordering::Relaxed);
            self.0.frames.lock().unwrap().clear();
            Ok(())
        }
    }

    /// Everything a batcher test needs, already wired together.
    struct Rig {
        archive: Archive,
        sink: SinkState,
        cache: CacheState,
        stop: watch::Sender<bool>,
        task: tokio::task::JoinHandle<()>,
    }

    fn rig(queue_size: usize) -> Rig {
        let sink = SinkState::default();
        let cache = CacheState::default();
        let (archive, batcher, stop) = channel(
            FakeSink(sink.clone()),
            FakeCache(cache.clone()),
            &batching(queue_size),
            0.0,
        );
        Rig {
            archive,
            sink,
            cache,
            stop,
            task: tokio::spawn(batcher.run()),
        }
    }

    impl Rig {
        /// Close the queue by dropping the last sender, and wait for the flush.
        /// Kept as the drop path so the tests below still cover it.
        async fn finish(self) {
            drop(self.archive);
            Self::joined(self.task, "the batcher stopped when its queue closed").await;
        }

        /// Stop by signal instead, with `archive` still held.
        async fn finish_by_signal(self) {
            let _ = self.stop.send(true);
            Self::joined(self.task, "the batcher stopped when it was told to").await;
        }

        async fn joined(task: tokio::task::JoinHandle<()>, what: &str) {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect(what)
                .unwrap();
        }
    }

    /// Poll until `cond`, or fail the test. Used instead of a fixed sleep so a
    /// slow machine does not make a false failure.
    async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn frames_reach_the_sink() {
        let r = rig(100);
        for i in 0..10 {
            r.archive.enqueue(sample(i));
        }
        let sink = r.sink.clone();
        r.finish().await;
        assert_eq!(sink.written(), (0..10).collect::<Vec<_>>());
    }

    /// The drill this whole module exists for: the gateway goes away, frames
    /// go to disk instead of being lost, and when it comes back they arrive
    /// **before** the frames captured since — so the archive's order is the
    /// bus's order across the boundary.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_outage_caches_and_drains_in_order() {
        let r = rig(100);
        r.sink.fail(Some("gateway down"));

        for i in 0..8 {
            r.archive.enqueue(sample(i));
        }
        let cache = r.cache.clone();
        wait_for("the frames to reach the cache", || cache.ids().len() == 8).await;
        assert!(r.sink.written().is_empty(), "nothing was archived");

        // The gateway returns, and more frames arrive while the cache drains.
        r.sink.fail(None);
        for i in 8..12 {
            r.archive.enqueue(sample(i));
        }

        let sink = r.sink.clone();
        r.finish().await;
        assert_eq!(
            sink.written(),
            (0..12).collect::<Vec<_>>(),
            "the cache drained ahead of the queue"
        );
        assert!(cache.ids().is_empty(), "and was emptied as it went");
        assert_eq!(
            cache.resets.load(Ordering::Relaxed),
            1,
            "then reset once, to give the space back"
        );
    }

    /// A full queue drops, counts, and says so — the only lossy step, and the
    /// one an operator needs to see.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_queue_drops_and_counts() {
        let sink = SinkState::default();
        sink.fail(Some("gateway down"));
        let (archive, batcher, _stop) = channel(
            FakeSink(sink.clone()),
            FakeCache(CacheState::default()),
            &batching(4),
            0.0,
        );
        // No worker: nothing drains the queue, so it fills and stays full.
        drop(batcher);

        for i in 0..10 {
            archive.enqueue(sample(i));
        }
        assert_eq!(
            get(&archive.counters.enqueued),
            0,
            "a closed queue takes none"
        );
        assert_eq!(get(&archive.counters.dropped), 10);
    }

    /// What `cache_batch` reports is what landed, because both `info` lines
    /// about the disk cache are written from its answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cache_that_cannot_be_written_reports_nothing_stored() {
        let cache = CacheState::default();
        let (_archive, mut batcher, _stop) = channel(
            FakeSink(SinkState::default()),
            FakeCache(cache.clone()),
            &batching(4),
            0.0,
        );
        let frames: Vec<_> = (0..4).map(sample).collect();

        assert_eq!(
            batcher.cache_batch(&frames),
            4,
            "a writable cache takes them"
        );

        cache.fail(Some("attempt to write a readonly database"));
        assert_eq!(
            batcher.cache_batch(&frames),
            0,
            "and a read-only one stores none, rather than saying it did"
        );
        assert_eq!(get(&batcher.counters.cached), 4, "only the first batch");
        assert_eq!(get(&batcher.counters.dropped), 4, "the second was lost");
    }

    /// Frames move to disk before the queue fills, so a burst that outruns the
    /// gateway costs disk rather than frames.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_filling_queue_spills_to_disk_before_it_overflows() {
        let sink = SinkState::default();
        sink.fail(Some("gateway down"));
        let cache = CacheState::default();
        let mut cfg = batching(20);
        cfg.queue_flush_pct = 50;
        let (archive, batcher, _stop) =
            channel(FakeSink(sink.clone()), FakeCache(cache.clone()), &cfg, 0.0);
        let task = tokio::spawn(batcher.run());

        for i in 0..20 {
            archive.enqueue(sample(i));
        }
        wait_for("the queue to spill", || !cache.ids().is_empty()).await;

        drop(archive);
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("stopped")
            .unwrap();
        assert_eq!(cache.ids().len(), 20, "all of them, none dropped");
    }

    /// A cache that has run out of room drops rather than growing without
    /// bound, and the frames it drops are counted.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_cache_drops_rather_than_growing() {
        let r = rig(100);
        r.sink.fail(Some("gateway down"));
        r.cache.full.store(true, Ordering::Relaxed);

        for i in 0..8 {
            r.archive.enqueue(sample(i));
        }
        let cache = r.cache.clone();
        let counters = Arc::clone(&r.archive.counters);
        wait_for("the frames to be dropped", || get(&counters.dropped) >= 8).await;
        assert!(cache.ids().is_empty(), "nothing was stored");
    }

    /// An idle connection is kept alive rather than left to be dropped by the
    /// far end.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_idle_sink_is_kept_alive() {
        let r = rig(100);
        let sink = r.sink.clone();
        wait_for("an idle keepalive", || {
            sink.keepalives.load(Ordering::Relaxed) > 0
        })
        .await;
        r.finish().await;
    }

    /// What is still queued at shutdown is written, not dropped.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_flushes_what_is_left() {
        let r = rig(100);
        for i in 0..3 {
            r.archive.enqueue(sample(i));
        }
        let sink = r.sink.clone();
        r.finish().await;
        assert_eq!(sink.written(), [0, 1, 2]);
    }

    /// The signal finishes the worker even though a producer is still holding
    /// the queue open — which is every real deployment: a reader, the transmit
    /// loop, or a connected ingest session. Under the old "drop the last
    /// sender" contract this hung, and no caller could honour it, because a
    /// session is spawned at accept time and nobody knows the set.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_finishes_while_a_producer_still_holds_the_queue() {
        let r = rig(100);
        for i in 0..3 {
            r.archive.enqueue(sample(i));
        }
        let sink = r.sink.clone();
        // The clone a reader or an ingest session would be holding. It
        // deliberately outlives the shutdown.
        let still_capturing = r.archive.clone();

        r.finish_by_signal().await;

        assert_eq!(sink.written(), [0, 1, 2], "the flush still ran");
        drop(still_capturing);
    }

    /// And if the sink is gone at shutdown, it goes to disk instead.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_falls_back_to_the_cache() {
        let r = rig(100);
        r.sink.fail(Some("gateway down"));
        for i in 0..3 {
            r.archive.enqueue(sample(i));
        }
        let cache = r.cache.clone();
        wait_for("the frames to be cached", || cache.ids().len() == 3).await;
        r.finish().await;
        assert_eq!(cache.ids(), [0, 1, 2]);
    }
}
