//! Modbus RTU samples off a serial line, framed and stamped by
//! `wiretap-catalog`'s tap.
//!
//! Every function code is framed and address 0 may start a message: a tap
//! stores bytes, and for it a code left undeclared is that traffic lost. The
//! fabrication rate and the line's measured rates are in the vault under
//! *Modbus Framing*.

use std::time::SystemTime;

use wiretap_catalog::{LineSettings, ModbusRtuOptions, TappedMessage};
use wiretap_model::{ModbusSample, SourceId};

use super::system_time_to_us;
use crate::settings::{Parity, SerialSettings};

impl From<&SerialSettings> for LineSettings {
    fn from(s: &SerialSettings) -> Self {
        Self {
            baud: s.baud,
            data_bits: s.data_bits,
            parity: match s.parity {
                Parity::None => wiretap_catalog::Parity::None,
                Parity::Even => wiretap_catalog::Parity::Even,
                Parity::Odd => wiretap_catalog::Parity::Odd,
            },
            stop_bits: s.stop_bits,
        }
    }
}

/// One serial line's tap, and the bus number its messages carry.
pub struct RtuTap {
    tap: wiretap_catalog::RtuTap,
    bus: SourceId,
}

impl RtuTap {
    pub fn new(bus: SourceId, line: &SerialSettings) -> Self {
        Self {
            tap: wiretap_catalog::RtuTap::new(&ModbusRtuOptions::tapped(), line.into()),
            bus,
        }
    }

    /// One read's bytes, and the wall clock when that read returned.
    pub fn push(&mut self, bytes: &[u8], read_at: SystemTime) -> Vec<ModbusSample> {
        self.tap
            .push(bytes, read_at)
            .into_iter()
            .map(|TappedMessage { at, message }| ModbusSample {
                ts_us: system_time_to_us(at),
                bus: self.bus,
                unit: message.device_address,
                func: message.function,
                crc_valid: message.crc_valid,
                raw: message.raw,
            })
            .collect()
    }

    /// The line was lost: the bytes on either side of the gap do not join.
    pub fn reset(&mut self) {
        self.tap.reset();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::settings::Framing;

    /// CRC-16/MODBUS, low byte first, as it goes on the wire.
    fn with_crc(body: &[u8]) -> Vec<u8> {
        let mut crc: u16 = 0xFFFF;
        for &b in body {
            crc ^= u16::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xA001
                } else {
                    crc >> 1
                };
            }
        }
        let mut out = body.to_vec();
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    fn at(us: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_micros(us)
    }

    fn line_9600(parity: Parity, stop_bits: u8) -> SerialSettings {
        SerialSettings {
            baud: 9600,
            data_bits: 8,
            parity,
            stop_bits,
            framing: Framing::ModbusRtu,
        }
    }

    fn tap_on_9600_8n1(bus: SourceId) -> RtuTap {
        RtuTap::new(bus, &line_9600(Parity::None, 1))
    }

    fn keys(samples: &[ModbusSample]) -> Vec<(u8, u8, usize)> {
        samples
            .iter()
            .map(|s| (s.unit, s.func, s.raw.len()))
            .collect()
    }

    #[test]
    fn the_line_settings_carry_parity_and_stop_bits() {
        let micros = |s: &SerialSettings| LineSettings::from(s).wire_time(1).as_micros();
        assert_eq!(micros(&line_9600(Parity::None, 1)), 1_041, "10 bits");
        assert_eq!(micros(&line_9600(Parity::Even, 2)), 1_250, "12 bits");
        assert_eq!(micros(&line_9600(Parity::Odd, 1)), 1_145, "11 bits");
    }

