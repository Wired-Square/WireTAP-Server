//! How a [`Sample`] crosses the ingest protocol, in both directions.
//!
//! A CAN sample crosses as the library's `CanFrame`, which maps to a record's
//! fields; the mapping from a sample lives here — once, because
//! [`crate::forward`] encodes it and [`crate::ingest`] decodes it, and the two
//! have to agree with each other and with the gateway's reading of the same
//! record.

use wiretap_model::{CanSample, Direction, ModbusSample, Sample, SerialSample, SourceId};
use wslib_can::CanFrame;
use wslib_wiretap_protocol::ingest as proto;

/// A capture timestamp as the protocol carries it. One spelling, because the
/// base and the deltas measured from it have to agree.
fn ts_us(s: &Sample) -> u64 {
    s.ts_us().max(0) as u64
}

/// The record a sample encodes as, and its payload.
fn parts(s: &Sample) -> (proto::RecordFields, &[u8]) {
    match s {
        Sample::Can(c) => (
            proto::RecordFields::from_can(&can_frame(c), c.dir == Direction::Tx),
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

/// The sample `frame` is, captured at `ts_us` on `bus`.
pub fn can_sample(frame: CanFrame, ts_us: i64, bus: SourceId, dir: Direction) -> CanSample {
    CanSample {
        ts_us,
        arb_id: frame.arb_id,
        extended: frame.extended,
        is_fd: frame.fd,
        rtr: frame.rtr.then(|| frame.dlc()),
        brs: frame.brs,
        esi: frame.esi,
        data: frame.data,
        bus,
        dir,
    }
}

/// The frame `c` holds.
pub fn can_frame(c: &CanSample) -> CanFrame {
    let mut frame = match c.rtr {
        Some(dlc) => CanFrame::remote(c.bus.0, c.arb_id, c.extended, dlc),
        None => CanFrame::data(
            c.bus.0,
            c.arb_id,
            c.extended,
            c.is_fd,
            c.brs,
            c.data.clone(),
        ),
    };
    frame.esi = c.esi;
    frame
}

/// `s` as one record of a `BATCH`.
pub fn record(s: &Sample) -> proto::BatchRecord<'_> {
    let (fields, payload) = parts(s);
    proto::BatchRecord {
        ts_us: ts_us(s),
        fields,
        bus: s.bus().0,
        payload,
    }
}

/// The sample a record describes, stamped at `ts_us`.
pub fn decode(ts_us: i64, r: proto::Record) -> Sample {
    let bus = SourceId(r.bus);
    let fields = proto::RecordFields::from_wire(r.kind, r.id_flags, r.flags);
    match fields {
        proto::RecordFields::Can { transmitted, .. } => {
            let frame = fields.to_can(r.bus, r.payload).expect("a CAN record");
            let dir = if transmitted {
                Direction::Tx
            } else {
                Direction::Rx
            };
            Sample::Can(can_sample(frame, ts_us, bus, dir))
        }
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
                rtr: None,
                brs: false,
                esi: false,
                data: (0..64).collect(),
                bus: SourceId(1),
                dir: Direction::Tx,
            }),
            Sample::Can(CanSample {
                ts_us: BASE + 8,
                arb_id: 0x123,
                extended: false,
                is_fd: true,
                rtr: None,
                brs: true,
                esi: true,
                data: vec![0xAA; 12],
                bus: SourceId(1),
                dir: Direction::Rx,
            }),
            Sample::Can(CanSample {
                ts_us: BASE + 8,
                arb_id: 0x7DF,
                extended: false,
                is_fd: false,
                rtr: Some(8),
                brs: false,
                esi: false,
                data: Vec::new(),
                bus: SourceId(1),
                dir: Direction::Rx,
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
        let (mut buf, len) = proto::encode_batch_from(1, &mut Vec::new(), &samples, record);
        assert_eq!(len, samples.len());
        let frame = proto::take_frame(&mut buf).unwrap().unwrap();
        let batch = proto::parse_batch(&frame.body, proto::MAX_BATCH_RECORDS)
            .unwrap()
            .unwrap();
        let decoded: Vec<Sample> = batch
            .stamped(false, 0)
            .map(|(ts_us, r)| decode(ts_us as i64, r))
            .collect();
        assert_eq!(decoded, samples);
    }
}
