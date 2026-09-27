//! Modbus RTU samples off a serial line, framed and stamped by
//! `wiretap-catalog`'s tap.
//!
//! Every function code is framed and address 0 may start a message: a tap
//! stores bytes, and for it a code left undeclared is that traffic lost. The
//! fabrication rate and the line's measured rates are in the vault under
//! *Modbus Framing*. A catalogue's declared lengths frame its codes exactly,
//! where the search can stop short.

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

fn options(line: &SerialSettings) -> ModbusRtuOptions {
    match &line.catalogue {
        Some(c) => c.rtu.clone().allow_broadcast().frame_any_function(),
        None => ModbusRtuOptions::tapped(),
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
            tap: wiretap_catalog::RtuTap::new(&options(line), line.into()),
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

    use wiretap_catalog::Catalog;
    use wiretap_checksum::algorithms::crc16_modbus_checksum;

    use super::*;
    use crate::settings::{Framing, LineCatalogue};

    const EXAMPLE_CATALOGUE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packaging/examples/sungrow-rs485.catalog.toml"
    );

    fn with_crc(body: &[u8]) -> Vec<u8> {
        [body, &crc16_modbus_checksum(body).to_le_bytes()].concat()
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
            catalogue: None,
        }
    }

    fn tap_on_9600_8n1(bus: SourceId) -> RtuTap {
        RtuTap::new(bus, &line_9600(Parity::None, 1))
    }

    fn sungrow_line() -> SerialSettings {
        SerialSettings {
            catalogue: Some(LineCatalogue::read(EXAMPLE_CATALOGUE).expect("the example loads")),
            ..line_9600(Parity::None, 1)
        }
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

    #[test]
    fn the_example_catalogue_declares_the_sungrow_codes() {
        let options = options(&sungrow_line());
        assert_eq!(options.vendor_functions, [0x20, 0x60, 0x65]);
        assert_eq!(options.vendor_lengths.len(), 5);
    }

    /// So a catalogue adds its rules and nothing else, however `tapped()`
    /// changes.
    #[test]
    fn a_catalogue_tap_is_the_plain_tap_plus_its_rules() {
        let empty = Catalog::parse("[meta]\nname = \"empty\"\n").unwrap();
        let line = SerialSettings {
            catalogue: Some(LineCatalogue {
                path: String::new(),
                name: String::new(),
                rtu: empty.rtu_options(),
            }),
            ..line_9600(Parity::None, 1)
        };
        assert_eq!(options(&line), ModbusRtuOptions::tapped());
    }

    /// A message whose first 18 bytes also pass CRC: the search stops there,
    /// and the declared length does not.
    #[test]
    fn a_declared_length_frames_what_the_search_cuts_short() {
        let dispatch = with_crc(&[
            0x00, 0x60, 0x00, 0x00, 0x00, 0x05, 0x0A, 0x00, 0x04, 0x01, 0xBB, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xE5,
        ]);
        assert_eq!(dispatch.len(), 19);
        assert_eq!(with_crc(&dispatch[..16]), dispatch[..18]);

        let framed =
            |line: &SerialSettings| keys(&RtuTap::new(SourceId(0), line).push(&dispatch, at(0)));
        assert_eq!(framed(&line_9600(Parity::None, 1)), [(0, 0x60, 18)]);
        assert_eq!(framed(&sungrow_line()), [(0, 0x60, 19)]);
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

    fn capture() -> Vec<u8> {
        let path = std::env::var("WIRETAP_RS485_RAW").expect("WIRETAP_RS485_RAW names the capture");
        std::fs::read(&path).expect("the capture")
    }

    /// The messages and the share of bytes framed, which must be the same
    /// whatever the read size.
    fn replay(line: &SerialSettings, bytes: &[u8]) -> (Vec<Vec<u8>>, f64) {
        let wire = LineSettings::from(line);
        let mut first: Option<(Vec<Vec<u8>>, f64)> = None;
        for chunk in [64usize, 4096] {
            let mut tap = RtuTap::new(SourceId(0), line);
            let mut samples = Vec::new();
            let mut burst = 0;
            for (i, c) in bytes.chunks(chunk).enumerate() {
                // Read as fast as the line can deliver a chunk.
                let batch = tap.push(c, UNIX_EPOCH + wire.wire_time(((i + 1) * chunk) as u64));
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
            for func in [0x20, 0x60, 0x65] {
                assert!(
                    samples.iter().any(|s| s.func == func),
                    "{chunk}-byte reads: no {func:#04x} message"
                );
            }
            assert!(samples.iter().all(|s| s.crc_valid));
            let raws: Vec<Vec<u8>> = samples.into_iter().map(|s| s.raw).collect();
            match &first {
                None => first = Some((raws, coverage)),
                Some((f, _)) => assert!(*f == raws, "{chunk}-byte reads framed differently"),
            }
        }
        first.expect("a read size")
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
        let (_, coverage) = replay(&line_9600(Parity::None, 1), &capture());
        assert!(coverage >= 0.995, "{coverage:.4}");
    }

    /// The same capture through the example catalogue: no vendor message cut
    /// short of its declared length.
    #[test]
    #[ignore = "needs a capture: WIRETAP_RS485_RAW=<path to rs485.raw>"]
    fn the_example_catalogue_frames_the_sungrow_capture_whole() {
        let (raws, coverage) = replay(&sungrow_line(), &capture());
        let declared = |raw: &Vec<u8>| {
            let count = |at: usize| raw.get(at).map(|&n| n as usize);
            match raw[1] {
                0x60 => !matches!(raw.len(), 18 | 26) && count(6).map(|n| 9 + n) == Some(raw.len()),
                // A request shares the response's selector, at 11 bytes.
                0x20 if raw.get(4) == Some(&0x04) => {
                    raw.len() == 11 || count(5).map(|n| 8 + n) == Some(raw.len())
                }
                0x65 => count(4).map(|n| 7 + n) == Some(raw.len()),
                _ => true,
            }
        };
        let short: Vec<&Vec<u8>> = raws.iter().filter(|r| !declared(r)).collect();
        assert!(
            short.is_empty(),
            "{} short, first {:02x?}",
            short.len(),
            short.first()
        );
        assert!(coverage >= 0.999_949, "{coverage:.6}");
    }
}