    /// The Sungrow line's traffic: a vendor code the length table does not
    /// model, and a broadcast.
    #[test]
    fn a_vendor_code_and_a_broadcast_become_samples_on_the_bus() {
        let mut tap = tap_on_9600_8n1(SourceId(2));
        let battery = with_crc(&[0x01, 0x20, 0x01, 0xC8, 0x03, 0x11, 0x1A, 0x00, 0x02]);
        let dispatch = with_crc(&[
            0x00, 0x60, 0x00, 0x00, 0x00, 0x05, 0x0A, 0x00, 0x04, 0x01, 0xBB,
        ]);
        let line = [&battery[..], &dispatch[..]].concat();

        let samples = tap.push(&line, at(1_700_000_000_000_000));
        assert_eq!(keys(&samples), [(1, 0x20, 11), (0, 0x60, 13)]);
        assert_eq!(samples[0].raw, battery);
        assert_eq!(samples[1].raw, dispatch);
        assert!(samples.iter().all(|s| s.crc_valid && s.bus == SourceId(2)));
        assert_eq!(samples[1].ts_us, 1_700_000_000_000_000, "the read's time");
        assert_eq!(
            samples[0].ts_us,
            1_700_000_000_000_000 - 13_541,
            "13 bytes of wire time earlier"
        );
    }

    /// Joined across a gap, half a message leaves the framer lost, holding
    /// the next message until the search gives the false head up.
    #[test]
    fn a_reset_forgets_the_half_message() {
        let request = with_crc(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x01]);

        let mut tap = tap_on_9600_8n1(SourceId(0));
        assert!(tap.push(&request[..5], at(0)).is_empty());
        tap.reset();
        assert_eq!(
            keys(&tap.push(&request, at(1))),
            [(1, 3, 8)],
            "framed at once"
        );

        let mut joined = tap_on_9600_8n1(SourceId(0));
        assert!(joined.push(&request[..5], at(0)).is_empty());
        assert!(joined.push(&request, at(1)).is_empty(), "lost, and holding");
    }

    /// The real line, replayed. `WIRETAP_RS485_RAW` names a capture taken on
    /// the trial box: the bytes off the adapter exactly as `read` returned
    /// them, in order, with no timestamps, delimiters or headers — so message
    /// boundaries are the framer's to find, which is the point. The vault's
    /// figure for the whole 34 MB is 99.975% of the bytes in messages, framed
    /// the same whatever the read size.
    #[test]
    #[ignore = "needs a capture: WIRETAP_RS485_RAW=<path to rs485.raw>"]
    fn the_sungrow_capture_frames_at_the_measured_coverage() {
        let path = std::env::var("WIRETAP_RS485_RAW").expect("WIRETAP_RS485_RAW names the capture");
        let bytes = std::fs::read(&path).expect("the capture");
        let line = LineSettings::from(&line_9600(Parity::None, 1));
        let mut first: Option<Vec<Vec<u8>>> = None;
        for chunk in [64usize, 4096] {
            let mut tap = tap_on_9600_8n1(SourceId(0));
            let mut samples = Vec::new();
            let mut burst = 0;
            for (i, c) in bytes.chunks(chunk).enumerate() {
                // Read as fast as the line can deliver a chunk.
                let batch = tap.push(c, UNIX_EPOCH + line.wire_time(((i + 1) * chunk) as u64));
                if burst == 0 {
                    burst = batch.len();
                }
                samples.extend(batch);
            }
            // Strictly, so the burst the sync releases shares no stamp.
            assert!(
                samples.windows(2).all(|w| w[0].ts_us < w[1].ts_us),
                "{chunk}-byte reads: stamps not increasing"
            );
            eprintln!("{chunk}-byte reads: the sync released {burst} messages");
            let framed: usize = samples.iter().map(|s| s.raw.len()).sum();
            let coverage = framed as f64 / bytes.len() as f64;
            eprintln!(
                "{chunk}-byte reads: {} messages, {framed} of {} bytes ({:.4}%)",
                samples.len(),
                bytes.len(),
                coverage * 100.0
            );
            assert!(
                coverage >= 0.995,
                "{chunk}-byte reads: {framed} of {} bytes framed ({coverage:.4})",
                bytes.len()
            );
            for func in [0x20, 0x60, 0x65] {
                assert!(
                    samples.iter().any(|s| s.func == func),
                    "{chunk}-byte reads: no {func:#04x} message"
                );
            }
            assert!(samples.iter().all(|s| s.crc_valid));
            let raws: Vec<Vec<u8>> = samples.into_iter().map(|s| s.raw).collect();
            match &first {
                None => first = Some(raws),
                Some(f) => assert!(*f == raws, "{chunk}-byte reads framed differently"),
            }
        }
    }
}
