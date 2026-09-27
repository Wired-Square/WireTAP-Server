//! A Linux SocketCAN interface, through `wiretap-io`: the socket, its kernel
//! stamps and its reopen by name are the library's, and what this server
//! archives and reports is decided here.

use std::io;

use wiretap_io::can::{
    socketcan::{self, SocketCanOptions},
    CanError, CanOptions, CanRead, CanTask,
};
use wiretap_model::{CanSample, Direction, SourceId};

use super::{system_time_to_us, Bitrates};

/// Open `interface` now, so a missing one is the caller's error, and read it
/// until the task is dropped.
///
/// Without `fd`, FD frames are dropped and FD sends refused. `listen_only`
/// refuses every send.
pub async fn open(interface: &str, fd: bool, listen_only: bool) -> io::Result<CanTask> {
    let mut options = CanOptions::default();
    options.listen_only = listen_only;
    let sc = SocketCanOptions {
        interface: interface.to_owned(),
        fd,
    };
    socketcan::open(sc, options).await.map_err(|e| match e {
        CanError::Open { source, .. } => source,
        other => io::Error::other(other),
    })
}

/// A read as the archive stores it.
///
/// Remote-transmission frames are `None`: they carry no data, and archiving
/// one as a zero-length frame would be noise. **This differs from the
/// Python**, which passed them through.
pub fn sample(read: CanRead, bus: SourceId, dir: Direction) -> Option<CanSample> {
    let frame = read.frame;
    (!frame.rtr).then(|| CanSample {
        ts_us: system_time_to_us(read.at),
        arb_id: frame.arb_id,
        extended: frame.extended,
        is_fd: frame.fd,
        data: frame.data,
        bus,
        dir,
    })
}

/// Ask the kernel what an interface is configured for, falling back as
/// [`Bitrates::reported`] does, and to [`Bitrates::FALLBACK`] when netlink
/// can't answer at all.
pub fn bitrates(interface: &str) -> Bitrates {
    socketcan::bitrates(interface).map_or(Bitrates::FALLBACK, |r| {
        Bitrates::reported(r.nominal, r.data)
    })
}

/// A lost interface as the journal names it: down and gone are different
/// fixes.
pub fn loss(error: &CanError) -> String {
    match error {
        CanError::Closed => "interface is gone; reopening it by name".to_owned(),
        CanError::Read(e) if e.kind() == io::ErrorKind::NetworkDown => {
            "interface is down; capture resumes once it is up".to_owned()
        }
        other => other.to_string(),
    }
}
