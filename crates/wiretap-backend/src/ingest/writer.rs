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
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, CopyInSink, GenericClient};
use wiretap_model::Protocol;
use wiretap_protocol::can::CanFlags;
use wiretap_protocol::ingest::{RecordFields, RecordKind, ID_ARB_MASK};

/// One row of `capture_frame`, whichever protocol it came off. A column the
/// row's protocol has no answer for is NULL, and
/// `capture_frame_protocol_columns_check` holds each protocol to its own.
#[derive(Debug)]
pub struct FrameRow {
    pub ts_us: i64,
    pub protocol: Protocol,
    pub id: u32,
    /// `CanFlags` bits; a row off another wire has only `TX`.
    pub flags: u8,
    pub dlc: u16,
    pub data: Vec<u8>,
    pub bus: u8,
    pub unit: Option<i16>,
    pub func: Option<i16>,
    pub crc_valid: Option<bool>,
}

impl FrameRow {
    /// A wire record's row. A CAN row's `dlc` is the length code, a remote
    /// frame's the one it requests; a Modbus row's is the message length, CRC
    /// included, and a raw serial row's the chunk length.
    pub fn new(
        ts_us: i64,
        kind: RecordKind,
        id_flags: u32,
        flags: u8,
        bus: u8,
        data: Vec<u8>,
    ) -> Self {
        let fields = RecordFields::from_wire(kind, id_flags, flags);
        let tx = |transmitted| if transmitted { CanFlags::TX.0 } else { 0 };
        match fields {
            RecordFields::Can { transmitted, .. } => {
                let frame = fields.to_can(bus, data).expect("a CAN record");
                Self {
                    ts_us,
                    protocol: Protocol::Can,
                    id: frame.arb_id,
                    flags: CanFlags::of(&frame, transmitted).0,
                    dlc: u16::from(frame.dlc()),
                    data: frame.data,
                    bus,
                    unit: None,
                    func: None,
                    crc_valid: None,
                }
            }
            RecordFields::Modbus {
                unit,
                func,
                crc_valid,
                transmitted,
            } => Self {
                ts_us,
                protocol: Protocol::Modbus,
                id: id_flags & ID_ARB_MASK,
                flags: tx(transmitted),
                dlc: data.len() as u16,
                data,
                bus,
                unit: Some(i16::from(unit)),
                func: Some(i16::from(func)),
                crc_valid: Some(crc_valid),
            },
            // Id 0, not the read sequence: compression segments by
            // (protocol, id), and an id per read would make every row its own
            // segment.
            RecordFields::RawSerial { transmitted, .. } => Self {
                ts_us,
                protocol: Protocol::Serial,
                id: 0,
                flags: tx(transmitted),
                dlc: data.len() as u16,
                data,
                bus,
                unit: None,
                func: None,
                crc_valid: None,
            },
        }
    }
}

/// A nullable column in COPY text: the value, or `\N`.
struct Nullable<T>(Option<T>);

fn copy_bool(v: bool) -> char {
    if v {
        't'
    } else {
        'f'
    }
}

impl<T: std::fmt::Display> std::fmt::Display for Nullable<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(v) => v.fmt(f),
            None => f.write_str("\\N"),
        }
    }
}

/// Why a COPY failed, with PostgreSQL's SQLSTATE when it gave one.
#[derive(Debug)]
pub struct CopyError {
    pub code: Option<SqlState>,
    message: String,
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl CopyError {
    fn postgres(what: &'static str) -> impl FnOnce(tokio_postgres::Error) -> Self {
        move |e| Self {
            code: e.code().cloned(),
            message: format!("{what}: {e}"),
        }
    }
}

/// A data exception (class 22) or an integrity constraint violation (class 23):
/// the database refused the rows themselves, so a retry fails the same way.
pub fn refused_the_rows(code: Option<&SqlState>) -> bool {
    code.is_some_and(|c| matches!(&c.code()[..2], "22" | "23"))
}

impl From<CopyError> for crate::sql::QueryError {
    fn from(e: CopyError) -> Self {
        if refused_the_rows(e.code.as_ref()) {
            Self::BadRequest(e.message)
        } else {
            Self::Database(e.message)
        }
    }
}

/// The columns this build writes. The gateway writes its own current shape, so
/// a migration must end at a `capture_frame` that takes these.
const COLUMNS: &str = "ts, protocol, id, flags, dlc, data_bytes, bus, unit, func, crc_valid";

/// Where a batch waits while its database is behind or migrating: `COLUMNS`,
/// typed as `capture_frame` types them, and nothing a drain does not need.
const CREATE_PENDING: &str = "CREATE TABLE IF NOT EXISTS public.capture_frame_pending (
    ts timestamptz NOT NULL, ingest_ts timestamptz NOT NULL DEFAULT now(),
    protocol text NOT NULL, id integer NOT NULL, flags smallint NOT NULL,
    dlc smallint NOT NULL, data_bytes bytea NOT NULL, bus integer NOT NULL,
    unit smallint, func smallint, crc_valid boolean)";

