//! WireTAP capture server.
//!
//! Reads CAN frames from a Linux host's SocketCAN interfaces, bridges them
//! live to GVRET clients (the WireTAP desktop app, SavvyCAN), and forwards
//! them to a gateway for archiving.
//!
//! The logic lives in the library and the binary is a thin wrapper, so the
//! pipeline and the configuration merge are testable without opening a socket
//! — which is what lets the port be checked against the Python it replaces.
//! The wire codecs live outside this crate, in `wiretap-protocol`: `gvret` for
//! the live bridge, `ingest` for the ingest listener and the forward link, and
//! `testpattern` for the test pattern.

/// `0.1.0 (g4bf526d44891)` — the package version and the commit `build.rs`
/// stamps in, reported by `--version` and by the journal's first line. See
/// `wiretap-build-id` for why the version alone is not enough.
pub const VERSION: &str = wiretap_build_id::build_version!();

pub mod archive;
pub mod cache;
pub mod catalogues;
pub mod cli;
pub mod console;
pub mod forward;
pub mod gvret;
pub mod ingest;
/// Wiring the server together. Only its CAN half is Linux-only.
pub mod pipeline;
pub mod settings;
pub mod source;
pub mod testpattern;
pub mod wire;
