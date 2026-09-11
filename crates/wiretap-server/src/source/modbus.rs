//! Modbus RTU messages out of a serial line's bytes.
//!
//! The framing is `wiretap-catalog`'s; this is the glue that turns what it
//! recovers into samples, plus one decision: every function code is framed
//! and address 0 may start a message. The catalogue defaults to a declared
//! allow-list because it decodes what it frames; a tap stores bytes, and for
//! it a code left undeclared is that traffic lost. What the choice costs is
//! resync time: while the framer is lost — at open, or after a gap — a false
//! head is searched until the buffer holds 256 bytes, so the first messages
//! after a reopen can be stamped late by that many bytes of line time. Its
//! fabrication rate and the line's measured rates are in the vault under
//! *Modbus Framing*.

use wiretap_catalog::ModbusRtuStream;
use wiretap_model::{ModbusSample, SourceId};

/// Every function code declared: a modelled one keeps its length rules, so
/// this is "search for anything the table does not know".
fn framer() -> ModbusRtuStream {
    let all: [u8; 256] = std::array::from_fn(|i| i as u8);
    ModbusRtuStream::for_address(None)
        .with_vendor_functions(&all)
        .allow_broadcast()
}

/// One serial line's framer, and the bus number its messages carry.
pub struct RtuTap {
    stream: ModbusRtuStream,
    bus: SourceId,
}

impl RtuTap {
    pub fn new(bus: SourceId) -> Self {
        Self {
            stream: framer(),
            bus,
        }
    }

    /// Feed what one read returned; every message it completed, stamped at
    /// `ts_us`. A message is stamped when its last byte arrives, so the
    /// timestamp is its end, to within one read.
    pub fn push(&mut self, chunk: &[u8], ts_us: i64) -> Vec<ModbusSample> {
        self.stream
            .push_bytes(chunk)
            .into_iter()
            .map(|m| ModbusSample {
                ts_us,
                bus: self.bus,
                unit: m.device_address,
                func: m.function,
                crc_valid: m.crc_valid,
                raw: m.raw,
            })
            .collect()
    }

    /// Forget a half-read message: the line has been reopened and the bytes
    /// on either side of the gap do not join.
    pub fn reset(&mut self) {
        self.stream = framer();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn keys(samples: &[ModbusSample]) -> Vec<(u8, u8, usize)> {
        samples
            .iter()
            .map(|s| (s.unit, s.func, s.raw.len()))
            .collect()
    }

    #[test]
    fn a_request_and_response_become_two_samples_with_unit_func() {
        let mut tap = RtuTap::new(SourceId(2));
        let request = with_crc(&[0x01, 0x03, 0x1E, 0x87, 0x00, 0x02]);
        let response = with_crc(&[0x01, 0x03, 0x04, 0x00, 0x00, 0x12, 0x34]);
        let mut line = request.clone();
        line.extend_from_slice(&response);

        let samples = tap.push(&line, 1_700_000_000_000_000);
        assert_eq!(keys(&samples), [(1, 3, 8), (1, 3, 9)]);
        assert_eq!(samples[0].raw, request);
        assert_eq!(samples[1].raw, response);
        assert!(samples.iter().all(|s| s.crc_valid && s.bus == SourceId(2)));
        assert_eq!(samples[1].ts_us, 1_700_000_000_000_000, "the chunk's time");
    }

    /// The Sungrow line's traffic: a vendor code the length table does not
    /// model, and a broadcast. Neither is declared anywhere; both are framed.
    #[test]
    fn an_undeclared_vendor_code_is_framed() {
        let mut tap = RtuTap::new(SourceId(0));
        let battery = with_crc(&[0x01, 0x20, 0x01, 0xC8, 0x03, 0x11, 0x1A, 0x00, 0x02]);
        let dispatch = with_crc(&[
            0x00, 0x60, 0x00, 0x00, 0x00, 0x05, 0x0A, 0x00, 0x04, 0x01, 0xBB,
        ]);
        let mut line = battery.clone();
        line.extend_from_slice(&dispatch);

        // Delivered a byte at a time, as a slow line is read.
        let mut samples = Vec::new();
        for (i, b) in line.iter().enumerate() {
            samples.extend(tap.push(std::slice::from_ref(b), i as i64));
        }
        assert_eq!(keys(&samples), [(1, 0x20, 11), (0, 0x60, 13)]);
        assert_eq!(samples[0].raw, battery);
        assert_eq!(samples[1].raw, dispatch);
        assert_eq!(samples[0].ts_us, 10, "stamped when its last byte arrived");
    }

    /// Declaring every code must not turn the search loose on a standard
    /// message: a long FC03 response whose interior happens to CRC-validate
    /// at a shorter length is still framed by the length table, whole.
    #[test]
    fn a_standard_code_keeps_its_length_rule_beside_the_search() {
        // A 125-register read response, 255 bytes, from a seed found by
        // searching: its 108-byte prefix carries a valid CRC of its own.
        let mut body = vec![0x01, 0x03, 250];
        let mut x = 138u32.wrapping_mul(2_654_435_761);
        for _ in 0..250 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            body.push(x as u8);
        }
        let response = with_crc(&body);
        assert!(
            (4..response.len()).any(|n| with_crc(&response[..n - 2]) == response[..n]),
            "the seed no longer yields a spurious inner CRC"
        );

        let mut tap = RtuTap::new(SourceId(0));
        let samples = tap.push(&response, 0);
        assert_eq!(keys(&samples), [(1, 3, 255)]);
        assert_eq!(samples[0].raw, response);
    }

    /// Half a message before a gap must not be joined to what follows it:
    /// joined, the framer is lost and holds the next message until the search
    /// gives the false head up.
    #[test]
    fn a_reset_forgets_the_half_message() {
        let request = with_crc(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x01]);

        let mut tap = RtuTap::new(SourceId(0));
        assert!(tap.push(&request[..5], 0).is_empty());
        tap.reset();
        assert_eq!(keys(&tap.push(&request, 1)), [(1, 3, 8)], "framed at once");

        let mut joined = RtuTap::new(SourceId(0));
        assert!(joined.push(&request[..5], 0).is_empty());
        assert!(joined.push(&request, 1).is_empty(), "lost, and holding");
    }

    /// The real line, replayed. `WIRETAP_RS485_RAW` names a capture taken on
    /// the trial box: the bytes off the adapter exactly as `read` returned
    /// them, in order, with no timestamps, delimiters or headers — so message
    /// boundaries are the framer's to find, which is the point. The vault's
    /// figure for the whole 34 MB with this framer is 99.97% of the bytes in
    /// messages.
    #[test]
    #[ignore = "needs a capture: WIRETAP_RS485_RAW=<path to rs485.raw>"]
    fn the_sungrow_capture_frames_at_the_measured_coverage() {
        let path = std::env::var("WIRETAP_RS485_RAW").expect("WIRETAP_RS485_RAW names the capture");
        let bytes = std::fs::read(&path).expect("the capture");
        for chunk in [64usize, 4096] {
            let mut tap = RtuTap::new(SourceId(0));
            let mut samples = Vec::new();
            for (i, c) in bytes.chunks(chunk).enumerate() {
                samples.extend(tap.push(c, i as i64));
            }
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
        }
    }
}
