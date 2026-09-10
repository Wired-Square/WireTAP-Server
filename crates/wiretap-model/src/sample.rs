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
/// and `is_fd` via `wiretap_protocol::payload_dlc`, and carrying both invites
/// the two to disagree. The one case that is not derivable — a classic frame
/// declaring a code of 9–15 while carrying 8 bytes — has no producer or
/// consumer here, and the Python this replaces did not preserve it either.
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

#[cfg(test)]
mod tests {
    use super::*;

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