/// Advisory lock keys. Every buffered write holds `BUFFER_LOCK` shared and the
/// drain holds it exclusively, so no batch lands in a buffer already drained.
const BUFFER_LOCK: i64 = 0x5754_4150_0001;
const CREATE_PENDING_LOCK: i64 = 0x5754_4150_0002;

fn pool_error(e: impl std::fmt::Display) -> CopyError {
    CopyError {
        code: None,
        message: format!("pool: {e}"),
    }
}

/// COPY a slice of rows into public.capture_frame.
///
/// **The base table, not the `can_frame` view.** The view exists so readers on
/// the pre-2026-09-10 name keep working, and PostgreSQL cannot COPY into one —
/// pointing this at it fails with `cannot copy to view`.
pub async fn copy_rows(pool: &Pool, batch: &[FrameRow]) -> Result<(), CopyError> {
    let text = copy_text(batch)?;
    let client = pool.get().await.map_err(pool_error)?;
    let sink = client
        .copy_in(&copy_into("public.capture_frame"))
        .await
        .map_err(CopyError::postgres("copy_in"))?;
    send_copy(sink, text).await
}

/// Where `buffer_rows` put a batch.
#[derive(Debug, PartialEq)]
pub enum Written {
    Buffered,
    /// The database reached this build's schema meanwhile.
    Live,
}

/// Buffer a batch for a database behind this build's schema, deciding under
/// the drain's lock, so a batch either lands before the drain or finds the
/// database current and goes to `capture_frame`.
pub async fn buffer_rows(pool: &Pool, batch: &[FrameRow]) -> Result<Written, CopyError> {
    let text = copy_text(batch)?;
    let mut client = pool.get().await.map_err(pool_error)?;
    let tx = client
        .transaction()
        .await
        .map_err(CopyError::postgres("begin"))?;
    tx.execute("SELECT pg_advisory_xact_lock_shared($1)", &[&BUFFER_LOCK])
        .await
        .map_err(CopyError::postgres("buffer lock"))?;
    let current = crate::schema::detect_version(&*tx)
        .await
        .map_err(pool_error)?
        == Some(crate::schema::SCHEMA_VERSION);
    let table = if current {
        "public.capture_frame"
    } else {
        // Serialised, because two concurrent `CREATE TABLE IF NOT EXISTS` can
        // both find it missing and one then fails.
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&CREATE_PENDING_LOCK])
            .await
            .map_err(CopyError::postgres("create lock"))?;
        tx.batch_execute(CREATE_PENDING)
            .await
            .map_err(CopyError::postgres("create buffer"))?;
        "public.capture_frame_pending"
    };
    let sink = tx
        .copy_in(&copy_into(table))
        .await
        .map_err(CopyError::postgres("copy_in"))?;
    send_copy(sink, text).await?;
    tx.commit().await.map_err(CopyError::postgres("commit"))?;
    Ok(if current {
        Written::Live
    } else {
        Written::Buffered
    })
}

/// What a drain moved: rows, and the first and last `ts` among them.
#[derive(Debug)]
pub struct Drained {
    pub rows: u64,
    pub first: std::time::SystemTime,
    pub last: std::time::SystemTime,
}

pub async fn has_pending(client: &impl GenericClient) -> Result<bool, String> {
    client
        .query_one(
            "SELECT to_regclass('public.capture_frame_pending') IS NOT NULL",
            &[],
        )
        .await
        .map(|row| row.get(0))
        .map_err(|e| format!("buffer check: {e}"))
}

