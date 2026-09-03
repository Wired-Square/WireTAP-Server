//! What a capture source produces.
//!
//! `Sample` is an enum from the outset, with only one variant implemented, so
//! that adding Modbus register capture later is additive rather than a
//! refactor of every queue, sink and bridge between here and the database.
//! The Python implementation this is ported from had CAN frames hard-wired
//! through the whole pipeline.

use serde::{Deserialize, Serialize};

/// Which interface a sample came from. `bus` is the GVRET-visible number,
/// which is the socket's index plus the configured `bus_offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceId(pub u8);

/// Frame direction. Captured frames are `Rx`; `Tx` is used for frames this
/// server transmitted on behalf of a GVRET client, so an archive can tell
/// them apart from bus traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Rx,
    Tx,
}

impl Direction {
    /// The tag PostgreSQL's `dir` column stores.
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Rx => "rx",
            Direction::Tx => "tx",
        }
    }
}

/// One CAN or CAN FD frame.
///
/// `dlc` is the raw data length code, not the byte count: for FD they differ
/// above 8 (a DLC of 15 means 64 bytes). Both are kept because the GVRET wire
/// format transmits the code while the database stores the length, and
/// conflating them is the classic CAN FD bug.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanSample {
    /// Capture time in microseconds since the Unix epoch.
    pub ts_us: i64,
    /// Arbitration id: 11-bit, or 29-bit when `extended`.
    pub arb_id: u32,
    pub extended: bool,
    pub is_fd: bool,
    /// Data length code, 0–15.
    pub dlc: u8,
    pub data: Vec<u8>,
    pub bus: SourceId,
    pub dir: Direction,
}

/// CAN FD data length code → byte count. Codes 9–15 map to 12, 16, 20, 24,
/// 32, 48, 64; below that the code is the length.
pub const FD_DLC_LEN: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64];

/// Data length code → payload length in bytes.
pub fn dlc_to_len(dlc: u8, is_fd: bool) -> usize {
    let dlc = (dlc & 0x0F) as usize;
    if dlc <= 8 {
        dlc
    } else if is_fd {
        FD_DLC_LEN[dlc]
    } else {
        // Classic CAN caps at 8 bytes however the code is encoded.
        8
    }
}

/// Payload length → the smallest data length code that can carry it.
pub fn len_to_dlc(len: usize) -> u8 {
    if len <= 8 {
        return len as u8;
    }
    FD_DLC_LEN.iter().position(|&l| l >= len).unwrap_or(15) as u8
}

/// Anything a capture source can emit. Only `Can` exists today; the enum is
/// the seam Modbus register capture drops into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sample {
    Can(CanSample),
}

impl Sample {
    pub fn ts_us(&self) -> i64 {
        match self {
            Sample::Can(c) => c.ts_us,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd_dlc_round_trips_above_eight() {
        // The pairs that actually differ between code and length.
        for (dlc, len) in [
            (9u8, 12usize),
            (10, 16),
            (11, 20),
            (12, 24),
            (13, 32),
            (14, 48),
            (15, 64),
        ] {
            assert_eq!(dlc_to_len(dlc, true), len, "dlc {dlc}");
            assert_eq!(len_to_dlc(len), dlc, "len {len}");
        }
    }

    #[test]
    fn classic_can_never_exceeds_eight_bytes() {
        // A classic frame carrying a DLC above 8 is legal on the wire and
        // means 8 bytes; reading it as an FD length would over-read.
        for dlc in 9u8..=15 {
            assert_eq!(dlc_to_len(dlc, false), 8);
        }
    }

    #[test]
    fn len_to_dlc_rounds_up_to_the_next_code() {
        // 9 bytes does not exist as an FD length; it must pad to 12 (code 9).
        assert_eq!(len_to_dlc(9), 9);
        assert_eq!(len_to_dlc(13), 10);
        assert_eq!(len_to_dlc(0), 0);
        assert_eq!(len_to_dlc(8), 8);
    }

    #[test]
    fn direction_tags_match_the_database_column() {
        assert_eq!(Direction::Rx.as_str(), "rx");
        assert_eq!(Direction::Tx.as_str(), "tx");
    }
}
