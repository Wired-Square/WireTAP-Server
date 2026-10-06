//! How a [`Sample`] crosses the ingest protocol, in both directions.
//!
//! `wiretap_protocol::ingest` carries no frame type, so the mapping from a
//! sample to a record's fields lives here — once, because [`crate::forward`]
//! encodes it and [`crate::ingest`] decodes it, and the two have to agree with
//! each other and with the gateway's reading of the same record.

use wiretap_model::{CanSample, Direction, ModbusSample, Sample, SerialSample, SourceId};
use wiretap_protocol::ingest as proto;

/// A capture timestamp as the protocol carries it. One spelling, because the
/// base and the deltas measured from it have to agree.
fn ts_us(s: &Sample) -> u64 {
    s.ts_us().max(0) as u64
}

/// The record a sample encodes as, and its payload.
fn parts(s: &Sample) -> (proto::RecordFields, &[u8]) {
    match s {
        Sample::Can(c) => (
            proto::RecordFields::Can {
                arb_id: c.arb_id,
                extended: c.extended,
                fd: c.is_fd,
                rtr: false,
                brs: false,
                esi: false,
                rtr_len: 0,
                transmitted: c.dir == Direction::Tx,
            },
            &c.data,
        ),
        Sample::Modbus(m) => (
            proto::RecordFields::Modbus {
                unit: m.unit,
                func: m.func,
                crc_valid: m.crc_valid,
                transmitted: false,
            },
            &m.raw,
        ),
        Sample::Serial(r) => (
            proto::RecordFields::RawSerial {
                seq: r.seq,
                transmitted: false,
            },
            &r.data,
        ),
    }
}

/// The `(ts_us, kind, payload_len)` that [`proto::fit_batch`] measures a sample by.
pub fn fit_input(s: &Sample) -> (u64, proto::RecordKind, usize) {
    let (fields, payload) = parts(s);
    (ts_us(s), fields.kind(), payload.len())
}

/// Append `s` as one record, measured from `base_ts_us`.
pub fn encode_into(out: &mut Vec<u8>, base_ts_us: u64, s: &Sample) {
    let (fields, payload) = parts(s);
    let (id_flags, flags) = fields.to_wire();
    proto::encode_record_into(
        out,
        base_ts_us,
        ts_us(s),
        fields.kind(),
        flags,
        s.bus().0,
        id_flags,
        payload,
    );
}

/// The sample a record describes, stamped at `ts_us`.
pub fn decode(ts_us: i64, r: proto::Record) -> Sample {
    let bus = SourceId(r.bus);
    match proto::RecordFields::from_wire(r.kind, r.id_flags, r.flags) {
        proto::RecordFields::Can {
            arb_id,
            extended,
            fd,
            transmitted,
            ..
        } => Sample::Can(CanSample {
            ts_us,
            arb_id,
            extended,
            is_fd: fd,
            data: r.payload,
            bus,
            dir: if transmitted {
                Direction::Tx
            } else {
                Direction::Rx
            },
        }),
        proto::RecordFields::Modbus {
            unit,
            func,
            crc_valid,
            ..
        } => Sample::Modbus(ModbusSample {
            ts_us,
            bus,
            unit,
            func,
            crc_valid,
            raw: r.payload,
        }),
        proto::RecordFields::RawSerial { seq, .. } => Sample::Serial(SerialSample {
            ts_us,
            bus,
            seq,
            data: r.payload,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encoded, parsed by the server half, decoded: every field of every kind
    /// comes back, which is what the relay listener depends on.
    #[test]
    fn every_kind_survives_the_round_trip() {
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
            Sample::Serial(SerialSample {
                ts_us: BASE + 11,
                bus: SourceId(3),
                seq: (1 << 31) - 1,
                data: (0..=255).collect(),
            }),
        ];
        let mut records = Vec::new();
        for s in &samples {
            encode_into(&mut records, BASE as u64, s);
        }
        assert_eq!(
            records.len(),
            samples
                .iter()
                .map(|s| {
                    let (_, kind, len) = fit_input(s);
                    proto::record_wire_len(kind, len)
                })
                .sum::<usize>(),
            "fit_input sizes what encode_into writes"
        );

        let mut buf = proto::encode_batch(1, BASE as u64, samples.len() as u16, &records);
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