/// Move a buffer into `capture_frame` and drop it, in one transaction. Only
/// for a database already at this build's schema.
pub async fn drain_pending(client: &mut Client) -> Result<Option<Drained>, String> {
    let pg = |what: &'static str| move |e: tokio_postgres::Error| format!("drain {what}: {e}");
    if !has_pending(client).await? {
        return Ok(None);
    }
    let tx = client.transaction().await.map_err(pg("begin"))?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&BUFFER_LOCK])
        .await
        .map_err(pg("lock"))?;
    // Asked again under the lock: another drain may have got there first.
    if !has_pending(&tx).await? {
        return Ok(None);
    }
    let span = tx
        .query_one(
            "SELECT min(ts), max(ts) FROM public.capture_frame_pending",
            &[],
        )
        .await
        .map_err(pg("span"))?;
    let rows = tx
        .execute(
            &format!(
                "INSERT INTO public.capture_frame (ingest_ts, {COLUMNS}) \
                 SELECT ingest_ts, {COLUMNS} FROM public.capture_frame_pending"
            ),
            &[],
        )
        .await
        .map_err(|e| format!("drain: {}", crate::schema::db_error_detail(&e)))?;
    tx.batch_execute("DROP TABLE public.capture_frame_pending")
        .await
        .map_err(pg("drop"))?;
    tx.commit().await.map_err(pg("commit"))?;
    Ok(match (span.get(0), span.get(1)) {
        (Some(first), Some(last)) => Some(Drained { rows, first, last }),
        _ => None,
    })
}

fn copy_into(table: &str) -> String {
    format!("COPY {table} ({COLUMNS}) FROM STDIN")
}

async fn send_copy(sink: CopyInSink<Bytes>, text: String) -> Result<(), CopyError> {
    futures_util::pin_mut!(sink);
    sink.send(Bytes::from(text))
        .await
        .map_err(CopyError::postgres("copy send"))?;
    sink.finish()
        .await
        .map_err(CopyError::postgres("copy finish"))?;
    Ok(())
}

