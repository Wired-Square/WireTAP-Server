//! Modbus RTU messages out of a serial line's bytes.
//!
//! The framing is `wiretap-catalog`'s; this is the glue that turns what it
//! recovers into samples, plus one decision: every function code is framed
//! and address 0 may start a message. The catalogue defaults to a declared
//! allow-list because it decodes what it frames; a tap stores bytes, and for
//! it a code left undeclared is that traffic lost. What the choice costs is
//! resync time: while the framer is lost — at open, or after a gap — a false
//! head is searched until the buffer holds 256 bytes, and the messages behind
//! it all come out of the read that completes the sync. Each is stamped by
//! the read that delivered its last byte, not the one that released it, so
//! the archive does not see the sync. The fabrication rate and the line's
//! measured rates are in the vault under *Modbus Framing*.

use std::collections::VecDeque;

use wiretap_catalog::ModbusRtuStream;
use wiretap_model::{ModbusSample, SourceId};

use crate::settings::SerialSettings;

/// Reads remembered for stamping. The framer holds at most 512 bytes, so a
/// message can never end in a read older than 512 reads back; past this the
/// oldest kept read stands in, which stamps late and never early.
const MAX_READS: usize = 1024;

/// Every function code framed: a modelled one keeps its length rules, so this
/// is "search for anything the table does not know".
fn framer() -> ModbusRtuStream {
    ModbusRtuStream::for_address(None)
        .frame_any_function()
        .allow_broadcast()
}

/// One read, as the stamping needs it: the stream cursor after it, its clock,
/// and the clock of the read before it — a byte in this read arrived after
/// that one returned.
struct ReadEnd {
    end: u64,
    ts_us: i64,
    floor_us: i64,
}

/// One serial line's framer, the bus number its messages carry, and the reads
/// that fed it, so each message is stamped by the read that delivered its
/// last byte rather than the one that released it.
pub struct RtuTap {
    stream: ModbusRtuStream,
    bus: SourceId,
    line: SerialSettings,
    /// Reads not yet passed by a message, oldest first.
    reads: VecDeque<ReadEnd>,
    /// The newest stamp handed out; the archive wants them monotonic.
    last_stamp_us: i64,
}

impl RtuTap {
    pub fn new(bus: SourceId, line: &SerialSettings) -> Self {
        Self {
            stream: framer(),
            bus,
            line: line.clone(),
            reads: VecDeque::new(),
            last_stamp_us: i64::MIN,
        }
    }

    /// Feed what one read returned, taken at `ts_us`; every message it
    /// completed, stamped by [`Self::stamp`].
    pub fn push(&mut self, chunk: &[u8], ts_us: i64) -> Vec<ModbusSample> {
        let messages = self.stream.push_bytes(chunk);
        self.reads.push_back(ReadEnd {
            end: self.stream.bytes_fed(),
            ts_us,
            floor_us: self.reads.back().map_or(i64::MIN, |r| r.ts_us),
        });
        if self.reads.len() > MAX_READS {
            self.reads.pop_front();
        }
        messages
            .into_iter()
            .map(|m| ModbusSample {
                ts_us: self.stamp(m.end_offset),
                bus: self.bus,
                unit: m.device_address,
                func: m.function,
                crc_valid: m.crc_valid,
                raw: m.raw,
            })
            .collect()
    }

    /// When the byte at `end_offset - 1` arrived: the clock of the read that
    /// delivered it, less the wire time of the bytes after it in that read —
    /// never before the read preceding it, never before the previous message.
    /// Messages come out in stream order, so earlier reads are done with.
    fn stamp(&mut self, end_offset: u64) -> i64 {
        while self.reads.len() > 1 && self.reads[0].end < end_offset {
            self.reads.pop_front();
        }
        let read = &self.reads[0];
        let ts = (read.ts_us - self.line.wire_time_us(read.end.saturating_sub(end_offset)))
            .max(read.floor_us)
            .max(self.last_stamp_us);
        self.last_stamp_us = ts;
        ts
    }

