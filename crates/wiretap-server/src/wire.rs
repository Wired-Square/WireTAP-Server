//! How a [`Sample`] crosses the ingest protocol, in both directions.
//!
//! `wiretap-ingest-proto` carries scalars and no frame type, so the mapping
//! from a sample to a record's kind, flags and id word lives here — once,
//! because [`crate::forward`] encodes it and [`crate::ingest`] decodes it, and
//! the two have to agree with each other and with the gateway's reading of the
//! same record.

use wiretap_ingest_proto as proto;
use wiretap_model::{CanSample, Direction, ModbusSample, Sample, SourceId};

/// A capture timestamp as the protocol carries it. One spelling, because the
/// base and the deltas measured from it have to agree.
pub fn ts_us(s: &Sample) -> u64 {
    s.ts_us().max(0) as u64
}

/// The record a sample encodes as: kind, flags, id word, payload.
fn parts(s: &Sample) -> (proto::RecordKind, u8, u32, &[u8]) {
    match s {
        Sample::Can(c) => (
            proto::RecordKind::Can,
            0,
            proto::record_id_flags(c.arb_id, c.extended, c.is_fd, c.dir == Direction::Tx),
            &c.data,
        ),
        Sample::Modbus(m) => (
            proto::RecordKind::Modbus,
            if m.crc_valid {
                proto::FLAG_CRC_VALID
            } else {
                0
            },
            proto::modbus_id(m.unit, m.func),
            &m.raw,
        ),
    }
}

/// Bytes the sample takes in a `BATCH` body.
pub fn wire_len(s: &Sample) -> usize {
    let (kind, _, _, payload) = parts(s);
    proto::record_wire_len(kind, payload.len())
}

/// Append `s` as one record, measured from `base_ts_us`.
pub fn encode_into(out: &mut Vec<u8>, base_ts_us: u64, s: &Sample) {
    let (kind, flags, id_flags, payload) = parts(s);
    proto::encode_record_into(
        out,
        base_ts_us,
        ts_us(s),
        kind,
        flags,
        s.bus().0,
        id_flags,
        payload,
    );
}

/// The sample a record describes, stamped at `ts_us`.
pub fn decode(ts_us: i64, r: proto::Record) -> Sample {
    let bus = SourceId(r.bus);
    match r.kind {
        proto::RecordKind::Can => {
            let (arb_id, extended, is_fd, transmitted) = proto::record_id_fields(r.id_flags);
            Sample::Can(CanSample {
                ts_us,
                arb_id,
                extended,
                is_fd,
                data: r.payload,
                bus,
                dir: if transmitted {
                    Direction::Tx
                } else {
                    Direction::Rx
                },
            })
        }
        proto::RecordKind::Modbus => {
            let (unit, func) = proto::modbus_unit_func(r.id_flags);
            Sample::Modbus(ModbusSample {
                ts_us,
                bus,
                unit,
                func,
                crc_valid: r.flags & proto::FLAG_CRC_VALID != 0,
                raw: r.payload,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encoded, parsed by the server half, decoded: every field of both kinds
    /// comes back, which is what the relay listener depends on.
    #[test]
    fn both_kinds_survive_the_round_trip() {
        const BASE: i64 = 1_700_000_000_000_000;
        let samples = [
            Sample::Can(CanSample {
                ts_us: BASE + 7,
                arb_id: 0x18DA_F110,
                extended: true,
                is_fd: true,
                data: (0..64).collect(),
                bus: SourceId(1),
                dir: Direction::Tx,
            }),
            Sample::Modbus(ModbusSample {
                ts_us: BASE + 9,
                bus: SourceId(2),
                unit: 0,
                func: 0x60,
                crc_valid: false,
                raw: vec![0x00, 0x60, 0x00, 0x00, 0x00, 0x05, 0x0A, 0x00],
            }),
        ];
        let mut records = Vec::new();
        for s in &samples {
            encode_into(&mut records, BASE as u64, s);
        }
        assert_eq!(
            records.len(),
            samples.iter().map(wire_len).sum::<usize>(),
            "wire_len is what encode_into writes"
        );

        let mut buf = proto::encode_batch(1, BASE as u64, 2, &records);
        let frame = proto::take_frame(&mut buf).unwrap().unwrap();
        let batch = proto::parse_batch(&frame.body, proto::MAX_BATCH_RECORDS)
            .unwrap()
            .unwrap();
        let decoded: Vec<Sample> = batch
            .records
            .into_iter()
            .map(|r| decode(BASE + i64::from(r.delta_us), r))
            .collect();
        assert_eq!(decoded, samples);
    }
}
