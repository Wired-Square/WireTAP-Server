## Highlights

First release of the WireTAP capture server and its database gateway — the two
halves that run on your network rather than on your desktop. Together they give a
CAN bus a permanent recorder: `wiretap-server` sits on a Linux host's SocketCAN
interfaces and captures continuously, `wiretap-backend` owns a TimescaleDB and is
the only thing that talks to it, and the desktop connects to whichever it needs.
The capture keeps running whether or not anyone is watching, and a frame is only
acknowledged once it is actually stored — so an archive that says it has your
capture has it.

The part worth knowing before you deploy anything: **a gateway going down does
not cost you frames.** The capture server notices the failed write, spills to a
local SQLite cache, and drains it in order when the gateway returns. The drill
that proves it — ten thousand frames, gateway stopped mid-stream and restarted,
then count what landed — is `tests/outage_drill.rs`, run by hand against a real
gateway rather than in CI, because it needs one.

It has been: **3 days 20 hours on a live Debian host reading two 250 kbit/s buses
— 83.7 million frames, none dropped, no restarts, and a disk cache that was never
needed.** Over that run it was compared frame-by-frame against the Python
implementation it replaces and the two archives were identical on every axis
checked: counts per bus and per id, payloads, DLC, flags and timestamps.

### New

- **Continuous CAN capture** from several SocketCAN interfaces at once, classic
  and CAN FD, with bit rates read from the kernel rather than assumed.
- **A GVRET server**, so the WireTAP desktop and SavvyCAN connect to this the way
  they connect to a serial adapter — and several clients can watch one bus at
  once without any of them slowing the capture.
- **Archiving to the gateway** in batches, acknowledged only after the write
  lands. Frames this server transmitted are archived too, tagged `tx`, so a
  request can be told apart from the traffic it was answering.
- **A disk cache that carries an outage** and drains oldest-first when the
  gateway comes back.
- **An ingest listener**, so a microcontroller too small to hold a connection to
  the gateway can push batches here instead. Every acknowledgement says how full
  the queue is, and a batch arriving at a full queue is refused rather than
  silently dropped — the device can only retry if it is told.
- **The gateway**: an HTTP query API, the ingest listener, a browser admin
  interface at `/admin` for API keys and databases, and a Compose stack that
  brings up TimescaleDB beside it.
- **One archive table for every protocol**, discriminated by `protocol`, so
  Modbus and serial capture can land beside CAN without a second migration later.
- **Debian packages** for amd64 and arm64, static musl builds installable on any
  distribution with systemd, under a hardened unit. Every maintainer script is
  exercised on both architectures on every push.
- **Test Pattern link validation**: a WireTAP desktop on the same bus sends a
  known sequence and this answers it, which is how you prove the transport is not
  quietly truncating payloads or downgrading CAN FD to classic. The FD half is
  proven at the socket and on a virtual bus; it has **not** yet been answered by
  a real FD-capable controller.
- **Every artefact names the commit it was built from** — the binary, its first
  journal line, the package version, the gateway's `/v1/health` and the image
  label. A version alone cannot tell two builds apart.

### Upgrading

Nothing to upgrade from — but two things to know before you install:

- **This transmits nothing by default.** The Test Pattern responder is the only
  part that can, and it stays off until you arm it with `[test_pattern] enable`.
  Armed, it says so at WARN on startup and names the interfaces. `tx_packets` on
  the interface is the proof either way.
- **If you are replacing the Python implementation**, its config file still
  parses and its disk cache is adopted and drained. The packaged unit shares the
  Python's unit name, so installing moves the old one aside and **deliberately
  does not start the new daemon** — carry your settings across, run
  `wiretap-server --check-config` to see what they resolve to, then start it.

**Pre-1.0, and the version means it.** The interfaces are not stable and the
binary ingest wire format is expected to break at v2, which will be a clean
version bump that refuses old clients at the handshake rather than ignoring them.
Pin exactly.

---

**Packages:** `wiretap-server_0.1.0_amd64.deb`, `wiretap-server_0.1.0_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.0` (and `:latest`).
The quick-start path does not need it — `docker compose up` in
`crates/wiretap-backend/` builds from source.

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
