//! A serial line opened through `wiretap-io` and drained into an [`RtuTap`], a
//! [`RawTap`], or both.

use std::io;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{error, info, warn};
use wiretap_catalog::LineSettings;
use wiretap_io::serial::{self, Access, SerialError, SerialEvent, SerialOptions, SerialTask};
use wiretap_model::{Sample, SourceId};

use super::modbus::RtuTap;
use super::raw::RawTap;
use crate::catalogues::Rules;
use crate::settings::SerialSettings;

/// A line's framer, and the catalogue rules it follows.
pub struct Framed {
    tap: RtuTap,
    rules: watch::Receiver<Rules>,
    current: Rules,
}

impl Framed {
    pub fn new(bus: SourceId, line: LineSettings, mut rules: watch::Receiver<Rules>) -> Self {
        let current = rules.borrow_and_update().clone();
        Self {
            tap: RtuTap::new(bus, line, current.catalogue()),
            rules,
            current,
        }
    }

    /// Rebuilt with the line's latest rules, if they changed.
    fn follow(&mut self, interface: &str) {
        if !self.rules.has_changed().unwrap_or(false) {
            return;
        }
        let new = self.rules.borrow_and_update().clone();
        info!(
            "{interface}: catalogue {} → {}",
            self.current.source(),
            new.source()
        );
        self.tap.reframe(new.catalogue());
        self.current = new;
    }
}

pub fn open(path: &str, line: &SerialSettings) -> io::Result<SerialTask> {
    let options = SerialOptions {
        // `O_RDONLY` with flow control cleared, so nothing here can reach the
        // wire; exclusive, because a second reader would split the stream.
        access: Access::ReadOnly,
        exclusive: true,
        read_buffer: 4096,
        reopen: Some(Duration::from_secs(1)),
        wait_for_device: true,
        ..SerialOptions::default()
    };
    serial::open(path, line.line, options).map_err(|e| match e {
        SerialError::Open { source, .. } => source,
        refused => io::Error::new(io::ErrorKind::InvalidInput, refused),
    })
}

