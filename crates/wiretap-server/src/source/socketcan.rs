//! Reading frames from a Linux SocketCAN interface.
//!
//! Linux-only, so none of this runs in the test suite on a development
//! machine; CI exercises it against a `vcan` interface.
//!
//! Two things the Python hand-rolled are gone. It sniffed CAN FD by checking
//! whether a read returned 72 bytes; `CanAnyFrame` says so directly. And it
//! parsed `IFLA_CAN_BITTIMING` netlink attributes itself via pyroute2, which
//! was the last Python-only dependency on the capture path.

use std::io;

use socketcan::{tokio::CanFdSocket, CanAnyFrame, EmbeddedFrame, Frame, SocketOptions};
use wiretap_model::{CanSample, Direction, SourceId};

use super::{system_time_to_us, Bitrates};

/// One interface, read as CAN FD frames.
///
/// The FD socket also delivers classic frames, so one socket type covers both
/// and `accept_fd` only decides whether FD frames are *reported* — matching
/// the Python, where a non-FD configuration would not surface them.
pub struct CanReader {
    socket: CanFdSocket,
    bus: SourceId,
    dir: Direction,
    accept_fd: bool,
}

impl CanReader {
    /// Open `iface` and enable kernel receive timestamps.
    ///
    /// The timestamp is the kernel's software receive time, as the Python got
    /// via `SO_TIMESTAMP`. `socketcan` asks in nanoseconds where the Python
    /// asked in microseconds; same clock reading, truncated the same way.
    pub fn open(iface: &str, bus: SourceId, dir: Direction, accept_fd: bool) -> io::Result<Self> {
        let socket = CanFdSocket::open(iface)?;
        socket.set_recv_timestamp(true)?;
        Ok(Self {
            socket,
            bus,
            dir,
            accept_fd,
        })
    }

    /// Await the next frame worth recording.
    ///
    /// Remote-transmission frames are skipped: they carry no data, and
    /// archiving one as a zero-length frame would be noise. **This differs
    /// from the Python**, which passed them through — recorded in
    /// `docs/porting-notes.md`. Error frames never arrive at all, because
    /// neither implementation sets `CAN_RAW_ERR_FILTER` and the kernel
    /// delivers none by default; that arm is defensive, not policy.
    pub async fn recv(&self) -> io::Result<CanSample> {
        loop {
            let (frame, at) = self.socket.read_frame_with_timestamp().await?;
            let is_fd = match frame {
                CanAnyFrame::Normal(_) => false,
                CanAnyFrame::Fd(_) if self.accept_fd => true,
                _ => continue,
            };
            // `CanAnyFrame` implements both traits by delegating per variant,
            // and `raw_id` has already stripped the EFF/RTR/ERR flags.
            return Ok(CanSample {
                ts_us: system_time_to_us(at),
                arb_id: frame.raw_id(),
                extended: frame.is_extended(),
                is_fd,
                data: frame.data().to_vec(),
                bus: self.bus,
                dir: self.dir,
            });
        }
    }
}

/// Ask the kernel what an interface is configured for.
///
/// One netlink round trip: `details()` returns both timings from the message
/// `bit_rate()` and `data_bit_timing()` would each fetch separately, so the
/// two rates also come from one snapshot rather than two.
///
/// The fallback mirrors the Python, which matters more than it looks. A
/// missing nominal falls back *without* discarding a data rate that was read
/// successfully, and a nominal of zero — a CAN device that is up but was never
/// given a bitrate — is reported as zero rather than dressed up as 500 kbit/s.
pub fn detect_bitrates(iface: &str) -> Bitrates {
    // Two `let else` rather than a chain: `open` fails with an `Errno` and
    // `details` with a netlink error, so there is no common error type.
    let Ok(nl) = socketcan::nl::CanInterface::open(iface) else {
        return Bitrates::FALLBACK;
    };
    let Ok(details) = nl.details() else {
        return Bitrates::FALLBACK;
    };
    Bitrates {
        nominal: details
            .can
            .bit_timing
            .map_or(Bitrates::FALLBACK.nominal, |t| t.bitrate),
        data: details.can.data_bit_timing.map_or(0, |t| t.bitrate),
    }
}
