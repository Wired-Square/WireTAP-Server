//! Types shared across the WireTAP server, the gateway, and the web interface:
//! what a capture produces, and how the server is configured.
//!
//! No runtime, no driver, no database client — a client needs the wire
//! contract without the capture stack behind it.
//!
//! These types are **this product's**. What crosses to the WireTAP desktop is
//! the wire format rather than the frame type, and that lives in
//! `wiretap-protocol` — the data length code table included, because a code on
//! the wire and a length in a column is a distinction both ends must share.

#[cfg(feature = "config")]
pub mod config;
pub mod sample;
pub mod secret;

#[cfg(feature = "config")]
pub use config::{parse_ifaces, FileConfig};
pub use sample::{CanSample, Direction, ModbusSample, Protocol, Sample, SourceId};
pub use secret::Secret;

/// The gateway's rule for a capture database name: a lowercase letter, then
/// lowercase letters, digits or `_`, at most 63. Here so the capture server
/// can refuse at `--check-config` what the gateway would refuse at HELLO.
pub fn valid_db_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::valid_db_name;

    #[test]
    fn db_name_validation() {
        assert!(valid_db_name("wiretap"));
        assert!(valid_db_name("vehicle_1"));
        assert!(!valid_db_name(""));
        assert!(!valid_db_name("1leading_digit"));
        assert!(!valid_db_name("Has-Caps"));
        assert!(!valid_db_name("name;drop table"));
        assert!(!valid_db_name(&"x".repeat(64)));
    }
}
