//! Shared application state for the HTTP layer and ingest listener.

use crate::catalogs::Catalogs;
use crate::db::Databases;
use crate::ingest::Sessions;
use crate::keys::KeyStore;
use crate::logbuf::LogBuffer;
use crate::secrets::SecretFiles;

pub struct AppState {
    pub dbs: Databases,
    pub keys: KeyStore,
    pub sessions: Sessions,
    pub logs: LogBuffer,
    pub catalogs: Catalogs,
    pub secret_files: SecretFiles,
    /// Held for the whole of a rotation, so two never interleave.
    pub rotation: tokio::sync::Mutex<()>,
}
