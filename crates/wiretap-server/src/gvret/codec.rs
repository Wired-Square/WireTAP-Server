//! GVRET wire codec: bytes in, commands out; frames in, bytes out.
//!
//! Pure and synchronous, so the bytes a desktop client sees can be asserted
//! against the Python implementation without opening a socket.
//!
//! Protocol reference: <https://github.com/collin80/M2RET/blob/master/CommProtocol.txt>
//!
//! | Opcode  | Direction | Meaning            |
//! |---------|-----------|--------------------|
//! | `F1 00` | both      | CAN frame (TX in, RX out) |
//! | `F1 01` | out       | timebase           |
//! | `F1 06` | out       | CAN bus parameters |
//! | `F1 07` | out       | device info        |
//! | `F1 09` | out       | keepalive          |
//! | `F1 0C` | out       | bus count          |

use wiretap_model::{dlc_to_len, payload_dlc};

/// Arbitration id masks. Named for the id width rather than for a protocol:
/// GVRET, SocketCAN and the ingest protocol all mask the same 29 and 11 bits
/// but pack their flags at different positions.
pub const ARB_MASK_EXT: u32 = 0x1FFF_FFFF;
pub const ARB_MASK_STD: u32 = 0x0000_07FF;

/// GVRET marks an extended id with the top bit. SocketCAN's `CAN_EFF_FLAG`
/// happens to sit at the same position; the ingest protocol's does not (bit
/// 29). Written out rather than shared, because the coincidence is not a
/// contract.
const GVRET_EFF_BIT: u32 = 0x8000_0000;

const SYNC: [u8; 2] = [0xE7, 0xE7];
const CMD: u8 = 0xF1;

/// Longest frame this encoder emits: 2 opcode + 4 ts + 4 id + 1 bus/dlc +
/// 64 payload + 1 terminator.
pub const MAX_FRAME_BYTES: usize = 76;

/// Something the client asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientCommand {
    /// `F1 00` — transmit this frame on the bus.
    Transmit {
        bus: u8,
        arb_id: u32,
        extended: bool,
        data: Vec<u8>,
    },
    /// `F1 01`
    Timebase,
    /// `F1 06`
    CanbusParams,
    /// `F1 07`
    DevInfo,
    /// `F1 09`
    Keepalive,
    /// `F1 0C`
    NumBuses,
}

/// Split a GVRET id word into its arbitration id and extended flag.
fn split_gvret_id(raw: u32) -> (u32, bool) {
    let extended = raw & GVRET_EFF_BIT != 0;
    (
        raw & if extended { ARB_MASK_EXT } else { ARB_MASK_STD },
        extended,
    )
}

/// The inverse of [`split_gvret_id`].
fn make_gvret_id(arb_id: u32, extended: bool) -> u32 {
    if extended {
        (arb_id & ARB_MASK_EXT) | GVRET_EFF_BIT
    } else {
        arb_id & ARB_MASK_STD
    }
}

/// Incremental decoder for one client connection.
#[derive(Debug, Default)]
pub struct Decoder {
    binary: bool,
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Has the client sent the `E7 E7` handshake? Frames are only pushed to a
    /// client in binary mode.
    pub fn is_binary(&self) -> bool {
        self.binary
    }

    /// Feed received bytes, returning every complete command they produced.
    /// Partial commands stay buffered.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<ClientCommand> {
        self.scan_for_handshake(chunk);
        let mut out = Vec::new();
        if !self.binary {
            return out;
        }

