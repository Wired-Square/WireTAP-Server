//! What the running daemon can say about itself over the control socket.

use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use crate::archive::Counters;
use crate::settings::Settings;

/// Shared by everything that reports, and read only by the control socket.
pub struct Live {
    pub started: Instant,
    /// By interface, in [`Settings::devices`] order.
    pub devices: Vec<(String, Arc<DeviceStatus>)>,
    /// Each pipeline's database and its counters, once the archives start.
    pub archives: OnceLock<Vec<(String, Arc<Counters>)>>,
    pub gvret_clients: OnceLock<Arc<AtomicUsize>>,
    pub ingest_clients: OnceLock<Arc<AtomicUsize>>,
}

impl Live {
    pub fn new(settings: &Settings) -> Self {
        Self {
            started: Instant::now(),
            devices: settings
                .devices
                .iter()
                .map(|d| (d.interface.clone(), Arc::default()))
                .collect(),
            archives: OnceLock::new(),
            gvret_clients: OnceLock::new(),
            ingest_clients: OnceLock::new(),
        }
    }

    /// The status of the device on `interface`, which is one of the settings'.
    pub fn device(&self, interface: &str) -> Arc<DeviceStatus> {
        let (_, status) = self
            .devices
            .iter()
            .find(|(i, _)| i == interface)
            .expect("a device these settings name");
        Arc::clone(status)
    }
}

/// One device's state and how many frames it has read. Relaxed throughout:
/// reported, never decided on.
#[derive(Debug, Default)]
pub struct DeviceStatus {
    state: AtomicU8,
    frames: AtomicU64,
}

const STARTING: u8 = 0;
const UP: u8 = 1;
const WAITING: u8 = 2;
const ERROR: u8 = 3;

impl DeviceStatus {
    pub fn up(&self) {
        self.state.store(UP, Ordering::Relaxed);
    }

    /// Lost after it had opened, or never yet found.
    pub fn lost(&self, opened: bool) {
        self.state
            .store(if opened { ERROR } else { WAITING }, Ordering::Relaxed);
    }

    pub fn read(&self, frames: u64) {
        self.frames.fetch_add(frames, Ordering::Relaxed);
    }

    pub fn state(&self) -> &'static str {
        match self.state.load(Ordering::Relaxed) {
            STARTING => "starting",
            UP => "up",
            WAITING => "waiting",
            _ => "error",
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
}

/// A client count that a connection holds one of while it lasts.
pub struct Connected(Arc<AtomicUsize>);

impl Connected {
    pub fn new(clients: &Arc<AtomicUsize>) -> Self {
        clients.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(clients))
    }
}

impl Drop for Connected {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
