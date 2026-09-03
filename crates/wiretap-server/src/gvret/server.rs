//! The GVRET TCP listener and the per-client tasks it spawns.
//!
//! This is where the port stops looking like the Python. There, the capture
//! loop held a lock and did a blocking `sendall` for every connected client
//! before it could read the next frame, so one stalled SavvyCAN back-pressured
//! archiving for everyone. Here each client owns a task and a
//! [`broadcast::Receiver`]: a client that stops reading lags its own
//! subscription, the frames it missed are counted and logged, and the capture
//! side never waits for it. See `docs/porting-notes.md`.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};
use wiretap_model::{CanSample, SourceId};

use super::codec::{
    encode_canbus_params, encode_dev_info, encode_frame_into, encode_keepalive, encode_num_buses,
    encode_timebase, ClientCommand, Decoder, MAX_FRAME_BYTES,
};
use crate::source::Transmit;

/// One read of a client's command stream, as the Python's `recv(4096)`.
const READ_BUF: usize = 4096;

/// How much encoded traffic to gather before writing it.
///
/// A busy 1 Mbit/s bus is ~15k frames a second, and a syscall each would be
/// most of what this server does; a burst that arrived while the socket was
/// last being written leaves as one `write`. The cap only bounds the buffer —
/// whatever is ready goes out immediately, so it costs no latency.
const WRITE_COALESCE: usize = 8 * 1024;

/// How long to wait after a failed `accept` before trying again, so a
/// process-wide file-descriptor exhaustion cannot become a spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// What every client is told about the buses this server exposes.
#[derive(Debug)]
pub struct BusInfo {
    /// The highest bus number plus one — `bus_offset + interfaces`, as the
    /// Python computed it, so an offset shifts the count as well as the
    /// numbers.
    pub count: u8,
    /// Nominal bitrate per interface, in configuration order.
    pub speeds: Vec<u32>,
}

/// The listening socket, and everything a client task is given at accept.
pub struct Server {
    listener: TcpListener,
    buses: Arc<BusInfo>,
    frames: broadcast::Sender<Arc<CanSample>>,
    transmits: mpsc::Sender<Transmit>,
}

impl Server {
    /// Bind the listener. The default port is 23, so this is where an
    /// unprivileged run fails; the caller reports it, since it knows the
    /// address it asked for.
    pub async fn bind(
        host: &str,
        port: u16,
        buses: BusInfo,
        frames: broadcast::Sender<Arc<CanSample>>,
        transmits: mpsc::Sender<Transmit>,
    ) -> io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind((host, port)).await?,
            buses: Arc::new(buses),
            frames,
            transmits,
        })
    }

    /// The address actually bound, which is how a test finds the port it was
    /// given after asking for 0.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept forever, one task per client.
    pub async fn run(self) {
        loop {
            let (stream, peer) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                // A client that vanished between the SYN and the accept, or a
                // file-descriptor limit, must not take the capture down with
                // it — which is what the Python's unguarded accept did.
                Err(e) => {
                    warn!("accept failed: {e}");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            };
            info!("Client {peer} connected");

            let client = Client {
                peer,
                t0: Instant::now(),
                buses: self.buses.clone(),
                transmits: self.transmits.clone(),
            };
            tokio::spawn(client.serve(stream, self.frames.subscribe()));
        }
    }
}

/// One connected GVRET client.
struct Client {
    peer: SocketAddr,
    /// Every timestamp this client sees — the `F1 01` timebase reply and the
    /// one in each frame — counts from here, which is what the Python's
    /// per-connection `t0` did. Two clients therefore stamp the same frame
    /// differently, so the fan-out shares samples and each task encodes its
    /// own bytes.
    t0: Instant,
    buses: Arc<BusInfo>,
    transmits: mpsc::Sender<Transmit>,
}

