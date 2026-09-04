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
