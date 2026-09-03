//! Types shared across the WireTAP server, the gateway, and the web interface:
//! what a capture produces, and how the server is configured.
//!
//! No runtime, no driver, no database client — a client needs the wire
//! contract without the capture stack behind it.

#[cfg(feature = "config")]
pub mod config;
pub mod sample;
pub mod secret;

#[cfg(feature = "config")]
pub use config::{parse_ifaces, FileConfig};
pub use sample::{dlc_to_len, len_to_dlc, payload_dlc, CanSample, Direction, SourceId};
pub use secret::Secret;
