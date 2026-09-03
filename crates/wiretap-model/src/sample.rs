//! What a capture source produces.

use serde::{Deserialize, Serialize};

/// Which interface a sample came from. `bus` is the GVRET-visible number,
/// which is the socket's index plus the configured `bus_offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceId(pub u8);

/// Frame direction. Captured frames are `Rx`; `Tx` is a frame this server
/// transmitted for a GVRET client, so an archive can tell them apart from bus
/// traffic. The serde representation is the tag the database stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Rx,
    Tx,
}

impl Direction {
    /// The tag the archive stores. Bound to the serde representation by
    /// `direction_tags_agree_with_serde`, so the two cannot drift.
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Rx => "rx",
            Direction::Tx => "tx",
        }
    }
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Direction {
    type Err = ();

    /// Case-insensitive on purpose: the Python accepted any `--default-dir`
    /// string, so `TX` has to keep working.
    ///
    /// Compared rather than lowercased, so this allocates nothing: it is called
    /// once per row when a disk cache is drained, which after a long outage is
    /// millions of rows.
    fn from_str(s: &str) -> Result<Self, ()> {
        if s.eq_ignore_ascii_case("rx") {
            Ok(Direction::Rx)
        } else if s.eq_ignore_ascii_case("tx") {
            Ok(Direction::Tx)
        } else {
            Err(())
        }
    }
}

/// One CAN or CAN FD frame.
///
/// The data length code is **not** stored: it is derivable from `data.len()`
/// and `is_fd` via [`payload_dlc`], and carrying both invites the two to
/// disagree. The one case that is not derivable — a classic frame declaring a
/// code of 9–15 while carrying 8 bytes — has no producer or consumer here, and
/// the Python this replaces did not preserve it either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanSample {
    /// Capture time in microseconds since the Unix epoch.
    pub ts_us: i64,
    /// Arbitration id: 11-bit, or 29-bit when `extended`.
    pub arb_id: u32,
    pub extended: bool,
    pub is_fd: bool,
    pub data: Vec<u8>,
    pub bus: SourceId,
    pub dir: Direction,
}

/// CAN FD data length code → byte count. Below 9 the code *is* the length.
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

/// The code for a payload of `len` bytes, clamped to what the frame type can
/// actually carry.
///
/// This is the CAN FD trap in one place: the wire carries a *code*, the
/// database stores a *length*, and above 8 bytes they differ. It was written
/// out separately at three call sites across two crates before this existed.
pub fn payload_dlc(len: usize, is_fd: bool) -> u8 {
    len_to_dlc(len.min(if is_fd { 64 } else { 8 }))
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
    fn payload_dlc_clamps_by_frame_type() {
        assert_eq!(payload_dlc(20, false), 8, "classic clamps to 8");
        assert_eq!(payload_dlc(20, true), 11, "fd keeps 20 as code 11");
        assert_eq!(payload_dlc(100, true), 15, "fd clamps to 64, code 15");
        assert_eq!(payload_dlc(3, false), 3);
        assert_eq!(payload_dlc(3, true), 3);
    }

    /// The code a payload is given must round-trip back to a length that can
    /// hold it — the property the three hand-written copies had to preserve
    /// individually.
    #[test]
    fn payload_dlc_round_trips_through_dlc_to_len() {
        for is_fd in [false, true] {
            for len in 0..=70usize {
                let dlc = payload_dlc(len, is_fd);
                let back = dlc_to_len(dlc, is_fd);
                let cap = if is_fd { 64 } else { 8 };
                assert!(
                    back >= len.min(cap),
                    "len {len} fd {is_fd}: {back} < {}",
                    len.min(cap)
                );
            }
        }
    }

    /// One tag, three representations — serde, `as_str`, and `FromStr`. They
    /// are asserted against each other rather than against literals, so adding
    /// a variant cannot leave one behind.
    #[test]
    fn direction_tags_agree_with_serde() {
        for d in [Direction::Rx, Direction::Tx] {
            assert_eq!(
                serde_json::to_string(&d).unwrap(),
                format!("\"{}\"", d.as_str())
            );
            assert_eq!(d.as_str().parse(), Ok(d));
            assert_eq!(d.to_string(), d.as_str());
        }
        assert_eq!(
            "TX".parse(),
            Ok(Direction::Tx),
            "case-insensitive, as the Python was"
        );
        assert_eq!("sideways".parse::<Direction>(), Err(()));
    }
}
