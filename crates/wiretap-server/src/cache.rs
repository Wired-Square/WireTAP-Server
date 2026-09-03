//! Where frames wait when the gateway is not there.
//!
//! The schema and the pragmas are the Python's, byte for byte, because
//! existing Raspberry Pis have a populated `~/.wiretap-server-cache.db` and an
//! upgrade *during* an outage must drain it rather than start an empty one
//! beside it. That is the whole constraint on this module: it could be a
//! better-shaped store, and one day it will be — see [`FrameCache`] — but not
//! at the cost of frames already captured.

use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// Frames moved at a time when adopting a cache an older install left behind.
const ADOPT_BATCH: usize = 1_000;

/// A frame in the cache, with whatever the store needs to find it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached {
    /// Opaque to the caller: a rowid here, a file offset in a segment store.
    pub id: i64,
    /// Shared, because the frames on this path came from a capture that had
    /// already wrapped them and go to a sink that only reads them.
    pub sample: Arc<CanSample>,
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
    fn append(&mut self, frames: &[Arc<CanSample>]) -> Result<usize>;

    /// The oldest `limit` frames, in capture order.
    fn oldest(&mut self, limit: usize) -> Result<Vec<Cached>>;

    /// Forget frames that have been written somewhere durable.
    fn remove(&mut self, frames: &[Cached]) -> Result<()>;

    fn count(&mut self) -> Result<u64>;

    /// Bytes on disk, for comparing against the configured limit.
    fn size_bytes(&self) -> u64;

    /// Whether the store has reached the size it was given.
    fn is_full(&self) -> bool;

    /// Empty the store and give its space back to the filesystem, leaving it
    /// usable. Called once a drain has emptied it, because a gigabyte of
    /// reclaimed SD card is the point.
    fn reset(&mut self) -> Result<()>;
}

/// The Python's cache, and the only implementation of [`FrameCache`].
pub struct SqliteCache {
    conn: Connection,
    path: PathBuf,
    max_bytes: u64,
    /// Tracked rather than counted. SQLite has no O(1) `COUNT(*)` and there is
    /// no index to scan instead, so asking the database would read every BLOB
    /// page — up to `cache_max_mb` of SD card — and the stats line asks every
    /// ten seconds, during an outage, on the task that should be spilling the
    /// queue.
    rows: u64,
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
        let conn = Self::open_conn(&path)?;
        Ok(Self {
            rows: count_rows(&conn)?,
            conn,
            path,
            max_bytes: max_mb.saturating_mul(1024 * 1024),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Take everything out of a cache an older install left behind, oldest
    /// first, and delete it.
    ///
    /// The Python cached to `~/.wiretap-server-cache.db` unconditionally. Now
    /// that the default lives under `StateDirectory=`, an upgrade would leave
    /// whatever an outage had captured in a file nothing reads again. Called at
    /// startup before a frame is enqueued, so the recovered frames are ahead of
    /// everything this run captures — the ordering the drain exists to keep.
    ///
    /// Frames are deleted from the source as they land in the destination, so
    /// an interruption resumes rather than duplicating. Returns how many moved;
    /// a source that is not there is `Ok(0)`.
    pub fn adopt(&mut self, legacy: &Path) -> Result<u64> {
        if !legacy.exists() || legacy == self.path {
            return Ok(0);
        }
        // No size limit: this is a move, and refusing it because the old cache
        // is over the *new* cache's limit would strand exactly the frames the
        // limit was never meant to be about.
        let old = Self::open(legacy, u64::MAX)?;
        let moved = old.rows;
        if moved == 0 {
            old.delete()?;
            return Ok(0);
        }

        // The upgrade case is an empty destination that was created moments
        // ago, and then this is a rename rather than ten million rows through
        // Rust and back into SQLite — which on a full cache is minutes of SD
        // card during which nothing is being captured.
        if self.rows == 0 && self.take_over(old).is_ok() {
            return Ok(moved);
        }

        let mut old = Self::open(legacy, u64::MAX)?;
        loop {
            let batch = old.oldest(ADOPT_BATCH)?;
            if batch.is_empty() {
                break;
            }
            let frames: Vec<Arc<CanSample>> = batch.iter().map(|c| Arc::clone(&c.sample)).collect();
            self.append(&frames)?;
            old.remove(&batch)?;
        }
        old.delete()?;
        Ok(moved)
    }

    /// Replace this cache's file with `other`'s, keeping `other`'s contents.
    ///
    /// Only sound because the caller has established that this cache is empty.
    /// A rename across filesystems fails with `EXDEV` — `$HOME` and
    /// `/var/lib` can be different mounts — which is why the caller keeps the
    /// copying path for when this does not work.
    fn take_over(&mut self, other: Self) -> Result<()> {
        let source = other.path.clone();
        // Dropping the connection closes it, which checkpoints the write-ahead
        // log into the database and removes it — so the file about to be moved
        // is the whole of the cache.
        drop(other);

        // Ours closes too, and its sidecars go: they describe a database that
        // is about to be replaced, and SQLite would read them against the new
        // one. The database itself is left for `rename` to replace atomically.
        self.conn = Connection::open_in_memory()?;
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(sidecar(&self.path, suffix));
        }
        let renamed = std::fs::rename(&source, &self.path);

        // Reopened whatever happened: on a failed rename this restores a
        // working, empty cache and the caller falls back to copying.
        self.conn = Self::open_conn(&self.path)?;
        self.rows = count_rows(&self.conn)?;
        renamed.map_err(|e| {
            CacheError(format!(
                "cannot move {} to {}: {e}",
                source.display(),
                self.path.display()
            ))
        })
    }

    /// Close the connection and delete the file, with its sidecars.
    pub fn delete(self) -> Result<()> {
        // Unlinking a file SQLite still has open leaves it writing to an inode
        // nothing can find, so the connection goes first.
        let Self { conn, path, .. } = self;
        drop(conn);
        unlink(&path)
    }

    /// The connection, with the pragmas and the schema the Python set.
    fn open_conn(path: &Path) -> Result<Connection> {
        let conn = Connection::open(path)?;
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
        Ok(conn)
    }
}

/// The database and the two files SQLite keeps beside it. Named once: a caller
/// that forgets `-wal` reintroduces the size bug `docs/porting-notes.md`
/// describes.
const SIDECARS: [&str; 3] = ["", "-wal", "-shm"];

/// The one full `COUNT(*)`, on opening a file that is empty except on an
/// upgrade or after an outage.
fn count_rows(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM frames", [], |r| r.get(0))?;
    Ok(n.max(0) as u64)
}

/// Remove the database and its sidecars, tolerating any that are absent.
fn unlink(path: &Path) -> Result<()> {
    for suffix in SIDECARS {
        let f = sidecar(path, suffix);
        match std::fs::remove_file(&f) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(CacheError(format!("cannot remove {}: {e}", f.display()))),
        }
    }
    Ok(())
}

