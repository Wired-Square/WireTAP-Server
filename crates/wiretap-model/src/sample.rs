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

/// What a sample came off the wire as: the `protocol` column's value, and
/// the tag a disk cache stores. The serde representation is that value, so a
/// misspelt query parameter is refused at deserialisation rather than
/// becoming a filter that matches nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Can,
    Modbus,
    Serial,
}

impl Protocol {
    /// The tag the archive stores. Bound to the serde representation by
    /// `protocol_tags_agree_with_serde`.
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Can => "can",
            Protocol::Modbus => "modbus",
            Protocol::Serial => "serial",
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Protocol {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "can" => Ok(Protocol::Can),
            "modbus" => Ok(Protocol::Modbus),
            "serial" => Ok(Protocol::Serial),
            _ => Err(()),
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

/// One Modbus RTU message, tapped off a serial line this server never
/// transmits on.
///
/// The whole message is kept, CRC included: on a line where most function
/// codes are a vendor's own, the bytes are the record and anything decoded
/// from them is a view. `unit` and `func` repeat the message's first two
/// bytes so a row can be keyed on them without parsing the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModbusSample {
    /// Capture time in microseconds since the Unix epoch.
    pub ts_us: i64,
    pub bus: SourceId,
    /// Slave address, or 0 for a broadcast.
    pub unit: u8,
    /// Function code, vendor codes included.
    pub func: u8,
    /// Whether the trailing CRC matched the body.
    pub crc_valid: bool,
    /// The message as it crossed the wire, CRC included.
    pub raw: Vec<u8>,
}

/// What any capture source produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sample {
    Can(CanSample),
    Modbus(ModbusSample),
}

impl Sample {
    pub fn ts_us(&self) -> i64 {
        match self {
            Sample::Can(c) => c.ts_us,
            Sample::Modbus(m) => m.ts_us,
        }
    }

    pub fn bus(&self) -> SourceId {
        match self {
            Sample::Can(c) => c.bus,
            Sample::Modbus(m) => m.bus,
        }
    }

    pub fn protocol(&self) -> Protocol {
        match self {
            Sample::Can(_) => Protocol::Can,
            Sample::Modbus(_) => Protocol::Modbus,
        }
    }
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

    /// The same three-way agreement as `Direction`'s, plus the refusal a
    /// query parameter relies on.
    #[test]
    fn protocol_tags_agree_with_serde() {
        for p in [Protocol::Can, Protocol::Modbus, Protocol::Serial] {
            assert_eq!(
                serde_json::to_string(&p).unwrap(),
                format!("\"{}\"", p.as_str())
            );
            assert_eq!(p.as_str().parse(), Ok(p));
            assert_eq!(p.to_string(), p.as_str());
        }
        assert!(serde_json::from_str::<Protocol>("\"modbsu\"").is_err());
        assert_eq!("CAN".parse::<Protocol>(), Err(()), "case is the column's");
    }

    #[test]
    fn a_sample_answers_for_whichever_kind_it_holds() {
        let can = Sample::Can(CanSample {
            ts_us: 1,
            arb_id: 0x123,
            extended: false,
            is_fd: false,
            data: vec![1],
            bus: SourceId(0),
            dir: Direction::Rx,
        });
        let modbus = Sample::Modbus(ModbusSample {
            ts_us: 2,
            bus: SourceId(1),
            unit: 1,
            func: 3,
            crc_valid: true,
            raw: vec![1, 3, 0, 0, 0, 1, 0x84, 0x0A],
        });
        assert_eq!(
            (can.ts_us(), can.bus(), can.protocol()),
            (1, SourceId(0), Protocol::Can)
        );
        assert_eq!(
            (modbus.ts_us(), modbus.bus(), modbus.protocol()),
            (2, SourceId(1), Protocol::Modbus)
        );
    }
}