        loop {
            // Resync: bytes that cannot start a command are skipped, so a
            // stream that loses framing recovers instead of wedging.
            let keep = self
                .buf
                .iter()
                .position(|&b| b == CMD)
                .unwrap_or(self.buf.len());
            self.buf.drain(..keep);

            if self.buf.len() < 2 {
                return out;
            }
            let cmd = self.buf[1];

            if cmd == 0x00 {
                match self.take_transmit() {
                    Some(c) => out.push(c),
                    // Incomplete: leave it buffered for the next read.
                    None => return out,
                }
                continue;
            }

            self.buf.drain(..2);
            match cmd {
                0x01 => out.push(ClientCommand::Timebase),
                0x06 => out.push(ClientCommand::CanbusParams),
                0x07 => out.push(ClientCommand::DevInfo),
                0x09 => out.push(ClientCommand::Keepalive),
                0x0C => out.push(ClientCommand::NumBuses),
                // Unknown opcode: header consumed, request ignored.
                _ => {}
            }
        }
    }

    /// Append `chunk` and consume everything up to and including the last
    /// `E7 E7`, latching binary mode.
    ///
    /// The scan deliberately runs even once latched — a quirk carried over
    /// from the Python; see `sync_bytes_are_consumed_even_in_binary_mode`.
    ///
    /// Only the appended region can hold a new handshake, plus one byte of
    /// overlap: this function never leaves a `SYNC` behind, and the command
    /// loop only removes prefixes, which cannot create an adjacency. Starting
    /// the scan there is what stops a client that connects and never
    /// handshakes from costing O(n²) — every read would otherwise rescan an
    /// ever-growing buffer.
    fn scan_for_handshake(&mut self, chunk: &[u8]) {
        let from = self.buf.len().saturating_sub(1);
        self.buf.extend_from_slice(chunk);

        let (mut i, mut cut) = (from, 0);
        while i + 1 < self.buf.len() {
            if self.buf[i..i + 2] == SYNC {
                i += 2;
                cut = i;
                self.binary = true;
            } else {
                i += 1;
            }
        }
        self.buf.drain(..cut);
    }

    /// `F1 00 <id:4LE> <bus:1> <len:1> <data:len>`; `None` until complete.
    fn take_transmit(&mut self) -> Option<ClientCommand> {
        // The caller has already established the two header bytes.
        debug_assert!(self.buf.len() >= 2 && self.buf[0] == CMD && self.buf[1] == 0x00);
        if self.buf.len() < 8 {
            return None;
        }
        // The declared length decides how many bytes to CONSUME, while the
        // payload is clamped to 8. Replicated from the Python so a malformed
        // request desynchronises both implementations identically; see
        // `an_overlong_declared_length_consumes_all_of_it`.
        let declared = self.buf[7] as usize;
        let need = 8 + declared;
        if self.buf.len() < need {
            return None;
        }

        let raw_id = u32::from_le_bytes([self.buf[2], self.buf[3], self.buf[4], self.buf[5]]);
        let bus = self.buf[6];
        let data = self.buf[8..8 + declared.min(8)].to_vec();
        self.buf.drain(..need);

        let (arb_id, extended) = split_gvret_id(raw_id);
        Some(ClientCommand::Transmit {
            bus,
            arb_id,
            extended,
            data,
        })
    }
}

/// `F1 07` — device info. The constants are what the Python advertised and
/// what clients have been parsing; they are not derived from anything.
pub fn encode_dev_info() -> Vec<u8> {
    let build: u16 = 400;
    let mut v = vec![CMD, 0x07];
    v.extend_from_slice(&build.to_le_bytes());
    v.extend_from_slice(&[1, 0, 0, 0]); // eeprom version, file type, auto start, single wire
    v
}

/// `F1 06` — CAN bus parameters. This legacy field describes at most two
/// buses; further buses are only visible through `F1 0C`.
pub fn encode_canbus_params(bus_count: u8, speeds: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12);
    v.extend_from_slice(&[CMD, 0x06]);
    for i in 0..2 {
        let enabled = bus_count as usize > i;
        let speed = if enabled {
            speeds.get(i).copied().unwrap_or(0)
        } else {
            0
        };
        v.push(u8::from(enabled)); // listen-only, bit 4, is always 0
        v.extend_from_slice(&speed.to_le_bytes());
    }
    v
}

/// `F1 0C` — number of buses.
pub fn encode_num_buses(n: u8) -> Vec<u8> {
    vec![CMD, 0x0C, n]
}

/// `F1 01` — microseconds since the connection opened.
pub fn encode_timebase(us: u32) -> Vec<u8> {
    let mut v = vec![CMD, 0x01];
    v.extend_from_slice(&us.to_le_bytes());
    v
}

/// `F1 09` — keepalive, with its fixed `DE AD` body.
pub fn encode_keepalive() -> Vec<u8> {
    vec![CMD, 0x09, 0xDE, 0xAD]
}

