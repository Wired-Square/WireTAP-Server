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
pub use sample::{CanSample, Direction, SourceId};
pub use secret::Secret;
