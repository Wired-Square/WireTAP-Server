//! The GVRET TCP server: the live bridge the WireTAP desktop app and SavvyCAN
//! connect to.

pub mod codec;

pub use wiretap_model::sample::len_to_dlc;