/// Append `F1 00` — a captured frame — to `out`.
///
/// Takes a buffer rather than returning one because this runs once per frame
/// per connected client: a busy 1 Mbit/s bus is ~15k frames/s, so an owned
/// `Vec` per frame is that many allocations per client per second. The bytes
/// are client-independent, so a caller should encode once and share.
///
/// The byte packing `bus` and `dlc` carries the **data length code** in its
/// low nibble, not the byte count. Above 8 bytes on CAN FD those differ, and
/// the desktop's parser depends on getting the code.
pub fn encode_frame_into(
    out: &mut Vec<u8>,
    ts_us: u32,
    arb_id: u32,
    extended: bool,
    bus: u8,
    data: &[u8],
    is_fd: bool,
) {
    let dlc = payload_dlc(data.len(), is_fd);
    // The code may round up past what the caller supplied; never read beyond it.
    let take = dlc_to_len(dlc, is_fd).min(data.len());

    out.reserve(12 + take);
    out.extend_from_slice(&[CMD, 0x00]);
    out.extend_from_slice(&ts_us.to_le_bytes());
    out.extend_from_slice(&make_gvret_id(arb_id, extended).to_le_bytes());
    out.push(((bus & 0x0F) << 4) | (dlc & 0x0F));
    out.extend_from_slice(&data[..take]);
    out.push(0x00);
}

