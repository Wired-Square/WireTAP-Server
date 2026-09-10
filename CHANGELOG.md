# Changelog

All notable changes to this project are documented here. Entries go under
`[Unreleased]` until a release is cut.

## [Unreleased]

First release. Two programs: a capture server that reads CAN buses and forwards
what it sees, and a gateway that stores it and answers queries. Nothing is
published yet — `packaging/make-deb.sh` builds the `.deb` locally, and the
gateway builds from source through its own Compose stack.

### Added

- **CAN capture over SocketCAN.** Several interfaces at once, classic and CAN FD,
  with bit rates read from the kernel over netlink rather than assumed. The
  capture path is Linux-only; everything else builds and runs anywhere.

- **A GVRET-compatible TCP server**, so a WireTAP desktop connects to this the way
  it connects to a serial adapter. Binary and text modes, the capability and
  bit-rate replies, and per-interface bus numbering.

- **Archiving to a gateway over a binary ingest protocol.** Batches of up to 256
  records, acknowledged only once the gateway has written them — so a slow
  archive is felt as a failed write rather than accepted and lost. Transmitted
  frames are archived too, tagged `tx`, so a request this server made can be told
  from the traffic it was answering. The wire format is
  [docs/ingest-protocol.md](docs/ingest-protocol.md).

- **A disk cache that carries an outage.** When the gateway is unreachable frames
  go to SQLite and drain in order when it returns, oldest first. A cache left by
  an older install is adopted at startup and then removed, before any frame is
  enqueued, so an upgrade during an outage keeps what the outage captured.

- **An ingest listener**, so a device too small to hold a connection to the
  gateway pushes batches here instead. They join the same queue the CAN readers
  feed, so a pushed frame is cached and drained exactly like a captured one.
  Every acknowledgement carries how full that queue is, and a batch arriving at a
  99%-full queue is refused rather than dropped silently — at-least-once delivery
  is the device's to complete, and it can only do that if a refusal is visible.

- **A Test Pattern responder.** A WireTAP desktop on the same bus runs a link
  validation and this answers it, proving the transport carries what it claims
  to. It is the only part of this server that transmits, so it is **off unless
  armed**, says `ARMED` at WARN naming the interfaces, and a config
  `enable = false` beats the command-line flag.

- **The gateway** — `crates/wiretap-backend/`: an HTTP query API, the ingest
  listener's server half, an admin SPA, and a Docker Compose stack with
  TimescaleDB.

- **A multi-protocol capture schema.** One `capture_frame` hypertable
  discriminated by `protocol`, so CAN, Modbus and serial share a table, with
  hourly continuous-aggregate rollups and compression. The CAN-only names it used
  before are kept as filtered views. `schema/migrations/0001_capture_frame.sql`
  migrates an existing archive in one command.

- **Debian packaging.** A `.deb` for amd64 and arm64, a hardened systemd unit,
  and maintainer scripts that handle upgrading from a pre-packaging deployment —
  the old unit is moved aside and the daemon deliberately left stopped so
  settings can be carried across first. `packaging/tests/deb-lifecycle.sh` runs
  install, reinstall, upgrade, remove, purge and a chroot against a real systemd.

- **Every artefact names the commit it was built from.** `wiretap-server
  --version`, the daemon's first journal line, the package version, the gateway's
  `/v1/health` and the image's `org.opencontainers.image.revision`. A `0.1.0`
  on its own identifies nothing.

### Notes

- The capture server is a **read-only tap** except for the Test Pattern
  responder, which is disabled by default. `tx_packets` on the interface is the
  proof, and it is worth checking after any session on a live bus.
- Verified on a real Debian 13 host reading two live 250 kbit/s buses for 3 days
  20 hours: 83.7 M frames, nothing dropped, no restarts, and a byte-identical
  comparison against the Python implementation this replaces.
