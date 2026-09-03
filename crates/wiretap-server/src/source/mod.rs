//! Capture sources: the things that produce frames.
//!
//! This module is platform-independent on purpose. The socket code is
//! Linux-only and cannot be tested on a development Mac, so the arithmetic
//! that is actually easy to get wrong — mapping between a socket index and the
//! bus number a GVRET client sees — lives here, where it can be.

#[cfg(target_os = "linux")]
pub mod socketcan;

use std::time::{SystemTime, UNIX_EPOCH};

/// Bitrates reported for an interface: nominal, and the data rate for CAN FD
/// (zero when the interface is not FD-capable or the rate is unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bitrates {
    pub nominal: u32,
    pub data: u32,
}

impl Default for Bitrates {
    /// What the Python reported when it could not ask the kernel. Preserved
    /// because a GVRET client shows this number to a user, and 500 kbit/s is a
    /// better guess than zero.
    fn default() -> Self {
        Self {
            nominal: 500_000,
            data: 0,
        }
    }
}

/// The GVRET bus number for the `index`-th configured interface.
pub fn bus_for_index(index: usize, bus_offset: u8) -> u8 {
    (index as u8).saturating_add(bus_offset)
}

/// The interface index a GVRET client meant by `bus`, or `None` if it named a
/// bus this server does not have.
///
/// A GVRET client addresses transmits by bus number, so this is where an
/// off-by-one silently puts a frame on the wrong physical bus. The bounds
/// check is why the return is an `Option` rather than an index.
pub fn index_for_bus(bus: u8, bus_offset: u8, iface_count: usize) -> Option<usize> {
    let index = usize::from(bus.checked_sub(bus_offset)?);
    (index < iface_count).then_some(index)
}

/// A `SystemTime` from the kernel as microseconds since the Unix epoch.
///
/// Saturates rather than wrapping: a Raspberry Pi has no real-time clock and
/// boots in 1970, so a pre-epoch timestamp is a real thing to see before NTP
/// lands, and it should clamp rather than become a huge positive number.
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
        assert_eq!(bus_for_index(0, 0), 0);
        assert_eq!(bus_for_index(1, 0), 1);
        // bus_offset = 2 means can0 -> bus 2, can1 -> bus 3.
        assert_eq!(bus_for_index(0, 2), 2);
        assert_eq!(bus_for_index(1, 2), 3);
    }

    /// The mapping a transmit takes, across the offsets the config documents.
    /// Getting this wrong puts a frame on a bus the operator did not name.
    #[test]
    fn transmit_routing_round_trips_through_the_offset() {
        for offset in [0u8, 2, 7] {
            for index in 0..3usize {
                let bus = bus_for_index(index, offset);
                assert_eq!(
                    index_for_bus(bus, offset, 3),
                    Some(index),
                    "offset {offset}"
                );
            }
        }
    }

    #[test]
    fn a_bus_this_server_does_not_have_is_refused() {
        // Two interfaces at offset 0: buses 0 and 1 exist, 2 does not.
        assert_eq!(index_for_bus(0, 0, 2), Some(0));
        assert_eq!(index_for_bus(1, 0, 2), Some(1));
        assert_eq!(index_for_bus(2, 0, 2), None, "past the end");

        // Below the offset must not wrap into a valid index.
        assert_eq!(index_for_bus(1, 2, 2), None, "below the offset");
        assert_eq!(index_for_bus(0, 2, 2), None);
        assert_eq!(index_for_bus(2, 2, 2), Some(0));

        // And no interfaces means nothing is routable.
        assert_eq!(index_for_bus(0, 0, 0), None);
    }

    #[test]
    fn the_default_bitrate_is_the_pythons_fallback() {
        assert_eq!(
            Bitrates::default(),
            Bitrates {
                nominal: 500_000,
                data: 0
            }
        );
    }

    #[test]
    fn timestamps_convert_to_microseconds() {
        assert_eq!(system_time_to_us(UNIX_EPOCH), 0);
        assert_eq!(
            system_time_to_us(UNIX_EPOCH + Duration::from_micros(1_500_000)),
            1_500_000
        );
        // A clock that has not been set yet clamps instead of going negative.
        assert_eq!(system_time_to_us(UNIX_EPOCH - Duration::from_secs(1)), 0);
    }
}
