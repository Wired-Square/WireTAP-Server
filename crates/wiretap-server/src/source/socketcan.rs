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

use socketcan::{
    tokio::CanFdSocket, CanAnyFrame, CanDataFrame, EmbeddedFrame, ExtendedId, Frame, Id,
    SocketOptions, StandardId,
};
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

    /// Put a frame on this interface's bus, for a GVRET client that asked.
    ///
    /// Classic frames only, because that is all a client can ask for: `F1 00`
    /// has no FD flag, and the Python's `_tx_can` was only ever called without
    /// one, so it packed a 16-byte `can_frame` even when the socket was in FD
    /// mode. The payload is already clamped to 8 bytes by the decoder; the
    /// clamp here is what makes that a local guarantee rather than a remote
    /// one.
    pub async fn transmit(&self, arb_id: u32, extended: bool, data: &[u8]) -> io::Result<()> {
        let invalid = |what| io::Error::new(io::ErrorKind::InvalidInput, what);
        let id: Id = if extended {
            ExtendedId::new(arb_id)
                .ok_or_else(|| invalid("extended id too large"))?
                .into()
        } else {
            u16::try_from(arb_id)
                .ok()
                .and_then(StandardId::new)
                .ok_or_else(|| invalid("standard id too large"))?
                .into()
        };
        let frame = CanDataFrame::new(id, &data[..data.len().min(8)])
            .ok_or_else(|| invalid("payload too long"))?;
        self.socket.write_frame(&frame).await
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