/// The cache file, or one of the two files SQLite keeps beside it.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
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
    fn append(&mut self, frames: &[Arc<CanSample>]) -> Result<usize> {
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
        self.rows += frames.len() as u64;
        Ok(frames.len())
    }

    fn oldest(&mut self, limit: usize) -> Result<Vec<Cached>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, ts, extended, is_fd, arb_id, data, bus, dir
             FROM frames ORDER BY id LIMIT ?1",
        )?;
        let mut out = Vec::with_capacity(limit);
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(Cached {
                id: r.get(0)?,
                sample: Arc::new(CanSample {
                    ts_us: from_secs(r.get(1)?),
                    extended: r.get(2)?,
                    is_fd: r.get(3)?,
                    arb_id: r.get(4)?,
                    data: r.get(5)?,
                    bus: SourceId(r.get(6)?),
                    // Read as a borrowed str: `parse()` would allocate a
                    // `String` for the column and a second one to lowercase it,
                    // per row, over millions of rows on a long drain. An
                    // unreadable tag is `rx` — the Python wrote whatever
                    // `--pg-dir` said, so a cache from one could hold anything,
                    // and the direction is not worth dropping a frame over.
                    dir: match r.get_ref(7)?.as_str() {
                        Ok(s) if s.eq_ignore_ascii_case("tx") => Direction::Tx,
                        _ => Direction::Rx,
                    },
                }),
            })
        })?;
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn remove(&mut self, frames: &[Cached]) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        // `prepare_cached` rather than `execute`: the placeholder count is the
        // batch size, which is the same for every batch but the last, so the
        // statement is parsed once for a whole drain rather than once per 500
        // frames.
        let placeholders = std::iter::repeat_n("?", frames.len())
            .collect::<Vec<_>>()
            .join(",");
        let removed = self
            .conn
            .prepare_cached(&format!("DELETE FROM frames WHERE id IN ({placeholders})"))?
            .execute(params_from_iter(frames.iter().map(|f| f.id)))?;
        self.rows = self.rows.saturating_sub(removed as u64);
        Ok(())
    }

    fn count(&mut self) -> Result<u64> {
        Ok(self.rows)
    }

    /// **The write-ahead log counts.** The Python stat'd the database file
    /// alone, so during an outage — exactly when the limit matters — it
    /// under-reported by however much had not been checkpointed, and
    /// `cache_max_mb` was a number the cache could sail past.
    fn size_bytes(&self) -> u64 {
        SIDECARS
            .iter()
            .filter_map(|s| std::fs::metadata(sidecar(&self.path, s)).ok())
            .map(|m| m.len())
            .sum()
    }

    fn is_full(&self) -> bool {
        self.size_bytes() >= self.max_bytes
    }

    /// Delete the file and open a fresh one, as the Python did rather than
    /// `VACUUM`.
    ///
    /// It matters on the hardware this runs on: after a long outage the cache
    /// can be a gigabyte, and `VACUUM` rebuilds it in place — needing that much
    /// free space again on an SD card that has just been filled by the outage.
    /// Unlinking asks for none.
    fn reset(&mut self) -> Result<()> {
        // An in-memory placeholder, so the connection can be closed before the
        // unlink without making the field an `Option` that every other method
        // would have to unwrap.
        self.conn = Connection::open_in_memory()?;
        unlink(&self.path)?;
        self.conn = Self::open_conn(&self.path)?;
        self.rows = 0;
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

    fn sample(ts_us: i64, arb_id: u32) -> Arc<CanSample> {
        Arc::new(CanSample {
            ts_us,
            arb_id,
            extended: false,
            is_fd: false,
            data: vec![1, 2, 3],
            bus: SourceId(0),
            dir: Direction::Rx,
        })
    }

    #[test]
    fn frames_come_back_out_in_the_order_they_went_in() {
        let (_dir, mut c) = temp_cache("fifo", 100);
        let frames: Vec<Arc<CanSample>> = (0..5).map(|i| sample(1_000 + i, i as u32)).collect();
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
        let original = Arc::new(CanSample {
            ts_us: 1_700_000_000_123_456,
            arb_id: 0x18DA_F110,
            extended: true,
            is_fd: true,
            data: (0..64).collect(),
            bus: SourceId(7),
            dir: Direction::Tx,
        });
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
        let frames: Vec<Arc<CanSample>> = (0..2_000).map(|i| sample(i, i as u32)).collect();
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
    /// A reset has to give the space back, not just the rows — that is the
    /// difference between a drained cache and a gigabyte of SD card an
    /// appliance never sees again.
    fn resetting_empties_the_cache_and_the_file() {
        let (_dir, mut c) = temp_cache("reset", 100);
        c.append(&(0..20_000).map(|i| sample(i, 1)).collect::<Vec<_>>())
            .unwrap();
        let grown = c.size_bytes();
        assert!(grown > 100_000, "the cache is worth reclaiming: {grown}");

        c.reset().unwrap();
        assert_eq!(c.count().unwrap(), 0);
        assert!(c.oldest(10).unwrap().is_empty());
        assert!(c.size_bytes() < grown / 2, "{} of {grown}", c.size_bytes());

        // And it is usable afterwards, which is what separates this from
        // `delete`.
        c.append(&[sample(1, 0x321)]).unwrap();
        assert_eq!(c.oldest(1).unwrap()[0].sample.arb_id, 0x321);
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
            *frames[0].sample,
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
            *frames[1].sample,
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
        c.append(&[Arc::new(CanSample {
            ts_us: 1_700_000_000_123_456,
            arb_id: 0x18DA_F110,
            extended: true,
            is_fd: true,
            data: vec![0xAA; 12],
            bus: SourceId(7),
            dir: Direction::Tx,
        })])
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

    /// The upgrade path, taken as a rename because the destination is the
    /// empty file a fresh install just made.
    #[test]
    fn a_previous_cache_is_adopted_whole() {
        let dir = TempDir::new("adopt");
        let legacy = dir.0.join("old.db");
        {
            let mut old = SqliteCache::open(&legacy, 100).unwrap();
            old.append(&(0..2_000).map(|i| sample(i, i as u32)).collect::<Vec<_>>())
                .unwrap();
        }

        let mut cache = SqliteCache::open(dir.db(), 100).unwrap();
        assert_eq!(cache.adopt(&legacy).unwrap(), 2_000);
        assert_eq!(cache.count().unwrap(), 2_000);
        assert!(!legacy.exists(), "and the old one is gone");

        // In order, and usable — the frames are older than anything this run
        // will capture, so they have to come out first.
        let oldest = cache.oldest(3).unwrap();
        assert_eq!(
            oldest.iter().map(|c| c.sample.arb_id).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        cache.append(&[sample(9_999, 0x321)]).unwrap();
        assert_eq!(cache.count().unwrap(), 2_001);
    }

    /// The copying path, taken when the destination already holds frames —
    /// a second start after a partial adoption, or a restart during an outage.
    #[test]
    fn a_previous_cache_merges_into_a_populated_one() {
        let dir = TempDir::new("adopt-merge");
        let legacy = dir.0.join("old.db");
        {
            let mut old = SqliteCache::open(&legacy, 100).unwrap();
            old.append(&(0..5).map(|i| sample(i, i as u32)).collect::<Vec<_>>())
                .unwrap();
        }

        let mut cache = SqliteCache::open(dir.db(), 100).unwrap();
        cache.append(&[sample(100, 0xAAA)]).unwrap();
        assert_eq!(cache.adopt(&legacy).unwrap(), 5);
        assert_eq!(cache.count().unwrap(), 6);
        assert!(!legacy.exists());
    }

    #[test]
    fn adopting_nothing_is_not_an_error() {
        let dir = TempDir::new("adopt-nothing");
        let mut cache = SqliteCache::open(dir.db(), 100).unwrap();
        assert_eq!(cache.adopt(&dir.0.join("absent.db")).unwrap(), 0);
        // Its own path is not a previous cache, however it is spelled.
        assert_eq!(cache.adopt(&dir.db()).unwrap(), 0);

        // An empty one is removed rather than left to be reconsidered.
        let empty = dir.0.join("empty.db");
        drop(SqliteCache::open(&empty, 100).unwrap());
        assert_eq!(cache.adopt(&empty).unwrap(), 0);
        assert!(!empty.exists());
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
