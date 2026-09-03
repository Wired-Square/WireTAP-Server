//! Types shared across the WireTAP server, its web interface and any future
//! CLI: what a capture produces, and how the server is configured.
//!
//! Nothing here pulls in a runtime, a driver or a database client. That is the
//! point — a terminal or web client needs the wire contract without the
//! capture stack behind it.

pub mod config;
pub mod sample;

pub use config::{parse_ifaces, unknown_keys, FileConfig};
pub use sample::{dlc_to_len, len_to_dlc, CanSample, Direction, Sample, SourceId};
