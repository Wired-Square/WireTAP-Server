//! Synchronous batch writer: COPY a slice of frames into public.capture_frame.
//!
//! The ingest path writes each batch to PostgreSQL inline and only ACKs the
//! client on success (ACK-after-write), so a DB outage immediately
//! back-pressures the device, which then caches durably and retries. There is
//! deliberately no in-process queue — durability lives at the device's disk
//! cache and in PostgreSQL, never in gateway RAM. Also used by the HTTP
//! capture-import endpoint, which manages its own chunking.

use std::fmt::Write as _;

use bytes::Bytes;
use chrono::DateTime;
use deadpool_postgres::Pool;
use futures_util::SinkExt;

#[derive(Debug)]
pub struct FrameRow {
    pub ts_us: i64,
    pub id: u32,
    pub extended: bool,
    pub dlc: u8,
    pub is_fd: bool,
    pub data: Vec<u8>,
    pub bus: u8,
    pub dir_tx: bool,
}

/// COPY a slice of rows into public.capture_frame.
///
/// **The base table, not the `can_frame` view.** The view exists so readers on
/// the pre-2026-09-10 name keep working, and PostgreSQL cannot COPY into one —
/// pointing this at it fails with `cannot copy to view`.
///
/// `protocol` is named rather than left to its column default so the write side
/// and `sql.rs`'s read-side default are visibly the same constant — they must
/// agree, or ingested rows are invisible to every query that does not name a
/// protocol. A [`FrameRow`] is built from `id_flags`, so it is CAN by
/// construction.
pub async fn copy_rows(pool: &Pool, batch: &[FrameRow]) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| format!("pool: {e}"))?;
    let sink = client
        .copy_in(
            "COPY public.capture_frame \
             (ts, protocol, id, extended, dlc, is_fd, data_bytes, bus, dir) \
             FROM STDIN",
        )
        .await
        .map_err(|e| format!("copy_in: {e}"))?;
    futures_util::pin_mut!(sink);

    // 80, not 64: a row is ~72 bytes (3-digit id, 8-byte payload), so the old
    // hint cost one realloc and one full memcpy per batch — 512 KB of each per
    // chunk on the 8192-row HTTP import path.
    let mut buf = String::with_capacity(batch.len() * 80);
    for row in batch {
        let ts = DateTime::from_timestamp_micros(row.ts_us)
            .ok_or_else(|| format!("timestamp out of range: {}", row.ts_us))?;
        // COPY text format: literal backslash is escaped, so bytea hex input
        // (\x…) is written as \\x…
        let _ = writeln!(
            buf,
            "{}\t{}\t{}\t{}\t{}\t{}\t\\\\x{}\t{}\t{}",
            ts.format("%Y-%m-%dT%H:%M:%S%.6f+00:00"),
            crate::sql::DEFAULT_PROTOCOL,
            row.id,
            if row.extended { 't' } else { 'f' },
            row.dlc,
            if row.is_fd { 't' } else { 'f' },
            hex::encode(&row.data),
            row.bus,
            if row.dir_tx { "tx" } else { "rx" },
        );
    }
    sink.send(Bytes::from(buf))
        .await
        .map_err(|e| format!("copy send: {e}"))?;
    sink.finish()
        .await
        .map_err(|e| format!("copy finish: {e}"))?;
    Ok(())
}
