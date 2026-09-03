//! Reading frames from a Linux SocketCAN interface.
//!
//! Linux-only, so nothing here runs in the test suite on a development
//! machine; the arithmetic worth testing lives in the parent module, and this
//! is kept thin enough to read. CI exercises it against a `vcan` interface.
//!
//! Two things the Python hand-rolled are gone. It sniffed CAN FD by checking
//! whether a read returned 72 bytes; `CanAnyFrame` says so directly. And it
//! parsed `IFLA_CAN_BITTIMING` netlink attributes itself, via pyroute2 — the
//! `socketcan` crate's netlink module answers that, which is what removes the
//! last Python-only dependency from the capture path.

use std::io;

use socketcan::{tokio::CanFdSocket, CanAnyFrame, EmbeddedFrame, Frame, SocketOptions};
use wiretap_model::{CanSample, Direction, SourceId};

use super::{system_time_to_us, Bitrates};

/// One interface, read as CAN FD frames.
///
/// The FD socket also delivers classic frames, so a single socket type covers
/// both and `can_fd` only decides whether FD frames are *accepted* — matching
/// the Python, where a non-FD configuration would not see them.
pub struct CanReader {
    socket: CanFdSocket,
    iface: String,
    bus: SourceId,
    dir: Direction,
    accept_fd: bool,
}

impl CanReader {
    /// Open `iface` and enable kernel receive timestamps.
    ///
    /// The timestamp is the kernel's software receive time, which is what the
    /// Python used via `SO_TIMESTAMP`. `socketcan` asks for nanoseconds where
    /// the Python asked for microseconds; both are the same clock reading and
    /// both are truncated to microseconds here.
    pub fn open(iface: &str, bus: u8, dir: Direction, accept_fd: bool) -> io::Result<Self> {
        let socket = CanFdSocket::open(iface)?;
        socket.set_recv_timestamp(true)?;
        Ok(Self {
            socket,
            iface: iface.to_string(),
            bus: SourceId(bus),
            dir,
            accept_fd,
        })
    }

    pub fn iface(&self) -> &str {
        &self.iface
    }

    pub fn bus(&self) -> SourceId {
        self.bus
    }

    /// Await the next frame worth reporting.
    ///
    /// Error and remote-transmission frames are skipped rather than returned:
    /// they carry no payload to archive, and the Python ignored them too. FD
    /// frames are skipped when the interface was not configured for FD.
    pub async fn recv(&self) -> io::Result<CanSample> {
        loop {
            let (frame, at) = self.socket.read_frame_with_timestamp().await?;
            let ts_us = system_time_to_us(at);
            match frame {
                CanAnyFrame::Normal(f) => return Ok(self.sample(ts_us, &f, false)),
                CanAnyFrame::Fd(f) if self.accept_fd => return Ok(self.sample(ts_us, &f, true)),
                // Remote, error, and FD-when-not-configured: nothing to record.
                _ => continue,
            }
        }
    }

    /// Generic over the frame kinds `CanAnyFrame` carries, which are distinct
    /// types. `raw_id` already strips the EFF/RTR/ERR flags for us.
    fn sample<F: Frame + EmbeddedFrame>(&self, ts_us: i64, f: &F, is_fd: bool) -> CanSample {
        CanSample {
            ts_us,
            arb_id: f.raw_id(),
            extended: f.is_extended(),
            is_fd,
            data: f.data().to_vec(),
            bus: self.bus,
            dir: self.dir,
        }
    }
}

/// Ask the kernel what an interface is configured for.
///
/// Every failure yields [`Bitrates::default`] — 500 kbit/s nominal, no data
/// rate — because a GVRET client displays this to a user and the Python did
/// the same rather than refusing to serve. An unreadable bitrate is not a
/// reason to fail a capture.
pub fn detect_bitrates(iface: &str) -> Bitrates {
    let Ok(nl) = socketcan::nl::CanInterface::open(iface) else {
        return Bitrates::default();
    };
    let nominal = match nl.bit_rate() {
        Ok(Some(r)) if r > 0 => r,
        _ => return Bitrates::default(),
    };
    let data = nl
        .data_bit_timing()
        .ok()
        .flatten()
        .map(|t| t.bitrate)
        .unwrap_or(0);
    Bitrates { nominal, data }
}
