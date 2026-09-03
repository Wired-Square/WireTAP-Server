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

pub mod cli;
pub mod console;
pub mod gvret;
/// Wiring the capture loop together. Linux-only: it starts with a CAN socket.
#[cfg(target_os = "linux")]
pub mod pipeline;
pub mod settings;
pub mod source;
