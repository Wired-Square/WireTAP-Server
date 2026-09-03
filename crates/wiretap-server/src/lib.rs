//! WireTAP capture server.
//!
//! Reads CAN frames from a Linux host's SocketCAN interfaces, bridges them
//! live to GVRET clients (the WireTAP desktop app, SavvyCAN), and forwards
//! them to a gateway for archiving.
//!
//! The logic lives in the library and the binary is a thin wrapper, so the
//! protocol codecs are testable without opening a socket — which is what lets
//! the port be checked byte-for-byte against the Python it replaces.

pub mod gvret;
