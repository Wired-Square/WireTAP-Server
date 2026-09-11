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
use wiretap_ingest_proto::{
    modbus_unit_func, record_id_fields, FLAG_CRC_VALID, ID_ARB_MASK, ID_TX,
};
use wiretap_model::Protocol;

/// One row of `capture_frame`, whichever protocol it came off.
///
/// Built through [`FrameRow::can`] or [`FrameRow::modbus`], which are the two
/// readings of a wire record this gateway knows — the columns a Modbus row has
/// no answer for take what the schema's `NOT NULL` demands.
#[derive(Debug)]
pub struct FrameRow {
    pub ts_us: i64,
    pub protocol: Protocol,
    pub id: u32,
    pub extended: bool,
    pub dlc: u16,
    pub is_fd: bool,
    pub data: Vec<u8>,
    pub bus: u8,
    pub dir_tx: bool,
    pub unit: Option<i16>,
    pub func: Option<i16>,
    pub crc_valid: Option<bool>,
}

impl FrameRow {
    /// A CAN frame from its `id_flags` word: the arbitration id with the
    /// extended, FD and transmitted bits packed in.
    pub fn can(ts_us: i64, id_flags: u32, bus: u8, data: Vec<u8>) -> Self {
        let (id, extended, is_fd, dir_tx) = record_id_fields(id_flags);
        Self {
            ts_us,
            protocol: Protocol::Can,
            id,
            extended,
            dlc: u16::from(wiretap_protocol::payload_dlc(data.len(), is_fd)),
            is_fd,
            data,
            bus,
            dir_tx,
            unit: None,
            func: None,
            crc_valid: None,
        }
    }

    /// A Modbus message from its `id_flags` word — unit and function code, as
    /// `wiretap_ingest_proto::modbus_id` packs them — and its flags. `dlc` is
    /// the message length, CRC included, not a CAN length code.
    pub fn modbus(ts_us: i64, id_flags: u32, flags: u8, bus: u8, data: Vec<u8>) -> Self {
        let (unit, func) = modbus_unit_func(id_flags);
        Self {
            ts_us,
            protocol: Protocol::Modbus,
            id: id_flags & ID_ARB_MASK,
            extended: false,
            dlc: data.len() as u16,
            is_fd: false,
            data,
            bus,
            dir_tx: id_flags & ID_TX != 0,
            unit: Some(i16::from(unit)),
            func: Some(i16::from(func)),
            crc_valid: Some(flags & FLAG_CRC_VALID != 0),
        }
    }
}

/// A nullable column in COPY text: the value, or `\N`.
struct Nullable<T>(Option<T>);

impl<T: std::fmt::Display> std::fmt::Display for Nullable<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(v) => v.fmt(f),
            None => f.write_str("\\N"),
        }
    }
}

/// COPY a slice of rows into public.capture_frame.
///
/// **The base table, not the `can_frame` view.** The view exists so readers on
/// the pre-2026-09-10 name keep working, and PostgreSQL cannot COPY into one —
/// pointing this at it fails with `cannot copy to view`.
///
pub async fn copy_rows(pool: &Pool, batch: &[FrameRow]) -> Result<(), String> {
    let client = pool.get().await.map_err(|e| format!("pool: {e}"))?;
    let sink = client
        .copy_in(
            "COPY public.capture_frame \
             (ts, protocol, id, extended, dlc, is_fd, data_bytes, bus, dir, unit, func, crc_valid) \
             FROM STDIN",
        )
        .await
        .map_err(|e| format!("copy_in: {e}"))?;
    futures_util::pin_mut!(sink);

    // Sized to the batch: a CAN row is ~81 bytes of text with an 8-byte
    // payload, and a Modbus row's payload alone is two hex characters a byte.
    let mut buf = String::with_capacity(batch.iter().map(|r| 72 + 2 * r.data.len()).sum());
    for row in batch {
        let ts = DateTime::from_timestamp_micros(row.ts_us)
            .ok_or_else(|| format!("timestamp out of range: {}", row.ts_us))?;
        // COPY text format: literal backslash is escaped, so bytea hex input
        // (\x…) is written as \\x…
        let _ = writeln!(
            buf,
            "{}\t{}\t{}\t{}\t{}\t{}\t\\\\x{}\t{}\t{}\t{}\t{}\t{}",
            ts.format("%Y-%m-%dT%H:%M:%S%.6f+00:00"),
            row.protocol.as_str(),
            row.id,
            if row.extended { 't' } else { 'f' },
            row.dlc,
            if row.is_fd { 't' } else { 'f' },
            hex::encode(&row.data),
            row.bus,
            if row.dir_tx { "tx" } else { "rx" },
            Nullable(row.unit),
            Nullable(row.func),
            Nullable(row.crc_valid.map(|v| if v { 't' } else { 'f' })),
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

#[cfg(test)]
mod tests {
    use super::*;
    use wiretap_ingest_proto::{modbus_id, ID_FD};

    #[test]
    fn a_modbus_row_fills_the_columns_a_can_row_leaves_null() {
        let raw = vec![
            0x01, 0x20, 0x01, 0xC8, 0x03, 0x11, 0x1A, 0x00, 0x02, 0xE4, 0xCA,
        ];
        let m = FrameRow::modbus(5, modbus_id(1, 0x20), FLAG_CRC_VALID, 2, raw.clone());
        assert_eq!((m.protocol, m.id, m.dlc), (Protocol::Modbus, 0x0120, 11));
        assert_eq!(
            (m.unit, m.func, m.crc_valid),
            (Some(1), Some(0x20), Some(true))
        );
        assert!(!m.extended && !m.is_fd && !m.dir_tx);
        assert_eq!(m.data, raw);

        let c = FrameRow::can(5, 0x7E0 | ID_FD | ID_TX, 0, vec![0; 12]);
        assert_eq!((c.protocol, c.id, c.dlc), (Protocol::Can, 0x7E0, 9));
        assert!(c.is_fd && c.dir_tx && !c.extended);
        assert_eq!((c.unit, c.func, c.crc_valid), (None, None, None));
    }
}
