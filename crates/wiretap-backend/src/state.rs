//! Shared application state for the HTTP layer and ingest listener.

use crate::db::Databases;
use crate::ingest::Sessions;
use crate::keys::KeyStore;
use crate::logbuf::LogBuffer;

pub struct AppState {
    pub dbs: Databases,
    pub keys: KeyStore,
    pub sessions: Sessions,
    pub logs: LogBuffer,
}