/// [`encode_frame_into`] into a fresh buffer. For tests and one-off sends;
/// the per-client fan-out should use the `_into` form.
pub fn encode_frame(
    ts_us: u32,
    arb_id: u32,
    extended: bool,
    bus: u8,
    data: &[u8],
    is_fd: bool,
) -> Vec<u8> {
    let mut v = Vec::with_capacity(MAX_FRAME_BYTES);
    encode_frame_into(&mut v, ts_us, arb_id, extended, bus, data, is_fd);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_decoder() -> Decoder {
        let mut d = Decoder::new();
        assert!(d.feed(&SYNC).is_empty());
        assert!(d.is_binary());
        d
    }

    // --- golden bytes, captured from the Python implementation -------------

    #[test]
    fn dev_info_bytes() {
        assert_eq!(
            encode_dev_info(),
            vec![0xF1, 0x07, 0x90, 0x01, 0x01, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn keepalive_bytes() {
        assert_eq!(encode_keepalive(), vec![0xF1, 0x09, 0xDE, 0xAD]);
    }

    #[test]
    fn num_buses_bytes() {
        assert_eq!(encode_num_buses(2), vec![0xF1, 0x0C, 0x02]);
    }

    #[test]
    fn timebase_bytes() {
        assert_eq!(
            encode_timebase(0x0001_E240),
            vec![0xF1, 0x01, 0x40, 0xE2, 0x01, 0x00]
        );
    }

    #[test]
    fn canbus_params_advertise_two_buses() {
        assert_eq!(
            encode_canbus_params(2, &[500_000, 250_000]),
            vec![0xF1, 0x06, 0x01, 0x20, 0xA1, 0x07, 0x00, 0x01, 0x90, 0xD0, 0x03, 0x00]
        );
    }

    #[test]
    fn canbus_params_zero_the_second_bus_when_only_one_exists() {
        assert_eq!(
            encode_canbus_params(1, &[500_000]),
            vec![0xF1, 0x06, 0x01, 0x20, 0xA1, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn standard_frame_bytes() {
        // bus 0, 11-bit id 0x123, 8 bytes.
        let out = encode_frame(0x1234, 0x123, false, 0, &[1, 2, 3, 4, 5, 6, 7, 8], false);
        assert_eq!(
            out,
            vec![
                0xF1, 0x00, 0x34, 0x12, 0x00, 0x00, // ts
                0x23, 0x01, 0x00, 0x00, // id, no EFF bit
                0x08, // bus 0, dlc 8
                1, 2, 3, 4, 5, 6, 7, 8, 0x00,
            ]
        );
    }

    #[test]
    fn extended_frame_sets_the_top_id_bit() {
        let out = encode_frame(0, 0x18DA_F110, true, 1, &[0xAA], false);
        assert_eq!(&out[6..10], &[0x10, 0xF1, 0xDA, 0x98]); // 0x18DAF110 | 0x80000000
        assert_eq!(out[10], 0x11); // bus 1, dlc 1
    }

    /// The one that is easy to get wrong: for FD the low nibble is the data
    /// length *code*, not the byte count.
    #[test]
    fn fd_frame_packs_the_dlc_code_not_the_length() {
        let out = encode_frame(0, 0x100, false, 0, &[0u8; 32], true);
        assert_eq!(out[10] & 0x0F, 13, "32 bytes is code 13");
        assert_eq!(
            out.len(),
            12 + 32,
            "2 opcode + 4 ts + 4 id + 1 bus/dlc + data + terminator"
        );

        let out = encode_frame(0, 0x100, false, 2, &[0u8; 64], true);
        assert_eq!(out[10], (2 << 4) | 15, "bus 2, 64 bytes is code 15");
        assert_eq!(out.len(), MAX_FRAME_BYTES);
    }

    #[test]
    fn classic_frames_truncate_at_eight_bytes() {
        let out = encode_frame(0, 0x100, false, 0, &[0xFFu8; 20], false);
        assert_eq!(out[10] & 0x0F, 8);
        assert_eq!(out.len(), 12 + 8);
    }

    /// An FD payload whose length has no exact code rounds the code up, but
    /// must not claim bytes the caller did not supply.
    #[test]
    fn fd_frame_with_an_inexact_length_emits_only_what_it_was_given() {
        let out = encode_frame(0, 0x100, false, 0, &[0xABu8; 9], true);
        assert_eq!(out[10] & 0x0F, 9, "9 bytes rounds up to code 9, meaning 12");
        assert_eq!(out.len(), 12 + 9, "but only 9 payload bytes are written");
    }

    #[test]
    fn encode_frame_into_appends_rather_than_replacing() {
        let mut buf = vec![0xEE];
        encode_frame_into(&mut buf, 0, 0x123, false, 0, &[1, 2], false);
        encode_frame_into(&mut buf, 0, 0x124, false, 0, &[3], false);
        assert_eq!(buf[0], 0xEE);
        assert_eq!(buf.len(), 1 + (12 + 2) + (12 + 1));
    }

    #[test]
    fn gvret_ids_round_trip() {
        for (arb, ext) in [
            (0x123u32, false),
            (0x7FF, false),
            (0x18DA_F110, true),
            (0, true),
        ] {
            let (back, back_ext) = split_gvret_id(make_gvret_id(arb, ext));
            assert_eq!((back, back_ext), (arb, ext), "id {arb:#x} extended {ext}");
        }
    }

    // --- decoding ----------------------------------------------------------

    #[test]
    fn nothing_is_decoded_before_the_handshake() {
        let mut d = Decoder::new();
        assert!(!d.is_binary());
        assert_eq!(d.feed(&[0xF1, 0x07]), vec![]);
        assert!(!d.is_binary());
    }

    #[test]
    fn fixed_commands_decode_after_the_handshake() {
        let mut d = binary_decoder();
        let got = d.feed(&[0xF1, 0x07, 0xF1, 0x06, 0xF1, 0x0C, 0xF1, 0x01, 0xF1, 0x09]);
        assert_eq!(
            got,
            vec![
                ClientCommand::DevInfo,
                ClientCommand::CanbusParams,
                ClientCommand::NumBuses,
                ClientCommand::Timebase,
                ClientCommand::Keepalive,
            ]
        );
    }

    #[test]
    fn the_handshake_may_arrive_after_leading_noise() {
        let mut d = Decoder::new();
        let got = d.feed(&[0x00, 0xFF, 0xE7, 0xE7, 0xF1, 0x07]);
        assert!(d.is_binary());
        assert_eq!(got, vec![ClientCommand::DevInfo]);
    }

    /// Overlapping sync bytes are not "seek to the last pair": `E7 E7 E7`
    /// consumes the first pair and leaves one byte behind.
    #[test]
    fn overlapping_sync_bytes_leave_the_odd_byte() {
        let mut d = Decoder::new();
        let got = d.feed(&[0xE7, 0xE7, 0xE7, 0xF1, 0x07]);
        assert!(d.is_binary());
        // The stray 0xE7 is then dropped by the resync, and DevInfo decodes.
        assert_eq!(got, vec![ClientCommand::DevInfo]);
    }

    /// A handshake split across two reads must still be seen — the scan keeps
    /// one byte of overlap for exactly this.
    #[test]
    fn a_handshake_split_across_reads_is_found() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(&[0x00, 0xE7]), vec![]);
        assert!(!d.is_binary());
        let got = d.feed(&[0xE7, 0xF1, 0x07]);
        assert!(d.is_binary(), "the pair spans the read boundary");
        assert_eq!(got, vec![ClientCommand::DevInfo]);
    }

    /// Deliberate stream recovery: a byte that cannot start a command is
    /// dropped rather than stalling the connection.
    #[test]
    fn resync_discards_leading_non_command_bytes() {
        let mut d = binary_decoder();
        let got = d.feed(&[0x00, 0x11, 0x22, 0xF1, 0x07]);
        assert_eq!(got, vec![ClientCommand::DevInfo]);
    }

    #[test]
    fn unknown_opcodes_are_ignored_without_desynchronising() {
        let mut d = binary_decoder();
        let got = d.feed(&[0xF1, 0x42, 0xF1, 0x07]);
        assert_eq!(got, vec![ClientCommand::DevInfo]);
    }

    #[test]
    fn a_split_command_waits_for_the_rest() {
        let mut d = binary_decoder();
        assert_eq!(d.feed(&[0xF1]), vec![]);
        assert_eq!(d.feed(&[0x07]), vec![ClientCommand::DevInfo]);
    }

    #[test]
    fn transmit_decodes_a_standard_frame() {
        let mut d = binary_decoder();
        let got = d.feed(&[
            0xF1, 0x00, 0x23, 0x01, 0x00, 0x00, 0x01, 0x03, 0xAA, 0xBB, 0xCC,
        ]);
        assert_eq!(
            got,
            vec![ClientCommand::Transmit {
                bus: 1,
                arb_id: 0x123,
                extended: false,
                data: vec![0xAA, 0xBB, 0xCC],
            }]
        );
    }

    #[test]
    fn transmit_decodes_an_extended_frame() {
        let mut d = binary_decoder();
        let got = d.feed(&[0xF1, 0x00, 0x10, 0xF1, 0xDA, 0x98, 0x00, 0x01, 0x55]);
        assert_eq!(
            got,
            vec![ClientCommand::Transmit {
                bus: 0,
                arb_id: 0x18DA_F110,
                extended: true,
                data: vec![0x55],
            }]
        );
    }

    /// A partial transmit must not be consumed, or the rest of the stream is
    /// parsed as garbage.
    #[test]
    fn a_partial_transmit_is_left_buffered() {
        let mut d = binary_decoder();
        assert_eq!(
            d.feed(&[0xF1, 0x00, 0x23, 0x01, 0x00, 0x00, 0x00, 0x08, 0x01]),
            vec![]
        );
        let got = d.feed(&[0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        assert_eq!(
            got,
            vec![ClientCommand::Transmit {
                bus: 0,
                arb_id: 0x123,
                extended: false,
                data: vec![1, 2, 3, 4, 5, 6, 7, 8],
            }]
        );
    }

    /// Replicated quirk — see docs/porting-notes.md.
    #[test]
    fn an_overlong_declared_length_consumes_all_of_it() {
        let mut d = binary_decoder();
        let mut msg = vec![0xF1, 0x00, 0x23, 0x01, 0x00, 0x00, 0x00, 10];
        msg.extend_from_slice(&[9u8; 10]);
        msg.extend_from_slice(&[0xF1, 0x07]); // must still be found afterwards
        let got = d.feed(&msg);
        assert_eq!(
            got,
            vec![
                ClientCommand::Transmit {
                    bus: 0,
                    arb_id: 0x123,
                    extended: false,
                    data: vec![9; 8],
                },
                ClientCommand::DevInfo,
            ]
        );
    }

    /// Replicated quirk — see docs/porting-notes.md.
    #[test]
    fn sync_bytes_are_consumed_even_in_binary_mode() {
        let mut d = binary_decoder();
        let got = d.feed(&[0xF1, 0x00, 0x23, 0x01, 0x00, 0x00, 0x00, 0x02, 0xE7, 0xE7]);
        assert_eq!(
            got,
            vec![],
            "the payload's E7 E7 is eaten by the handshake scan"
        );
    }
}
