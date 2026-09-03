//! Where frames wait when the gateway is not there.
//!
//! The schema and the pragmas are the Python's, byte for byte, because
//! existing Raspberry Pis have a populated `~/.wiretap-server-cache.db` and an
//! upgrade *during* an outage must drain it rather than start an empty one
//! beside it. That is the whole constraint on this module: it could be a
//! better-shaped store, and one day it will be — see [`FrameCache`] — but not
//! at the cost of frames already captured.

use std::path::{Path, PathBuf};

use rusqlite::{params_from_iter, Connection};
use wiretap_model::{payload_dlc, CanSample, Direction, SourceId};

/// Why a cache operation failed.
///
/// A string, because every caller has the same two options: log it and carry
/// on, or give up on the cache. Nothing recovers differently per variant, and a
/// failing cache must never stop a capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheError(pub String);

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<rusqlite::Error> for CacheError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, CacheError>;

/// A frame in the cache, with whatever the store needs to find it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached {
    /// Opaque to the caller: a rowid here, a file offset in a segment store.
    pub id: i64,
    pub sample: CanSample,
}

/// A durable FIFO of frames waiting for the gateway.
///
/// A trait with one implementation, deliberately. The access pattern is append
/// and strict-FIFO drain, which SQLite serves by making you pay an `INSERT` and
/// a `COMMIT` per batch, a `DELETE … WHERE id IN (…)` per drain, and a whole
/// `VACUUM` to clear — where 64 MiB segment files drained by unlinking segment
/// zero would be simpler, faster, FIFO by construction and honest about their
/// size. This is the seam that change goes through. Measure first.
///
/// Synchronous on purpose: SQLite is, and pretending otherwise inside the trait
/// would hide the blocking from the caller that has to keep it off a runtime
/// worker.
pub trait FrameCache: Send {
    /// Append frames, returning how many were stored.
    fn append(&mut self, frames: &[CanSample]) -> Result<usize>;

    /// The oldest `limit` frames, in capture order.
    fn oldest(&mut self, limit: usize) -> Result<Vec<Cached>>;

    /// Forget frames that have been written somewhere durable.
    fn remove(&mut self, frames: &[Cached]) -> Result<()>;

    fn count(&mut self) -> Result<u64>;

    /// Bytes on disk, for comparing against the configured limit.
    fn size_bytes(&self) -> u64;

    /// Drop everything and give the space back.
    fn clear(&mut self) -> Result<()>;
}

/// The Python's cache, and the only implementation of [`FrameCache`].
pub struct SqliteCache {
    conn: Connection,
    path: PathBuf,
    max_bytes: u64,
}

impl SqliteCache {
    /// Open or create the cache at `path`.
    ///
    /// `CREATE TABLE IF NOT EXISTS` with the Python's exact column list, so
    /// opening a cache it wrote is a no-op rather than a migration.
    pub fn open(path: impl Into<PathBuf>, max_mb: u64) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| CacheError(format!("cannot create {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(&path)?;
        // WAL so a reader and the appending writer do not block each other;
        // NORMAL because losing the last few frames to a power cut is better
        // than an fsync per batch on an SD card. Both are the Python's.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS frames (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts REAL NOT NULL,
                extended INTEGER NOT NULL,
                is_fd INTEGER NOT NULL,
                arb_id INTEGER NOT NULL,
                dlc INTEGER NOT NULL,
                data BLOB NOT NULL,
                bus INTEGER NOT NULL,
                dir TEXT NOT NULL
            )",
        )?;
        Ok(Self {
            conn,
            path,
            max_bytes: max_mb.saturating_mul(1024 * 1024),
        })
    }

    /// Whether the cache has reached its configured size.
    pub fn is_full(&self) -> bool {
        self.size_bytes() >= self.max_bytes
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Close the connection and delete the file, with its sidecars.
    ///
    /// For the one-shot drain of a cache left by an older install: the frames
    /// are somewhere durable, and leaving an empty database behind invites the
    /// next version to find it and wonder.
    pub fn delete(self) -> Result<()> {
        let path = self.path.clone();
        drop(self.conn);
        for suffix in ["", "-wal", "-shm"] {
            let f = sidecar(&path, suffix);
            if let Err(e) = std::fs::remove_file(&f) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(CacheError(format!("cannot remove {}: {e}", f.display())));
                }
            }
        }
        Ok(())
    }
}