fn copy_text(batch: &[FrameRow]) -> Result<String, CopyError> {
    // Sized to the batch: a CAN row is ~81 bytes of text with an 8-byte
    // payload, and a Modbus row's payload alone is two hex characters a byte.
    let mut buf = String::with_capacity(batch.iter().map(|r| 72 + 2 * r.data.len()).sum());
    for row in batch {
        // The SQLSTATE PostgreSQL gives the same refusal.
        let ts = DateTime::from_timestamp_micros(row.ts_us).ok_or_else(|| CopyError {
            code: Some(SqlState::DATETIME_FIELD_OVERFLOW),
            message: format!("timestamp out of range: {}", row.ts_us),
        })?;
        // COPY text format: literal backslash is escaped, so bytea hex input
        // (\x…) is written as \\x…
        let _ = writeln!(
            buf,
            "{}\t{}\t{}\t{}\t{}\t\\\\x{}\t{}\t{}\t{}\t{}",
            ts.format("%Y-%m-%dT%H:%M:%S%.6f+00:00"),
            row.protocol.as_str(),
            row.id,
            row.flags,
            row.dlc,
            hex::encode(&row.data),
            row.bus,
            Nullable(row.unit),
            Nullable(row.func),
            Nullable(row.crc_valid.map(copy_bool)),
        );
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiretap_protocol::ingest::{modbus_id, raw_serial_id, FLAG_CRC_VALID, ID_FD, ID_TX};

    #[test]
    fn a_modbus_row_fills_the_columns_a_can_row_leaves_null() {
        let raw = vec![
            0x01, 0x20, 0x01, 0xC8, 0x03, 0x11, 0x1A, 0x00, 0x02, 0xE4, 0xCA,
        ];
        let m = FrameRow::new(
            5,
            RecordKind::Modbus,
            modbus_id(1, 0x20),
            FLAG_CRC_VALID,
            2,
            raw.clone(),
        );
        assert_eq!((m.protocol, m.id, m.dlc), (Protocol::Modbus, 0x0120, 11));
        assert_eq!(
            (m.unit, m.func, m.crc_valid),
            (Some(1), Some(0x20), Some(true))
        );
        assert_eq!(m.flags, 0);
        assert_eq!(m.data, raw);

        let c = FrameRow::new(5, RecordKind::Can, 0x7E0 | ID_FD | ID_TX, 0, 0, vec![0; 12]);
        assert_eq!((c.protocol, c.id, c.dlc), (Protocol::Can, 0x7E0, 9));
        assert_eq!(c.flags, (CanFlags::FD.0 | CanFlags::TX.0));
        assert_eq!((c.unit, c.func, c.crc_valid), (None, None, None));
    }

    /// Every column the schema's check holds a serial row to, and none of the
    /// read sequence.
    #[test]
    fn a_raw_serial_chunk_is_a_serial_row_with_id_0() {
        let chunk: Vec<u8> = (0..=255).collect();
        let r = FrameRow::new(
            7,
            RecordKind::RawSerial,
            raw_serial_id(12_345),
            0,
            3,
            chunk.clone(),
        );
        assert_eq!(
            (r.ts_us, r.protocol, r.id, r.flags, r.dlc, r.bus),
            (7, Protocol::Serial, 0, 0, 256, 3)
        );
        assert_eq!((r.unit, r.func, r.crc_valid), (None, None, None));
        assert_eq!(r.data, chunk);
        let line = copy_text(&[r]).unwrap();
        assert!(
            line.starts_with("1970-01-01T00:00:00.000007+00:00\tserial\t0\t0\t256\t\\\\x0001"),
            "{line}"
        );
        assert!(line.ends_with("feff\t3\t\\N\t\\N\t\\N\n"), "{line}");
    }

    /// The buffer holds what the writer writes, typed as `capture_frame` types
    /// it, or the drain's INSERT … SELECT fails after a migration.
    #[test]
    fn the_buffer_has_the_shape_the_writer_writes() {
        let init = include_str!("../../schema/init_schema.sql");
        let table = &init[init
            .find("CREATE TABLE IF NOT EXISTS public.capture_frame (")
            .unwrap()..];
        let typed = |sql: &str, column: &str| {
            sql.lines().find_map(|line| {
                let mut words = line.split_whitespace();
                (words.next() == Some(column)).then(|| {
                    words
                        .next()
                        .unwrap()
                        .trim_end_matches([',', ')'])
                        .to_string()
                })
            })
        };
        for column in COLUMNS.split(", ").chain(["ingest_ts"]) {
            let ours = typed(&CREATE_PENDING.replace(", ", ",\n"), column);
            assert!(ours.is_some(), "the buffer has no {column}");
            assert_eq!(ours, typed(table, column), "{column}");
        }
    }

    #[test]
    fn a_timestamp_out_of_range_is_a_datetime_field_overflow() {
        let row = FrameRow::new(i64::MAX, RecordKind::Can, 0x123, 0, 0, vec![1]);
        let err = copy_text(&[row]).unwrap_err();
        assert_eq!(err.code, Some(SqlState::DATETIME_FIELD_OVERFLOW));
        assert!(err.to_string().contains("timestamp out of range"), "{err}");
    }

    #[test]
    fn a_failed_import_blames_the_request_only_for_rows_the_database_refused() {
        use crate::sql::QueryError;
        let classify = |code: Option<SqlState>| {
            QueryError::from(CopyError {
                code,
                message: "copy finish: db error".into(),
            })
        };
        for code in [
            SqlState::DATETIME_FIELD_OVERFLOW,
            SqlState::UNIQUE_VIOLATION,
        ] {
            let e = classify(Some(code.clone()));
            assert!(matches!(e, QueryError::BadRequest(_)), "{code:?}");
        }
        for code in [Some(SqlState::CONNECTION_FAILURE), None] {
            let e = classify(code.clone());
            assert!(matches!(e, QueryError::Database(_)), "{code:?}");
        }
    }

    /// The `flags` column's bits are `CanFlags`, and a remote frame stores the
    /// length code it requests with no data, whatever the record carried.
    #[test]
    fn a_can_row_stores_its_flags_and_a_remote_frame_its_requested_code() {
        use wiretap_protocol::ingest::{CAN_FLAG_BRS, CAN_FLAG_ESI, CAN_FLAG_RTR, ID_EXTENDED};
        let remote = FrameRow::new(
            1,
            RecordKind::Can,
            0x7DF,
            CAN_FLAG_RTR | 8 << 3,
            0,
            vec![1, 2],
        );
        let fd = FrameRow::new(
            1,
            RecordKind::Can,
            0x18DA_F110 | ID_EXTENDED | ID_FD | ID_TX,
            CAN_FLAG_BRS | CAN_FLAG_ESI,
            2,
            vec![0xAA; 12],
        );
        let modbus = FrameRow::new(
            1,
            RecordKind::Modbus,
            modbus_id(1, 3) | ID_TX,
            0,
            0,
            vec![1],
        );
        let lines = copy_text(&[remote, fd, modbus]).unwrap();
        let columns: Vec<Vec<&str>> = lines
            .lines()
            .map(|l| l.split('\t').skip(1).take(6).collect())
            .collect();
        assert_eq!(
            columns,
            [
                ["can", "2015", "1", "8", "\\\\x", "0"],
                [
                    "can",
                    "417001744",
                    "62",
                    "9",
                    &format!("\\\\x{}", "aa".repeat(12)),
                    "2"
                ],
                ["modbus", "259", "32", "1", "\\\\x01", "0"],
            ]
        );
    }
}