/// Hand every read to the taps for as long as the task runs. A loss, or a line
/// missing at startup, is logged once and its end once.
pub async fn drain(
    interface: String,
    mut task: SerialTask,
    mut framed: Option<Framed>,
    mut raw: Option<RawTap>,
    mut publish: impl FnMut(Sample),
) {
    let mut opened = false;
    let mut lost = false;
    while let Some(event) = task.next_event().await {
        match event {
            SerialEvent::Connected => {
                if let Some(raw) = &mut raw {
                    raw.reset();
                }
                if std::mem::take(&mut lost) {
                    info!("{interface}: {}", if opened { "reopened" } else { "found" });
                }
                opened = true;
            }
            SerialEvent::Read { bytes, at } => {
                if let Some(raw) = &mut raw {
                    raw.push(&bytes, at)
                        .into_iter()
                        .map(Sample::Serial)
                        .for_each(&mut publish);
                }
                if let Some(framed) = &mut framed {
                    framed.follow(&interface);
                    framed
                        .tap
                        .push(&bytes, at)
                        .into_iter()
                        .map(Sample::Modbus)
                        .for_each(&mut publish);
                }
            }
            SerialEvent::Disconnected {
                error, consecutive, ..
            } => {
                match consecutive {
                    1 if opened => error!("{interface}: {error}; reopening"),
                    1 => warn!("waiting for {interface}: {error}"),
                    _ => {}
                }
                if let Some(framed) = &mut framed {
                    framed.tap.reset();
                }
                lost = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::ptr;

    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use wiretap_catalog::{LineSettings, Parity};
    use wiretap_model::{ModbusSample, SerialSample, SourceId};

    use super::*;
    use crate::settings::Framing;

    const LINE: SerialSettings = SerialSettings {
        line: LineSettings {
            baud: 9600,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: 1,
        },
        framing: Some(Framing::ModbusRtu),
        catalogue: None,
        raw_database: None,
    };

    struct Pty {
        master: File,
        slave: String,
    }

    impl Pty {
        /// The slave closed, so only the code under test holds it open.
        fn new() -> Self {
            let (mut master, mut slave) = (0, 0);
            let mut name = [0; 128];
            // SAFETY: two out-parameters for descriptors; no name, termios or
            // window size is passed in.
            let opened = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            assert_eq!(opened, 0, "openpty: {}", io::Error::last_os_error());
            // SAFETY: openpty returned both descriptors to us alone.
            let (master, slave) =
                unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
            // SAFETY: `name` is as long as the length passed.
            let named =
                unsafe { libc::ttyname_r(slave.as_raw_fd(), name.as_mut_ptr(), name.len()) };
            assert_eq!(named, 0, "ttyname_r");
            // SAFETY: ttyname_r wrote a NUL-terminated path into `name`.
            let slave = unsafe { CStr::from_ptr(name.as_ptr()) };
            Self {
                master: master.into(),
                slave: slave.to_str().expect("utf-8 path").to_owned(),
            }
        }
    }

    const REQUEST: [u8; 8] = [0x01, 0x03, 0x00, 0x00, 0x00, 0x01, 0x84, 0x0A];

    /// The server's path, from the open to what the taps publish.
    fn taps(path: &str, bus: SourceId, framed: bool, raw: bool) -> mpsc::UnboundedReceiver<Sample> {
        let rules = framed.then(|| watch::channel(Rules::None).1);
        taps_following(path, bus, rules, raw)
    }

    fn taps_following(
        path: &str,
        bus: SourceId,
        rules: Option<watch::Receiver<Rules>>,
        raw: bool,
    ) -> mpsc::UnboundedReceiver<Sample> {
        let task = open(path, &LINE).expect("open");
        let (samples, received) = mpsc::unbounded_channel();
        tokio::spawn(drain(
            path.to_owned(),
            task,
            rules.map(|rules| Framed::new(bus, LINE.line, rules)),
            raw.then(|| RawTap::new(bus, LINE.line)),
            move |s| samples.send(s).expect("the test is listening"),
        ));
        received
    }

    fn tap(path: &str, bus: SourceId) -> mpsc::UnboundedReceiver<Sample> {
        taps(path, bus, true, false)
    }

    async fn next_sample(received: &mut mpsc::UnboundedReceiver<Sample>) -> Sample {
        timeout(Duration::from_secs(1), received.recv())
            .await
            .expect("no sample")
            .expect("the tap ended")
    }

    async fn next(received: &mut mpsc::UnboundedReceiver<Sample>) -> ModbusSample {
        match next_sample(received).await {
            Sample::Modbus(m) => m,
            other => panic!("a Modbus message was expected, not {other:?}"),
        }
    }

    async fn next_raw(received: &mut mpsc::UnboundedReceiver<Sample>) -> SerialSample {
        match next_sample(received).await {
            Sample::Serial(r) => r,
            other => panic!("a raw chunk was expected, not {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_raw_tap_publishes_the_reads_and_counts_them_from_each_open() {
        let link = std::env::temp_dir().join(format!("wiretap-server-raw-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        let mut first = Pty::new();
        let mut second = Pty::new();
        std::os::unix::fs::symlink(&first.slave, &link).expect("symlink");
        let mut received = taps(link.to_str().expect("utf-8 path"), SourceId(5), false, true);

        // A byte at a time, so no write can arrive as two reads.
        first.master.write_all(&[0xA5]).expect("write");
        let r = next_raw(&mut received).await;
        assert_eq!((r.bus, r.seq, r.data), (SourceId(5), 0, vec![0xA5]));
        first.master.write_all(&[0x5A]).expect("write");
        assert_eq!(next_raw(&mut received).await.seq, 1);

        std::fs::remove_file(&link).expect("unlink");
        drop(first);
        std::os::unix::fs::symlink(&second.slave, &link).expect("symlink");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        second.master.write_all(&[0xA5]).expect("write");
        assert_eq!(
            next_raw(&mut received).await.seq,
            0,
            "counted again from the reopen"
        );
        let _ = std::fs::remove_file(&link);
    }

    #[tokio::test]
    async fn a_line_captured_both_ways_publishes_the_read_and_the_message() {
        let mut pty = Pty::new();
        let mut received = taps(&pty.slave, SourceId(1), true, true);
        pty.master.write_all(&REQUEST).expect("write");
        let mut raw = Vec::new();
        let m = loop {
            match next_sample(&mut received).await {
                Sample::Serial(r) => raw.push(r),
                Sample::Modbus(m) => break m,
                other => panic!("{other:?}"),
            }
        };
        let bytes: Vec<u8> = raw.iter().flat_map(|r| r.data.clone()).collect();
        assert_eq!((bytes, m.raw), (REQUEST.to_vec(), REQUEST.to_vec()));
        assert_eq!(
            raw.last().map(|r| r.ts_us),
            Some(m.ts_us),
            "the read with the last byte stamps both"
        );
    }

    #[tokio::test]
    async fn a_request_and_its_response_on_the_line_become_two_samples_on_the_bus() {
        let mut pty = Pty::new();
        let mut received = tap(&pty.slave, SourceId(3));

        let response = [0x01, 0x03, 0x02, 0x00, 0x2A, 0x39, 0x9B];
        pty.master
            .write_all(&[&REQUEST[..], &response[..]].concat())
            .expect("write");

        for raw in [&REQUEST[..], &response[..]] {
            let m = next(&mut received).await;
            assert_eq!((m.unit, m.func, m.bus), (1, 3, SourceId(3)));
            assert_eq!(m.raw, raw);
            assert!(m.crc_valid);
        }
    }

    /// A dispatch whose first 18 bytes pass CRC too: the search frames 18,
    /// and the example catalogue's declared length 19.
    fn dispatch() -> Vec<u8> {
        let body = [
            0x00, 0x60, 0x00, 0x00, 0x00, 0x05, 0x0A, 0x00, 0x04, 0x01, 0xBB, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xE5,
        ];
        let crc = wiretap_checksum::algorithms::crc16_modbus_checksum(&body);
        [&body[..], &crc.to_le_bytes()].concat()
    }

    #[tokio::test]
    async fn a_new_catalogue_frames_the_reads_after_it() {
        let mut pty = Pty::new();
        let (rules, following) = watch::channel(Rules::None);
        let mut received = taps_following(&pty.slave, SourceId(0), Some(following), false);

        pty.master.write_all(&dispatch()).expect("write");
        assert_eq!(next(&mut received).await.raw.len(), 18, "the old rules");
        // The 19th byte read too, so it is the old tap's to drop.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let example = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/examples/sungrow-rs485.catalog.toml"
        );
        let catalogue = crate::settings::LineCatalogue::read(example).expect("the example");
        rules
            .send(Rules::Etc(catalogue))
            .expect("the drain follows");
        pty.master.write_all(&dispatch()).expect("write");
        assert_eq!(next(&mut received).await.raw, dispatch(), "the new rules");
    }

    /// macOS has no termios constant above 230 400 baud.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn every_supported_baud_reaches_the_open() {
        for &baud in crate::settings::SUPPORTED_BAUDS {
            let line = SerialSettings {
                line: LineSettings { baud, ..LINE.line },
                ..LINE
            };
            if let Err(refused) = open("/nonexistent/wiretap-tap", &line) {
                panic!("{baud}: {refused}");
            }
        }
    }

    /// A pty's slave ignores `TIOCEXCL` on macOS, and root bypasses it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_second_open_of_a_tapped_line_is_refused() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let pty = Pty::new();
        let _tapped = open(&pty.slave, &LINE).expect("open");
        let refused = open(&pty.slave, &LINE)
            .err()
            .expect("a second open got past TIOCEXCL");
        assert_eq!(refused.raw_os_error(), Some(libc::EBUSY), "{refused}");
    }

    #[tokio::test]
    async fn a_line_missing_at_startup_is_waited_for() {
        let link =
            std::env::temp_dir().join(format!("wiretap-server-absent-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        let mut received = tap(link.to_str().expect("utf-8 path"), SourceId(0));

        let mut pty = Pty::new();
        std::os::unix::fs::symlink(&pty.slave, &link).expect("symlink");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        pty.master.write_all(&REQUEST).expect("write");
        assert_eq!(next(&mut received).await.raw, REQUEST);
        let _ = std::fs::remove_file(&link);
    }

    /// Unplugged mid-request and replugged as a fresh tty: the half request
    /// is forgotten, so the next one frames at once.
    #[tokio::test]
    async fn a_lost_line_is_reopened_and_the_half_message_forgotten() {
        let link = std::env::temp_dir().join(format!("wiretap-server-tap-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        let mut first = Pty::new();
        let mut second = Pty::new();
        std::os::unix::fs::symlink(&first.slave, &link).expect("symlink");
        let mut received = tap(link.to_str().expect("utf-8 path"), SourceId(0));

        first.master.write_all(&REQUEST[..5]).expect("write");
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::remove_file(&link).expect("unlink");
        drop(first);
        std::os::unix::fs::symlink(&second.slave, &link).expect("symlink");

        // Past the 1 s reopen; were the half request still held, this one
        // would be too.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        second.master.write_all(&REQUEST).expect("write");
        assert_eq!(next(&mut received).await.raw, REQUEST);
        let _ = std::fs::remove_file(&link);
    }
}