impl Client {
    async fn serve(self, mut stream: TcpStream, mut frames: broadcast::Receiver<Arc<CanSample>>) {
        let mut decoder = Decoder::new();
        let mut rx_buf = [0u8; READ_BUF];
        let mut out = Vec::with_capacity(WRITE_COALESCE + MAX_FRAME_BYTES);

        loop {
            out.clear();
            tokio::select! {
                read = stream.read(&mut rx_buf) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        for command in decoder.feed(&rx_buf[..n]) {
                            self.reply(&mut out, command);
                        }
                    }
                },
                lagged = drain(&mut frames, &mut out, self.t0, decoder.is_binary()) => match lagged {
                    // The capture side is gone, so this connection has no
                    // reason to stay open.
                    None => break,
                    Some(0) => {}
                    Some(n) => warn!("Client {} is not keeping up, dropped {n} frames", self.peer),
                },
            }

            // The one place a client can stall, and it stalls only itself: the
            // broadcast behind it drops frames and says how many.
            if !out.is_empty() && stream.write_all(&out).await.is_err() {
                break;
            }
        }
        info!("Client {} disconnected", self.peer);
    }

    /// Append the answer to one command, or act on it.
    fn reply(&self, out: &mut Vec<u8>, command: ClientCommand) {
        match command {
            ClientCommand::DevInfo => out.extend_from_slice(&encode_dev_info()),
            ClientCommand::CanbusParams => {
                out.extend_from_slice(&encode_canbus_params(self.buses.count, &self.buses.speeds));
            }
            ClientCommand::NumBuses => out.extend_from_slice(&encode_num_buses(self.buses.count)),
            ClientCommand::Timebase => out.extend_from_slice(&encode_timebase(elapsed_us(self.t0))),
            ClientCommand::Keepalive => out.extend_from_slice(&encode_keepalive()),
            ClientCommand::Transmit {
                bus,
                arb_id,
                extended,
                data,
            } => {
                let queued = self.transmits.try_send(Transmit {
                    bus: SourceId(bus),
                    arb_id,
                    extended,
                    data,
                });
                // Not `send().await`: a wedged bus must not also stop this
                // client being read from, which would cost it received frames
                // as well. Transmits are rare enough that a full queue means
                // something is already wrong.
                if queued.is_err() {
                    warn!(
                        "Client {}: transmit queue full, dropped a frame for bus {bus}",
                        self.peer
                    );
                }
            }
        }
    }
}

/// Microseconds since a connection opened, wrapping at 2^32 — a little over 71
/// minutes — exactly as the Python's `& 0xFFFFFFFF` did.
fn elapsed_us(t0: Instant) -> u32 {
    t0.elapsed().as_micros() as u32
}

