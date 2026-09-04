//! The outage drill, against a real gateway.
//!
//! Ignored by default: it needs a WireTAP gateway on `WIRETAP_GATEWAY` with a
//! key in `WIRETAP_FORWARD_TOKEN`, and it stops and starts that gateway partway
//! through. Everything it exercises is unit-tested with fakes; what this adds is
//! that the fakes agree with a real gateway writing to a real TimescaleDB.
//!
//! ```sh
//! docker compose -f crates/wiretap-backend/docker-compose.yml up -d
//! WIRETAP_GATEWAY=127.0.0.1:9323 \
//! WIRETAP_FORWARD_TOKEN=… \
//! WIRETAP_DRILL_DB=vehicle_drill \
//!     cargo test -p wiretap-server --test outage_drill -- --ignored --nocapture
//! ```
//!
//! The interruption is left to the operator running it: the test prints what it
//! is waiting for and polls, so the gateway can be stopped and started with
//! `docker compose stop backend` / `start backend` while it runs. That is
//! deliberate — a drill that mocks the outage is the drill that already passes.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use wiretap_model::{CanSample, Direction, Secret, SourceId};
use wiretap_server::archive;
use wiretap_server::cache::{FrameCache, SqliteCache};
use wiretap_server::settings::{Batching, Forward};
use wiretap_server::source::system_time_to_us;

/// Frames pushed through the drill. Enough that batching, chunking at the
/// protocol's 256-record cap, and a cache spanning several batches all happen.
const FRAMES: u32 = 10_000;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn sample(seq: u32, base_us: i64) -> Arc<CanSample> {
    Arc::new(CanSample {
        // A microsecond apart, so the archive's ordering is checkable and no
        // two rows collide on a timestamp.
        ts_us: base_us + i64::from(seq),
        arb_id: 0x700 + (seq % 4),
        extended: false,
        is_fd: false,
        // The sequence number in the payload, so a row can be traced back to
        // the frame that produced it.
        data: seq.to_le_bytes().to_vec(),
        bus: SourceId(0),
        dir: Direction::Rx,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a running gateway; see the module docs"]
async fn frames_survive_a_gateway_outage() {
    let gateway = env("WIRETAP_GATEWAY").expect("set WIRETAP_GATEWAY=host:port");
    let (host, port) = gateway.rsplit_once(':').expect("host:port");
    let token = env("WIRETAP_FORWARD_TOKEN").expect("set WIRETAP_FORWARD_TOKEN");
    let database = env("WIRETAP_DRILL_DB").unwrap_or_default();

    let dir = std::env::temp_dir().join(format!("wiretap-drill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cache_path = dir.join("cache.db");

    let forward = Forward {
        host: host.to_string(),
        port: port.parse().expect("a port"),
        api_key: Secret::new(token),
        database,
        batching: Batching {
            size: 500,
            flush_interval: 0.5,
            queue_size: 50_000,
            cache_path: cache_path.clone(),
            cache_max_mb: 1000,
            queue_flush_pct: 50,
            legacy_cache_path: None,
        },
    };
    let running = archive::start(&forward, 2.0).expect("an archive");
    let counters = running.frames.counters();
    let base_us = system_time_to_us(SystemTime::now());

    println!("--- pushing {FRAMES} frames; stop and start the gateway while this runs ---");
    let started = Instant::now();
    for seq in 0..FRAMES {
        running.frames.enqueue(sample(seq, base_us));
        // Roughly 500 frames a second, so the run lasts long enough for a
        // gateway to be stopped and started by hand partway through.
        if seq % 50 == 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    println!(
        "--- all {FRAMES} enqueued in {:?}; draining ---",
        started.elapsed()
    );

    // Shutting down tells the batcher to flush and stop. If the gateway is
    // still down it will stop with frames on disk, which the assertions below
    // then report as the failure it is.
    tokio::time::timeout(Duration::from_secs(300), running.shutdown())
        .await
        .expect("the batcher finished")
        .unwrap();

    let mut cache = SqliteCache::open(&cache_path, 1000).expect("reopen");
    let left = cache.count().expect("count");
    let dropped = counters.dropped.load(Relaxed);
    println!(
        "--- {left} left in the cache; wrote={} cached={} recovered={} dropped={dropped} ---",
        counters.written.load(Relaxed),
        counters.cached.load(Relaxed),
        counters.cache_recovered.load(Relaxed),
    );
    println!(
        "Now check the gateway's database: {FRAMES} rows with ts between \
         {base_us} and {}, no duplicates, and strictly increasing.",
        base_us + i64::from(FRAMES)
    );
    assert_eq!(left, 0, "every cached frame was drained before shutdown");
    assert_eq!(dropped, 0, "nothing was dropped");
    let _ = std::fs::remove_dir_all(&dir);
}
