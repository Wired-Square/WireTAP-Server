//! GVRET wire codec: bytes in, commands out; frames in, bytes out.
//!
//! Pure and synchronous on purpose — no sockets, no tasks — because the only
//! way to be sure the port did not change what SavvyCAN and the WireTAP
//! desktop see is to assert exact bytes against the Python implementation.
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

/// SocketCAN's extended-frame flag, as it appears in a `can_id`.
pub const CAN_EFF_FLAG: u32 = 0x8000_0000;
pub const CAN_EFF_MASK: u32 = 0x1FFF_FFFF;
pub const CAN_SFF_MASK: u32 = 0x0000_07FF;

/// GVRET marks extended ids with the top bit, the same bit position
/// SocketCAN uses for `CAN_EFF_FLAG` — but the two are separate conventions
/// that happen to coincide, so the conversion is written out rather than
/// assumed.
const GVRET_EFF_BIT: u32 = 0x8000_0000;

const SYNC: [u8; 2] = [0xE7, 0xE7];
const CMD: u8 = 0xF1;

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
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();

        // Handshake scan. Deliberately runs even once binary mode is latched,
        // matching the Python: see `sync_bytes_are_consumed_even_in_binary_mode`.
        while let Some(idx) = find(&self.buf, &SYNC) {
            self.buf.drain(..idx + 2);
            self.binary = true;
        }

        while self.binary {
            // Resync: in binary mode anything not starting a command is
            // dropped a byte at a time, so a stream that loses framing
            // recovers instead of wedging.
            let keep = self
                .buf
                .iter()
                .position(|&b| b == CMD)
                .unwrap_or(self.buf.len());
            self.buf.drain(..keep);

            if self.buf.len() < 2 {
                break;
            }
            let cmd = self.buf[1];

            if cmd == 0x00 {
                match self.take_transmit() {
                    Some(c) => {
                        out.push(c);
                        continue;
                    }
                    // Incomplete: leave it buffered for the next read.
                    None => break,
                }
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
        out
    }

    /// `F1 00 <id:4LE> <bus:1> <len:1> <data:len>`; `None` until complete.
    fn take_transmit(&mut self) -> Option<ClientCommand> {
        if self.buf.len() < 8 || self.buf[0] != CMD || self.buf[1] != 0x00 {
            return None;
        }
        // The declared length decides how many bytes to CONSUME, but the
        // payload is clamped to 8 below. A client declaring 10 therefore
        // consumes 18 bytes and contributes 8 — replicating the Python, so a
        // malformed request desynchronises both implementations identically
        // rather than only one of them.
        let declared = self.buf[7] as usize;
        let need = 8 + declared;
        if self.buf.len() < need {
            return None;
        }

        let raw_id = u32::from_le_bytes([self.buf[2], self.buf[3], self.buf[4], self.buf[5]]);
        let bus = self.buf[6];
        let take = declared.min(8);
        let data = self.buf[8..8 + take].to_vec();
        self.buf.drain(..need);

        let extended = raw_id & GVRET_EFF_BIT != 0;
        let arb_id = raw_id & if extended { CAN_EFF_MASK } else { CAN_SFF_MASK };
        Some(ClientCommand::Transmit {
            bus,
            arb_id,
            extended,
            data,
        })
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
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
    let flags = |enabled: bool| -> u8 { u8::from(enabled) }; // listen-only bit 4 is always 0
    let speed = |i: usize| -> u32 {
        if bus_count as usize > i {
            speeds.get(i).copied().unwrap_or(0)
        } else {
            0
        }
    };
    let mut v = vec![CMD, 0x06];
    v.push(flags(bus_count >= 1));
    v.extend_from_slice(&speed(0).to_le_bytes());
    v.push(flags(bus_count >= 2));
    v.extend_from_slice(&speed(1).to_le_bytes());
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

/// `F1 00` — a captured frame, pushed to the client.
///
/// The byte packing `bus` and `dlc` carries the **data length code** in its
/// low nibble, not the byte count. For CAN FD above 8 bytes those differ, and
/// the desktop's parser depends on getting the code.
pub fn encode_frame(
    ts_us: u32,
    arb_id: u32,
    extended: bool,
    bus: u8,
    data: &[u8],
    is_fd: bool,
) -> Vec<u8> {
    let take = if is_fd {
        data.len().min(64)
    } else {
        data.len().min(8)
    };
    let dlc = if is_fd {
        crate::gvret::len_to_dlc(take)
    } else {
        take as u8
    };

    let masked = arb_id & if extended { CAN_EFF_MASK } else { CAN_SFF_MASK };
    let gvret_id = masked | if extended { GVRET_EFF_BIT } else { 0 };

    let mut v = Vec::with_capacity(12 + take);
    v.extend_from_slice(&[CMD, 0x00]);
    v.extend_from_slice(&ts_us.to_le_bytes());
    v.extend_from_slice(&gvret_id.to_le_bytes());
    v.push(((bus & 0x0F) << 4) | (dlc & 0x0F));
    v.extend_from_slice(&data[..take]);
    v.push(0x00);
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
            "2 opcode + 4 ts + 4 id + 1 bus/dlc + 32 data + 1 terminator"
        );

        let out = encode_frame(0, 0x100, false, 2, &[0u8; 64], true);
        assert_eq!(out[10], (2 << 4) | 15, "bus 2, 64 bytes is code 15");
    }

    #[test]
    fn classic_frames_truncate_at_eight_bytes() {
        let out = encode_frame(0, 0x100, false, 0, &[0xFFu8; 20], false);
        assert_eq!(out[10] & 0x0F, 8);
        assert_eq!(out.len(), 12 + 8);
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

    /// An over-long declared length consumes what it declared but yields only
    /// eight bytes — matching the Python exactly, so a misbehaving client
    /// desynchronises both implementations the same way.
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

    /// Known quirk, carried over deliberately: the handshake scan runs on
    /// every read, so `E7 E7` inside a transmit payload is swallowed even
    /// after binary mode is latched. Recorded rather than fixed, because the
    /// port's acceptance gate is byte-equality with the Python. See
    /// docs/porting-notes.md.
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