/// The cache file, or one of the two files SQLite keeps beside it.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    if suffix.is_empty() {
        return path.to_path_buf();
    }
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Seconds since the epoch, as the Python stored them.
///
/// A `REAL` column holds microseconds exactly for any date this will see: the
/// gap between representable `f64`s at 2^31 seconds is about 0.5 µs, so the
/// rounding below recovers the integer it started from. It stops being true
/// somewhere in the 23rd century, which is a cheaper problem than a schema that
/// an existing cache cannot be read with.
fn to_secs(ts_us: i64) -> f64 {
    ts_us as f64 / 1_000_000.0
}

fn from_secs(ts: f64) -> i64 {
    (ts * 1_000_000.0).round() as i64
}

impl FrameCache for SqliteCache {
    fn append(&mut self, frames: &[CanSample]) -> Result<usize> {
        if frames.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO frames (ts, extended, is_fd, arb_id, dlc, data, bus, dir)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for f in frames {
                // `dlc` is written for the Python's benefit, not ours: it is
                // derivable from the payload and this store's reader derives
                // it, but a Python still reading this file expects a column.
                stmt.execute(rusqlite::params![
                    to_secs(f.ts_us),
                    f.extended,
                    f.is_fd,
                    f.arb_id,
                    payload_dlc(f.data.len(), f.is_fd),
                    f.data,
                    f.bus.0,
                    f.dir.as_str(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(frames.len())
    }

    fn oldest(&mut self, limit: usize) -> Result<Vec<Cached>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, ts, extended, is_fd, arb_id, data, bus, dir
             FROM frames ORDER BY id LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(Cached {
                id: r.get(0)?,
                sample: CanSample {
                    ts_us: from_secs(r.get(1)?),
                    extended: r.get(2)?,
                    is_fd: r.get(3)?,
                    arb_id: r.get(4)?,
                    data: r.get(5)?,
                    bus: SourceId(r.get(6)?),
                    // An unreadable tag is `rx`: the Python wrote whatever
                    // `--pg-dir` said, so a cache from one could hold anything,
                    // and the direction is not worth dropping a frame over.
                    dir: r.get::<_, String>(7)?.parse().unwrap_or(Direction::Rx),
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn remove(&mut self, frames: &[Cached]) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        // Built per call rather than prepared: the placeholder count is the
        // batch size, and a short final batch would miss a cached statement
        // anyway.
        let placeholders = std::iter::repeat_n("?", frames.len())
            .collect::<Vec<_>>()
            .join(",");
        self.conn.execute(
            &format!("DELETE FROM frames WHERE id IN ({placeholders})"),
            params_from_iter(frames.iter().map(|f| f.id)),
        )?;
        Ok(())
    }

    fn count(&mut self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM frames", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    /// **The write-ahead log counts.** The Python stat'd the database file
    /// alone, so during an outage — exactly when the limit matters — it
    /// under-reported by however much had not been checkpointed, and
    /// `cache_max_mb` was a number the cache could sail past.
    fn size_bytes(&self) -> u64 {
        ["", "-wal", "-shm"]
            .iter()
            .filter_map(|s| std::fs::metadata(sidecar(&self.path, s)).ok())
            .map(|m| m.len())
            .sum()
    }

    fn clear(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM frames", [])?;
        // Outside a transaction by necessity, and the point of it: the space
        // is what was wanted back.
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that goes away when the test does. Held by every test as
    /// `_dir`, so it outlives the cache inside it.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("wiretap-cache-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("a writable temp directory");
            Self(dir)
        }

        fn db(&self) -> PathBuf {
            self.0.join("cache.db")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_cache(name: &str, max_mb: u64) -> (TempDir, SqliteCache) {
        let dir = TempDir::new(name);
        let cache = SqliteCache::open(dir.db(), max_mb).expect("opens");
        (dir, cache)
    }

    fn sample(ts_us: i64, arb_id: u32) -> CanSample {
        CanSample {
            ts_us,
            arb_id,
            extended: false,
            is_fd: false,
            data: vec![1, 2, 3],
            bus: SourceId(0),
            dir: Direction::Rx,
        }
    }

    #[test]
    fn frames_come_back_out_in_the_order_they_went_in() {
        let (_dir, mut c) = temp_cache("fifo", 100);
        let frames: Vec<CanSample> = (0..5).map(|i| sample(1_000 + i, i as u32)).collect();
        assert_eq!(c.append(&frames).unwrap(), 5);
        assert_eq!(c.count().unwrap(), 5);

        let first = c.oldest(3).unwrap();
        assert_eq!(
            first.iter().map(|f| f.sample.arb_id).collect::<Vec<_>>(),
            [0, 1, 2]
        );

        // Removing the head leaves the tail, still in order.
        c.remove(&first).unwrap();
        assert_eq!(c.count().unwrap(), 2);
        let rest = c.oldest(10).unwrap();
        assert_eq!(
            rest.iter().map(|f| f.sample.arb_id).collect::<Vec<_>>(),
            [3, 4]
        );
    }

    /// Every field has to survive the round trip, or an outage silently
    /// rewrites what was captured.
    #[test]
    fn a_frame_survives_the_round_trip_intact() {
        let (_dir, mut c) = temp_cache("roundtrip", 100);
        let original = CanSample {
            ts_us: 1_700_000_000_123_456,
            arb_id: 0x18DA_F110,
            extended: true,
            is_fd: true,
            data: (0..64).collect(),
            bus: SourceId(7),
            dir: Direction::Tx,
        };
        c.append(std::slice::from_ref(&original)).unwrap();
        assert_eq!(c.oldest(1).unwrap()[0].sample, original);
    }

    /// The `REAL` column is the Python's, and it has to hold microseconds
    /// exactly across the range a capture will actually see.
    #[test]
    fn microseconds_survive_the_real_column() {
        for ts_us in [
            0,
            1,
            1_700_000_000_000_001,
            1_700_000_000_999_999,
            // 2038, where a 32-bit time_t stops and the gaps between f64s are
            // widest within any plausible lifetime of this format.
            2_147_483_647_999_999,
        ] {
            assert_eq!(from_secs(to_secs(ts_us)), ts_us, "{ts_us}");
        }
    }

    /// The write-ahead log is where an outage's frames actually are, so it has
    /// to count against the limit that is supposed to bound them.
    #[test]
    fn the_size_includes_the_write_ahead_log() {
        let (_dir, mut c) = temp_cache("size", 100);
        let frames: Vec<CanSample> = (0..2_000).map(|i| sample(i, i as u32)).collect();
        c.append(&frames).unwrap();

        let db_only = std::fs::metadata(c.path()).map(|m| m.len()).unwrap_or(0);
        let wal = std::fs::metadata(sidecar(c.path(), "-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(wal > 0, "WAL mode is on and the frames are in it");
        assert!(
            c.size_bytes() > db_only,
            "the Python counted only {db_only} of {}",
            c.size_bytes()
        );
    }

    #[test]
    fn a_full_cache_says_so() {
        let (_dir, mut c) = temp_cache("full", 0);
        assert!(c.is_full(), "a zero-byte limit is full from the start");
        c.append(&[sample(1, 1)]).unwrap();
        assert!(c.is_full());
    }

    #[test]
    fn clearing_empties_the_cache() {
        let (_dir, mut c) = temp_cache("clear", 100);
        c.append(&(0..100).map(|i| sample(i, 1)).collect::<Vec<_>>())
            .unwrap();
        c.clear().unwrap();
        assert_eq!(c.count().unwrap(), 0);
        assert!(c.oldest(10).unwrap().is_empty());
    }

    /// Reopening is what an upgrade does, and it must find what the last run
    /// left rather than starting again.
    #[test]
    fn reopening_finds_what_was_left() {
        let dir = TempDir::new("reopen");
        {
            let mut c = SqliteCache::open(dir.db(), 100).unwrap();
            c.append(&[sample(1, 0x123)]).unwrap();
        }
        let mut reopened = SqliteCache::open(dir.db(), 100).expect("reopens");
        assert_eq!(reopened.count().unwrap(), 1);
        assert_eq!(reopened.oldest(1).unwrap()[0].sample.arb_id, 0x123);
    }

    #[test]
    fn deleting_takes_the_sidecars_with_it() {
        let dir = TempDir::new("delete");
        let mut c = SqliteCache::open(dir.db(), 100).unwrap();
        c.append(&[sample(1, 1)]).unwrap();
        c.delete().unwrap();
        for suffix in ["", "-wal", "-shm"] {
            assert!(
                !sidecar(&dir.db(), suffix).exists(),
                "{suffix:?} was left behind"
            );
        }
    }

    /// A cache the Python wrote, verbatim from `sqlite3 .dump` over a database
    /// produced by `DiskCache` in `tools/oracle/`. Three frames: a classic one,
    /// a 64-byte extended FD one tagged `tx`, and an empty payload at the
    /// epoch.
    const PYTHON_CACHE: &str = "\
CREATE TABLE frames (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts REAL NOT NULL,
                extended INTEGER NOT NULL,
                is_fd INTEGER NOT NULL,
                arb_id INTEGER NOT NULL,
                dlc INTEGER NOT NULL,
                data BLOB NOT NULL,
                bus INTEGER NOT NULL,
                dir TEXT NOT NULL
            );
INSERT INTO frames VALUES(1,1700000000.123456001,0,0,291,3,X'010203',0,'rx');
INSERT INTO frames VALUES(2,1700000000.98765397,1,1,417001744,15,X'000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f',7,'tx');
INSERT INTO frames VALUES(3,0.0,0,0,2047,0,X'',1,'rx');
INSERT INTO sqlite_sequence VALUES('frames',3);
";

    /// The upgrade that must not lose data: a Pi caching frames through a
    /// gateway outage, upgraded from the Python mid-outage. If this fails, the
    /// frames in that cache are gone.
    #[test]
    fn a_cache_the_python_wrote_reads_back_intact() {
        let dir = TempDir::new("legacy");
        Connection::open(dir.db())
            .unwrap()
            .execute_batch(PYTHON_CACHE)
            .unwrap();

        let mut c = SqliteCache::open(dir.db(), 100).expect("opens a database it did not create");
        assert_eq!(
            c.count().unwrap(),
            3,
            "CREATE TABLE IF NOT EXISTS is a no-op"
        );

        let frames = c.oldest(10).unwrap();
        assert_eq!(
            frames[0].sample,
            CanSample {
                ts_us: 1_700_000_000_123_456,
                arb_id: 0x123,
                extended: false,
                is_fd: false,
                data: vec![1, 2, 3],
                bus: SourceId(0),
                dir: Direction::Rx,
            }
        );
        assert_eq!(
            frames[1].sample,
            CanSample {
                ts_us: 1_700_000_000_987_654,
                arb_id: 0x18DA_F110,
                extended: true,
                is_fd: true,
                data: (0..64).collect(),
                bus: SourceId(7),
                dir: Direction::Tx,
            }
        );
        assert_eq!(frames[2].sample.ts_us, 0, "the epoch is a timestamp too");
        assert!(frames[2].sample.data.is_empty());

        // And draining it works, which is the point of being able to read it.
        c.remove(&frames).unwrap();
        assert_eq!(c.count().unwrap(), 0);
    }

    /// The other direction, which no test here can run the Python for: assert
    /// the columns it would `SELECT` are all present and hold what it expects,
    /// including the `dlc` this implementation does not itself read back.
    #[test]
    fn a_python_could_read_what_this_writes() {
        let (_dir, mut c) = temp_cache("forward-compat", 100);
        c.append(&[CanSample {
            ts_us: 1_700_000_000_123_456,
            arb_id: 0x18DA_F110,
            extended: true,
            is_fd: true,
            data: vec![0xAA; 12],
            bus: SourceId(7),
            dir: Direction::Tx,
        }])
        .unwrap();

        let row: (f64, i64, i64, i64, i64, Vec<u8>, i64, String) = c
            .conn
            .query_row(
                "SELECT ts, extended, is_fd, arb_id, dlc, data, bus, dir FROM frames",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, 1_700_000_000.123_456);
        assert_eq!((row.1, row.2), (1, 1), "booleans store as 0/1");
        assert_eq!(row.3, 0x18DA_F110);
        assert_eq!(row.4, 9, "twelve FD bytes are code 9, not 12");
        assert_eq!(row.5.len(), 12);
        assert_eq!(row.6, 7);
        assert_eq!(row.7, "tx");
    }

    #[test]
    fn an_unopenable_path_is_an_error_rather_than_a_panic() {
        let opened = SqliteCache::open("/proc/nonexistent/cache.db", 10);
        let Err(err) = opened else {
            panic!("a path under /proc is not a cache");
        };
        assert!(!err.to_string().is_empty());
    }
}
