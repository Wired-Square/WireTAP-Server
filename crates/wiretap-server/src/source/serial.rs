//! A serial line, opened read-only and read as bytes arrive.
//!
//! Read-only is the whole of the passive guarantee: a descriptor opened with
//! `O_RDONLY` cannot be written, whatever a bug elsewhere might try, and the
//! packaged unit's `DeviceAllow` grants the daemon no more than that. Raw
//! termios, with software and hardware flow control cleared explicitly —
//! `cfmakeraw` leaves `IXOFF` as the port had it, and with it set the kernel
//! itself would put an XOFF on the wire. `VMIN = 1` so a read with nothing
//! there reports `WouldBlock` rather than zero: zero is the line gone.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;

use nix::fcntl::OFlag;
use nix::sys::termios::{self, BaudRate, ControlFlags, FlushArg, InputFlags, SetArg};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

use crate::settings::{Parity, SerialSettings};

pub struct SerialLine(AsyncFd<File>);

impl SerialLine {
    /// Open `path` read-only, raw, at the given rate and framing.
    pub fn open_readonly(path: &str, s: &SerialSettings) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags((OFlag::O_NOCTTY | OFlag::O_NONBLOCK).bits())
            .open(path)?;

        let mut t = termios::tcgetattr(&file)?;
        termios::cfmakeraw(&mut t);
        let speed = baud(s.baud).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("{} baud", s.baud))
        })?;
        termios::cfsetspeed(&mut t, speed)?;
        t.input_flags &= !(InputFlags::IXON | InputFlags::IXOFF | InputFlags::IXANY);
        t.control_flags |= ControlFlags::CLOCAL | ControlFlags::CREAD;
        t.control_flags &= !(ControlFlags::CSIZE
            | ControlFlags::PARENB
            | ControlFlags::PARODD
            | ControlFlags::CSTOPB
            | ControlFlags::CRTSCTS);
        t.control_flags |= match s.data_bits {
            5 => ControlFlags::CS5,
            6 => ControlFlags::CS6,
            7 => ControlFlags::CS7,
            _ => ControlFlags::CS8,
        };
        t.control_flags |= match s.parity {
            Parity::None => ControlFlags::empty(),
            Parity::Even => ControlFlags::PARENB,
            Parity::Odd => ControlFlags::PARENB | ControlFlags::PARODD,
        };
        if s.stop_bits == 2 {
            t.control_flags |= ControlFlags::CSTOPB;
        }
        termios::tcsetattr(&file, SetArg::TCSANOW, &t)?;
        // Whatever arrived before the line was configured is not this
        // capture's.
        termios::tcflush(&file, FlushArg::TCIFLUSH)?;

        Ok(Self(AsyncFd::new(file)?))
    }

    /// Wait for bytes and read what is there. Zero is the line gone.
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.0
            .async_io(Interest::READABLE, |mut file| file.read(buf))
            .await
    }
}

/// The kernel's constant for a rate. Every entry of
/// [`crate::settings::SUPPORTED_BAUDS`] has one; the test below holds them
/// together.
fn baud(rate: u32) -> Option<BaudRate> {
    Some(match rate {
        1200 => BaudRate::B1200,
        2400 => BaudRate::B2400,
        4800 => BaudRate::B4800,
        9600 => BaudRate::B9600,
        19200 => BaudRate::B19200,
        38400 => BaudRate::B38400,
        57600 => BaudRate::B57600,
        115200 => BaudRate::B115200,
        230400 => BaudRate::B230400,
        460800 => BaudRate::B460800,
        921600 => BaudRate::B921600,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SUPPORTED_BAUDS;

    #[test]
    fn every_supported_baud_has_a_kernel_constant() {
        for &rate in SUPPORTED_BAUDS {
            assert!(baud(rate).is_some(), "{rate}");
        }
        assert!(baud(9601).is_none());
    }
}
