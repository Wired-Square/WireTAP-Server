//! WireTAP capture server.
//!
//! Reads CAN frames from a Linux host's SocketCAN interfaces, bridges them
//! live to GVRET clients (the WireTAP desktop app, SavvyCAN), and forwards
//! them to a gateway for archiving.
//!
//! The logic lives in the library and the binary is a thin wrapper, so the
//! protocol codecs and the configuration merge are testable without opening a
//! socket — which is what lets the port be checked against the Python it
//! replaces.

/// `0.1.0 (g4bf526d4489)` — the package version and the commit `build.rs`
/// stamps in, reported by `--version` and by the journal's first line. See
/// `build.rs` for why the version alone is not enough.
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("WIRETAP_BUILD_ID"),
    ")"
);

pub mod archive;
pub mod cache;
pub mod cli;
pub mod console;
pub mod forward;
pub mod gvret;
pub mod ingest;
/// Wiring the server together. Only its CAN half is Linux-only.
pub mod pipeline;
pub mod settings;
pub mod source;