    /// Forget a half-read message: the line has been reopened and the bytes
    /// on either side of the gap do not join. The reads go with the framer,
    /// whose cursor restarts with it.
    pub fn reset(&mut self) {
        self.stream = framer();
        self.reads.clear();
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

    /// A tap on a 9600 8N1 line: 10 bits a byte, 1 041.67 µs each.
    fn tap_on_9600_8n1(bus: SourceId) -> RtuTap {
        RtuTap::new(
            bus,
            &SerialSettings {
                baud: 9600,
                data_bits: 8,
                parity: crate::settings::Parity::None,
                stop_bits: 1,
                framing: crate::settings::Framing::ModbusRtu,
            },
        )
    }

    fn keys(samples: &[ModbusSample]) -> Vec<(u8, u8, usize)> {
        samples
            .iter()
            .map(|s| (s.unit, s.func, s.raw.len()))
            .collect()
    }

    #[test]
    fn a_request_and_response_become_two_samples_with_unit_func() {
        let mut tap = tap_on_9600_8n1(SourceId(2));
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
        let mut tap = tap_on_9600_8n1(SourceId(0));
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

        let mut tap = tap_on_9600_8n1(SourceId(0));
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

        let mut tap = tap_on_9600_8n1(SourceId(0));
        assert!(tap.push(&request[..5], 0).is_empty());
        tap.reset();
        assert_eq!(keys(&tap.push(&request, 1)), [(1, 3, 8)], "framed at once");

        let mut joined = tap_on_9600_8n1(SourceId(0));
        assert!(joined.push(&request[..5], 0).is_empty());
        assert!(joined.push(&request, 1).is_empty(), "lost, and holding");
    }

    /// A line opened mid-message: the half message is a false head the framer
    /// holds until the buffer fills, then every message buffered behind it
    /// comes out of one read. Each must carry the clock of the read that
    /// delivered its last byte, not the read that released it.
    #[test]
    fn a_sync_burst_is_stamped_by_the_read_each_message_arrived_in() {
        let request = with_crc(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x01]);
        let mut line = request[..5].to_vec();
        for _ in 0..40 {
            line.extend_from_slice(&request);
        }

        // One byte a read, a millisecond apart, as a slow line is read.
        let mut tap = tap_on_9600_8n1(SourceId(0));
        let mut released = Vec::new();
        for (i, b) in line.iter().enumerate() {
            let batch = tap.push(std::slice::from_ref(b), i as i64 * 1000);
            if !batch.is_empty() {
                released.push(batch);
            }
        }

        let burst = &released[0];
        assert!(
            burst.len() > 1,
            "the sync released {} message(s)",
            burst.len()
        );
        let stamps: Vec<i64> = released.iter().flatten().map(|s| s.ts_us).collect();
        assert!(stamps.windows(2).all(|w| w[0] < w[1]), "{stamps:?}");
        // Message k's last byte is at line offset 5 + 8k + 7, in the read of
        // that index — a millisecond each.
        let expected: Vec<i64> = (0..stamps.len())
            .map(|k| (12 + 8 * k as i64) * 1000)
            .collect();
        assert_eq!(stamps, expected);
        assert_eq!(
            released.iter().flatten().count(),
            40,
            "every message framed"
        );
    }

    /// Several messages in one read are spread by the time their bytes took
    /// on the wire, back from the read's clock: the last one carries it, each
    /// earlier one sits 8 bytes — 8.3 ms at 9600 8N1 — before the next.
    #[test]
    fn messages_in_one_read_are_spread_by_wire_time() {
        let request = with_crc(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x01]);
        let mut tap = tap_on_9600_8n1(SourceId(0));
        assert_eq!(tap.push(&request, 1_000_000).len(), 1);

        let chunk: Vec<u8> = request.iter().copied().cycle().take(10 * 8).collect();
        let stamps: Vec<i64> = tap
            .push(&chunk, 1_100_000)
            .iter()
            .map(|s| s.ts_us)
            .collect();
        assert_eq!(stamps.len(), 10);
        assert_eq!(
            *stamps.last().unwrap(),
            1_100_000,
            "the last byte is the read's"
        );
        assert_eq!(
            stamps[8],
            1_100_000 - 8_333,
            "one message of wire time earlier"
        );
        assert_eq!(stamps[0], 1_100_000 - 75_000, "72 bytes of wire time");
        assert!(stamps.windows(2).all(|w| w[0] < w[1]));
    }

    /// The floors. Wire time cannot reach back past the previous read, whose
    /// return proves those bytes had not arrived; and neither a clock stepped
    /// backwards nor a reopen may hand the archive a stamp below the last.
    #[test]
    fn a_stamp_never_precedes_the_previous_read_or_the_previous_message() {
        let request = with_crc(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x01]);
        let chunk: Vec<u8> = request.iter().copied().cycle().take(10 * 8).collect();

        // Ten messages of wire time (75 ms) in a read half a millisecond
        // after the previous one: all but the last pin to that read.
        let mut tap = tap_on_9600_8n1(SourceId(0));
        tap.push(&request, 1_000_000);
        let stamps: Vec<i64> = tap
            .push(&chunk, 1_000_500)
            .iter()
            .map(|s| s.ts_us)
            .collect();
        assert_eq!(&stamps[..9], &[1_000_000; 9]);
        assert_eq!(stamps[9], 1_000_500);

        // The clock steps back between reads.
        let mut tap = tap_on_9600_8n1(SourceId(0));
        assert_eq!(tap.push(&request, 2_000_000)[0].ts_us, 2_000_000);
        assert_eq!(
            tap.push(&request, 1_000_000)[0].ts_us,
            2_000_000,
            "held, not reversed"
        );

        // A reopen restarts the framer's cursor and the reads with it; the
        // floor on what the archive has been given survives.
        tap.reset();
        assert_eq!(tap.push(&request, 1_500_000)[0].ts_us, 2_000_000);
        assert_eq!(tap.push(&request, 2_500_000)[0].ts_us, 2_500_000);
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
            let mut tap = tap_on_9600_8n1(SourceId(0));
            let mut samples = Vec::new();
            let mut burst = 0;
            for (i, c) in bytes.chunks(chunk).enumerate() {
                // Read as fast as a 9600 8N1 line can deliver a chunk.
                let ts_us = ((i + 1) * chunk) as i64 * 10 * 1_000_000 / 9600;
                let batch = tap.push(c, ts_us);
                if burst == 0 && !batch.is_empty() {
                    burst = batch.len();
                }
                samples.extend(batch);
            }
            let stamps: Vec<i64> = samples.iter().map(|s| s.ts_us).collect();
            assert!(
                stamps.windows(2).all(|w| w[0] < w[1]),
                "{chunk}-byte reads: stamps not increasing"
            );
            let first: std::collections::BTreeSet<i64> =
                stamps.iter().take(burst).copied().collect();
            eprintln!(
                "{chunk}-byte reads: the sync released {burst} messages, {} distinct stamps",
                first.len()
            );
            assert_eq!(
                first.len(),
                burst,
                "{chunk}-byte reads: the sync burst shares stamps"
            );
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