/// Encode everything the channel currently holds into `out`, returning how many
/// frames it dropped for this client on the way, or `None` once the capture
/// side has shut down.
///
/// Only the first receive awaits; the drain that follows is `try_recv`. That
/// single suspension point is what makes this safe to cancel, which `select!`
/// does every time the client sends a byte: cancellation can only happen before
/// anything has been encoded. **An `await` inside the drain loop would silently
/// drop whatever the buffer already held.**
///
/// Frames are received and discarded before the `E7 E7` handshake rather than
/// left in the channel, so a client that connects and never handshakes does not
/// accumulate a lag it would then be blamed for.
async fn drain(
    frames: &mut broadcast::Receiver<Arc<CanSample>>,
    out: &mut Vec<u8>,
    t0: Instant,
    binary: bool,
) -> Option<u64> {
    use broadcast::error::{RecvError, TryRecvError};

    let mut lagged = 0;
    match frames.recv().await {
        Ok(s) if binary => encode(out, &s, t0),
        // Taken and dropped: this client has not handshaken yet.
        Ok(_) => {}
        Err(RecvError::Lagged(n)) => lagged += n,
        Err(RecvError::Closed) => return None,
    }
    while out.len() < WRITE_COALESCE {
        match frames.try_recv() {
            Ok(s) if binary => encode(out, &s, t0),
            Ok(_) => {}
            Err(TryRecvError::Lagged(n)) => lagged += n,
            // A close is reported by the next `recv`; this call still owes the
            // caller the lag it counted.
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }
    Some(lagged)
}

fn encode(out: &mut Vec<u8>, s: &CanSample, t0: Instant) {
    encode_frame_into(
        out,
        elapsed_us(t0),
        s.arb_id,
        s.extended,
        s.bus.0,
        &s.data,
        s.is_fd,
    );
}

#[cfg(test)]
mod tests {
    use super::super::codec::SYNC;
    use super::*;
    use wiretap_model::Direction;

    fn sample(bus: u8, arb_id: u32) -> Arc<CanSample> {
        Arc::new(CanSample {
            ts_us: 0,
            arb_id,
            extended: false,
            is_fd: false,
            data: vec![0xAA, 0xBB],
            bus: SourceId(bus),
            dir: Direction::Rx,
        })
    }

    /// A running server, plus the ends of the two channels a test drives it
    /// through.
    struct Harness {
        addr: SocketAddr,
        frames: broadcast::Sender<Arc<CanSample>>,
        transmits: mpsc::Receiver<Transmit>,
    }

    async fn harness(buses: BusInfo) -> Harness {
        let (frames, _) = broadcast::channel(64);
        let (tx, transmits) = mpsc::channel(8);
        let server = Server::bind("127.0.0.1", 0, buses, frames.clone(), tx)
            .await
            .expect("bind an ephemeral port");
        let addr = server.local_addr().expect("bound");
        tokio::spawn(server.run());
        Harness {
            addr,
            frames,
            transmits,
        }
    }

    fn two_buses() -> BusInfo {
        BusInfo {
            count: 2,
            speeds: vec![500_000, 250_000],
        }
    }

    /// Read exactly `n` bytes, failing the test rather than hanging forever if
    /// the server sends fewer.
    async fn expect_bytes(stream: &mut TcpStream, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
            .await
            .expect("the server answered in time")
            .expect("the server sent enough bytes");
        buf
    }

    /// Connect and handshake, then wait for a reply that could only have been
    /// written after the handshake was parsed — so a frame published next
    /// cannot race the client task into binary mode and be discarded.
    async fn handshaken(addr: SocketAddr) -> TcpStream {
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(&[&SYNC[..], &[0xF1, 0x07]].concat())
            .await
            .unwrap();
        assert_eq!(
            expect_bytes(&mut c, 8).await,
            [0xF1, 0x07, 0x90, 0x01, 0x01, 0x00, 0x00, 0x00]
        );
        c
    }

    #[tokio::test]
    async fn a_client_is_answered_over_a_real_socket() {
        let h = harness(two_buses()).await;
        let mut c = TcpStream::connect(h.addr).await.unwrap();

        c.write_all(&[&SYNC[..], &[0xF1, 0x07, 0xF1, 0x0C]].concat())
            .await
            .unwrap();
        assert_eq!(
            expect_bytes(&mut c, 8 + 3).await,
            [0xF1, 0x07, 0x90, 0x01, 0x01, 0x00, 0x00, 0x00, 0xF1, 0x0C, 0x02]
        );

        // Bus parameters carry the speeds this server was built with.
        c.write_all(&[0xF1, 0x06]).await.unwrap();
        assert_eq!(
            expect_bytes(&mut c, 12).await,
            encode_canbus_params(2, &[500_000, 250_000])
        );
    }

    #[tokio::test]
    async fn a_captured_frame_reaches_a_client_as_gvret_bytes() {
        let h = harness(two_buses()).await;
        let mut c = handshaken(h.addr).await;

        h.frames.send(sample(1, 0x222)).unwrap();
        let got = expect_bytes(&mut c, 12 + 2).await;
        assert_eq!(got[0..2], [0xF1, 0x00], "a frame, not a reply");
        assert_eq!(u32::from_le_bytes(got[6..10].try_into().unwrap()), 0x222);
        assert_eq!(got[10], (1 << 4) | 2, "bus 1, two bytes");
        assert_eq!(&got[11..13], &[0xAA, 0xBB]);
    }

    /// The handshake gates frames, not just commands: until a client has sent
    /// `E7 E7` its socket carries something else entirely. The frame is still
    /// taken from the channel, or the client would be blamed later for a lag
    /// it could do nothing about.
    #[tokio::test]
    async fn frames_are_discarded_before_the_handshake() {
        let (frames, mut rx) = broadcast::channel(4);
        frames.send(sample(0, 0x111)).unwrap();

        let mut out = Vec::new();
        assert_eq!(
            drain(&mut rx, &mut out, Instant::now(), false).await,
            Some(0)
        );
        assert!(out.is_empty(), "nothing encoded");
        assert!(rx.is_empty(), "and nothing left to lag on");
    }

    /// A burst leaves as one write, which is the point of the coalescing.
    #[tokio::test]
    async fn a_burst_of_frames_is_delivered_in_order() {
        let h = harness(two_buses()).await;
        let mut c = handshaken(h.addr).await;

        for id in 0..20u32 {
            h.frames.send(sample(0, id)).unwrap();
        }
        let got = expect_bytes(&mut c, 20 * (12 + 2)).await;
        for (id, frame) in got.chunks_exact(12 + 2).enumerate() {
            assert_eq!(
                u32::from_le_bytes(frame[6..10].try_into().unwrap()),
                id as u32,
                "frames arrive in capture order"
            );
        }
    }

    /// The mapping an off-by-one would break: a transmit reaches the CAN side
    /// carrying the bus the client named, untranslated.
    #[tokio::test]
    async fn a_transmit_command_reaches_the_can_side() {
        let mut h = harness(two_buses()).await;
        let mut c = handshaken(h.addr).await;
        c.write_all(&[
            0xF1, 0x00, 0x23, 0x01, 0x00, 0x00, 0x01, 0x03, 0xAA, 0xBB, 0xCC,
        ])
        .await
        .unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), h.transmits.recv())
            .await
            .expect("the transmit was forwarded in time")
            .expect("the channel is open");
        assert_eq!(
            got,
            Transmit {
                bus: SourceId(1),
                arb_id: 0x123,
                extended: false,
                data: vec![0xAA, 0xBB, 0xCC],
            }
        );
    }

    /// The behaviour the whole fan-out exists for: frames a client could not
    /// keep up with are counted and dropped, never held against the capture.
    #[tokio::test]
    async fn a_slow_client_is_told_what_it_lost() {
        let (frames, mut rx) = broadcast::channel(2);
        for id in 0..5u32 {
            frames.send(sample(0, id)).unwrap();
        }

        let mut out = Vec::new();
        assert_eq!(
            drain(&mut rx, &mut out, Instant::now(), true).await,
            Some(3),
            "capacity 2, five sent: three are gone"
        );
        let ids: Vec<u32> = out
            .chunks_exact(12 + 2)
            .map(|f| u32::from_le_bytes(f[6..10].try_into().unwrap()))
            .collect();
        assert_eq!(
            ids,
            [3, 4],
            "and the drain reports the loss without stopping at it"
        );
    }

    #[tokio::test]
    async fn a_closed_capture_ends_the_connection() {
        let (frames, mut rx) = broadcast::channel::<Arc<CanSample>>(2);
        drop(frames);
        assert_eq!(
            drain(&mut rx, &mut Vec::new(), Instant::now(), true).await,
            None
        );
    }

    /// Both timestamps a client sees come from its own connection, so the
    /// timebase it reads back is comparable with the frames it is sent.
    #[tokio::test]
    async fn the_timebase_is_measured_from_the_connection() {
        let h = harness(two_buses()).await;
        let mut c = TcpStream::connect(h.addr).await.unwrap();
        c.write_all(&[&SYNC[..], &[0xF1, 0x01]].concat())
            .await
            .unwrap();

        let got = expect_bytes(&mut c, 6).await;
        assert_eq!(got[0..2], [0xF1, 0x01]);
        let us = u32::from_le_bytes(got[2..6].try_into().unwrap());
        assert!(
            us < 5_000_000,
            "a connection seconds old, not the server's uptime: {us}"
        );
    }
}
