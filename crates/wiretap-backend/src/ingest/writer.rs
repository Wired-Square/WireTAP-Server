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
use wiretap_model::Protocol;
use wiretap_protocol::ingest::{RecordFields, RecordKind, ID_ARB_MASK};

/// One row of `capture_frame`, whichever protocol it came off. A column the
/// row's protocol has no answer for is NULL, and
/// `capture_frame_protocol_columns_check` holds each protocol to its own.
#[derive(Debug)]
pub struct FrameRow {
    pub ts_us: i64,
    pub protocol: Protocol,
    pub id: u32,
    pub extended: Option<bool>,
    pub dlc: u16,
    pub is_fd: Option<bool>,
    pub data: Vec<u8>,
    pub bus: u8,
    pub dir_tx: bool,
    pub unit: Option<i16>,
    pub func: Option<i16>,
    pub crc_valid: Option<bool>,
}

impl FrameRow {
    /// A wire record's row. A Modbus row's `dlc` is the message length, CRC
    /// included, and a raw serial row's the chunk length, not a CAN length code.
    pub fn new(
        ts_us: i64,
        kind: RecordKind,
        id_flags: u32,
        flags: u8,
        bus: u8,
        data: Vec<u8>,
    ) -> Self {
        match RecordFields::from_wire(kind, id_flags, flags) {
            RecordFields::Can {
                arb_id,
                extended,
                fd,
                transmitted,
            } => Self {
                ts_us,
                protocol: Protocol::Can,
                id: arb_id,
                extended: Some(extended),
                dlc: u16::from(wiretap_protocol::payload_dlc(data.len(), fd)),
                is_fd: Some(fd),
                data,
                bus,
                dir_tx: transmitted,
                unit: None,
                func: None,
                crc_valid: None,
            },
            RecordFields::Modbus {
                unit,
                func,
                crc_valid,
                transmitted,
            } => Self {
                ts_us,
                protocol: Protocol::Modbus,
                id: id_flags & ID_ARB_MASK,
                extended: None,
                dlc: data.len() as u16,
                is_fd: None,
                data,
                bus,
                dir_tx: transmitted,
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
                extended: None,
                dlc: data.len() as u16,
                is_fd: None,
                data,
                bus,
                dir_tx: transmitted,
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

/// COPY a slice of rows into public.capture_frame.
///
/// **The base table, not the `can_frame` view.** The view exists so readers on
/// the pre-2026-09-10 name keep working, and PostgreSQL cannot COPY into one —
/// pointing this at it fails with `cannot copy to view`.
///
pub async fn copy_rows(pool: &Pool, batch: &[FrameRow]) -> Result<(), CopyError> {
    let text = copy_text(batch)?;
    let client = pool.get().await.map_err(|e| CopyError {
        code: None,
        message: format!("pool: {e}"),
    })?;
    let sink = client
        .copy_in(
            "COPY public.capture_frame \
             (ts, protocol, id, extended, dlc, is_fd, data_bytes, bus, dir, unit, func, crc_valid) \
             FROM STDIN",
        )
        .await
        .map_err(CopyError::postgres("copy_in"))?;
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
            "{}\t{}\t{}\t{}\t{}\t{}\t\\\\x{}\t{}\t{}\t{}\t{}\t{}",
            ts.format("%Y-%m-%dT%H:%M:%S%.6f+00:00"),
            row.protocol.as_str(),
            row.id,
            Nullable(row.extended.map(copy_bool)),
            row.dlc,
            Nullable(row.is_fd.map(copy_bool)),
            hex::encode(&row.data),
            row.bus,
            if row.dir_tx { "tx" } else { "rx" },
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
        assert_eq!((m.extended, m.is_fd, m.dir_tx), (None, None, false));
        assert_eq!(m.data, raw);

        let c = FrameRow::new(5, RecordKind::Can, 0x7E0 | ID_FD | ID_TX, 0, 0, vec![0; 12]);
        assert_eq!((c.protocol, c.id, c.dlc), (Protocol::Can, 0x7E0, 9));
        assert_eq!(
            (c.extended, c.is_fd, c.dir_tx),
            (Some(false), Some(true), true)
        );
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
            (r.ts_us, r.protocol, r.id, r.dlc, r.bus, r.dir_tx),
            (7, Protocol::Serial, 0, 256, 3, false)
        );
        assert_eq!((r.extended, r.is_fd), (None, None));
        assert_eq!((r.unit, r.func, r.crc_valid), (None, None, None));
        assert_eq!(r.data, chunk);
        let line = copy_text(&[r]).unwrap();
        assert!(
            line.starts_with(
                "1970-01-01T00:00:00.000007+00:00\tserial\t0\t\\N\t256\t\\N\t\\\\x0001"
            ),
            "{line}"
        );
        assert!(line.ends_with("feff\t3\trx\t\\N\t\\N\t\\N\n"), "{line}");
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
}
