//! Capture sources: the things that produce frames.
//!
//! The socket code is Linux-only, so the bus arithmetic lives here where it
//! can be tested on any machine.

#[cfg(target_os = "linux")]
pub mod socketcan;

use std::time::{SystemTime, UNIX_EPOCH};

use wiretap_model::SourceId;

/// Bitrates reported for an interface: nominal, and the data rate for CAN FD.
///
/// `data` is zero when the interface is not FD-capable. Nothing on the wire
/// carries it — GVRET's `F1 06` has no field for a data rate — so it exists
/// for the startup log, as it did in the Python.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bitrates {
    pub nominal: u32,
    pub data: u32,
}

impl Bitrates {
    /// What the Python reported when it could not ask the kernel. A GVRET
    /// client shows this to a user, and 500 kbit/s is a better guess than
    /// zero.
    ///
    /// Deliberately a named constant and not a `Default` impl: at a call site
    /// `Bitrates::default()` would read as "zeroed", and any struct later
    /// deriving `Default` around this type would silently acquire a
    /// 500 kbit/s claim nobody made.
    pub const FALLBACK: Self = Self {
        nominal: 500_000,
        data: 0,
    };
}

/// A frame a GVRET client asked this server to put on a bus.
///
/// Carries the client's bus number rather than an interface index: resolving
/// one to the other is [`index_for_bus`]'s job and belongs with the sockets,
/// not with the protocol that named the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transmit {
    pub bus: SourceId,
    pub arb_id: u32,
    pub extended: bool,
    pub data: Vec<u8>,
}

/// The GVRET bus number for the `index`-th configured interface.
///
/// Returns `None` for an index past 255, which no configuration can reach —
/// stated rather than truncated, so the sibling below is its exact inverse.
pub fn bus_for_index(index: usize, bus_offset: u8) -> Option<SourceId> {
    u8::try_from(index)
        .ok()?
        .checked_add(bus_offset)
        .map(SourceId)
}

/// How many buses to advertise for `iface_count` interfaces: the highest bus
/// number plus one, so an offset shifts the *count* as well as the numbers.
///
/// That is the Python's `bus_offset + len(can_socks)`, and it is not academic:
/// the count it produced was used to index a list holding one speed per
/// interface, so an offset made the original advertise buses it had no speed
/// for and crash the client that asked. See `docs/porting-notes.md`.
pub fn bus_count(iface_count: usize, bus_offset: u8) -> u8 {
    // Saturating where the two mappings above refuse, because a count is what
    // a client is told rather than what a frame is routed by. Unreachable
    // either way: it needs 256 interfaces.
    bus_for_index(iface_count, bus_offset).map_or(u8::MAX, |b| b.0)
}

/// The interface index a GVRET client meant by `bus`, or `None` if it named a
/// bus this server does not have.
///
/// A GVRET client addresses transmits by an arbitrary bus byte, so this is
/// where an off-by-one silently puts a frame on the wrong physical bus. Taking
/// [`SourceId`] rather than `u8` is what stops an interface index being passed
/// here by mistake.
pub fn index_for_bus(bus: SourceId, bus_offset: u8, iface_count: usize) -> Option<usize> {
    let index = usize::from(bus.0.checked_sub(bus_offset)?);
    (index < iface_count).then_some(index)
}

/// A `SystemTime` from the kernel as microseconds since the Unix epoch.
///
/// The clamp is for arbitrary inputs, not for a real hazard on this path:
/// `socketcan` already clamps the kernel's `timespec` at zero, so a frame
/// timestamp cannot be pre-epoch. A Pi with an unset clock reads *at* the
/// epoch, not before it.
pub fn system_time_to_us(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bus_numbers_follow_the_offset() {
        assert_eq!(bus_for_index(0, 0), Some(SourceId(0)));
        assert_eq!(bus_for_index(1, 0), Some(SourceId(1)));
        // bus_offset = 2 means can0 -> bus 2, can1 -> bus 3.
        assert_eq!(bus_for_index(0, 2), Some(SourceId(2)));
        assert_eq!(bus_for_index(1, 2), Some(SourceId(3)));
        // Unreachable by configuration, but stated rather than wrapped.
        assert_eq!(bus_for_index(200, 200), None);
        assert_eq!(bus_for_index(300, 0), None);
    }

    /// The mapping a transmit takes, across the offsets the config documents.
    /// Getting this wrong puts a frame on a bus the operator did not name.
    #[test]
    fn transmit_routing_round_trips_through_the_offset() {
        for offset in [0u8, 2, 7] {
            for index in 0..3usize {
                let bus = bus_for_index(index, offset).expect("representable");
                assert_eq!(
                    index_for_bus(bus, offset, 3),
                    Some(index),
                    "offset {offset}"
                );
            }
        }
    }

    #[test]
    fn the_advertised_bus_count_includes_the_offset() {
        assert_eq!(bus_count(2, 0), 2);
        assert_eq!(bus_count(0, 0), 0);
        // The shape that crashed the Python: three buses advertised, one
        // interface behind them.
        assert_eq!(bus_count(1, 2), 3);
        assert_eq!(bus_count(300, 0), u8::MAX, "saturated, not wrapped");
    }

    #[test]
    fn a_bus_this_server_does_not_have_is_refused() {
        // Two interfaces at offset 0: buses 0 and 1 exist, 2 does not.
        assert_eq!(index_for_bus(SourceId(0), 0, 2), Some(0));
        assert_eq!(index_for_bus(SourceId(1), 0, 2), Some(1));
        assert_eq!(index_for_bus(SourceId(2), 0, 2), None, "past the end");

        // Below the offset must not wrap into a valid index.
        assert_eq!(index_for_bus(SourceId(1), 2, 2), None, "below the offset");
        assert_eq!(index_for_bus(SourceId(0), 2, 2), None);

        // And no interfaces means nothing is routable.
        assert_eq!(index_for_bus(SourceId(0), 0, 0), None);
    }

    #[test]
    fn timestamps_convert_to_microseconds() {
        assert_eq!(system_time_to_us(UNIX_EPOCH), 0);
        assert_eq!(
            system_time_to_us(UNIX_EPOCH + Duration::from_micros(1_500_000)),
            1_500_000
        );
        assert_eq!(
            system_time_to_us(UNIX_EPOCH - Duration::from_secs(1)),
            0,
            "clamped"
        );
    }
}
