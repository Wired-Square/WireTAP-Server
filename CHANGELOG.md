# Changelog

All notable changes to this project are documented here. Entries go under
`[Unreleased]` until a release is cut.

## [Unreleased]

### Added

- **Events: a user's annotations on a capture database.** A moment or a span
  with a note — `ts_us`, `duration_us`, `note` — kept in the database it
  describes, protocol-agnostic, and reached through
  `GET|POST /v1/db/{db}/events` and `PATCH|DELETE /v1/db/{db}/events/{id}`.
  Any key that may read a database may annotate it; a pinned key annotates
  only its own. This is what the desktop's Bookmarks become for a backend
  source.

### Changed

- **Schema version 2.** `public.events` — an unused sketch from the schema's
  first version, empty on every deployment — is reshaped in place by
  `0002_events_annotations.sql`, which refuses to run if the table holds a row.
  The gateway applies it on start as it did `0001`; `init_schema.sql` refuses
  the old shape by name rather than stepping over it.

- **A migration rebuilds the hourly rollup only when it has to.** The rebuild
  after a migration now runs only if the rollup no longer covers the archive
  from its first frame — `0001` recreates the aggregate empty and so needs it;
  `0002` leaves it alone. Without this, bringing the largest deployed archive
  to v2 would have refused its traffic for the twenty-odd minutes the previous
  migration's rebuild took, for a change that never touched the aggregate.

## [0.1.2] — 2026-09-12

### Changed

- **A serial tap stamps every message when its last byte arrived**, including
  the ones released together when the framer first syncs after an open or a
  reopen — those used to share the clock of the read that released them, up
  to 256 bytes of line time late. The tap remembers the clock of each read
  and stamps by the read that delivered the message's last byte, less the
  wire time of the bytes after it in that read — never reaching behind a read
  the tap has already seen return, so a line's stamps never go backwards.
  Built on `wiretap-lib-rs` v0.16.5, whose framer now reports where each
  message ended and takes `frame_any_function()` in place of a declaration of
  all 256 codes.

### Notes

- Where a single-register read response is followed by a broadcast, the
  framer can take a one-byte-longer reading that also passes its CRC and
  swallows the broadcast's address byte — the broadcast is then lost. Which
  reading wins depends on where the serial read happened to end. Measured at
  one response in 67 on the Sungrow line, every one a dispatch or write
  broadcast. The framer is `wiretap-lib-rs`'s and the fix belongs there;
  until it lands a tap's archive may hold fewer broadcasts than the line
  carried.

## [0.1.1] — 2026-09-12

The capture server taps a Modbus RTU line as well as CAN, and the gateway
migrates its own databases. The `.deb` files are on the releases page and the
gateway image is public at `ghcr.io/wired-square/wiretap-backend`.

### Added

- **A passive Modbus RTU tap.** A `[[device]]` of `kind = "serial"` opens the
  line read-only — `O_RDONLY`, and the packaged unit allows the adapter with `r`
  alone — and frames what it reads with `wiretap-catalog`'s `ModbusRtuStream`,
  every function code searchable and broadcast allowed, so a line full of a
  vendor's own codes is captured without knowing them first. Messages are stored
  whole, CRC included, keyed by `unit << 8 | func`. The line is reopened when it
  goes away.

- **Configuration per device.** `[[device]]` tables name a kind, an interface,
  a mode and the gateway database the device's frames land in, and the daemon
  runs one archive pipeline — its own queue, cache file and gateway session —
  per database. `[server] iface` stays as shorthand for CAN devices, so deployed
  files are untouched. `mode = "passive"` on a CAN device refuses GVRET
  transmits on that bus and never arms the responder there.

- **Ingest protocol v2.** The `BATCH` record names its kind and carries up to
  256 bytes, so a Modbus message crosses the wire whole; a CAN record's id word
  is bit-identical to v1's. The gateway accepts v1 sessions **for this release
  only**, so a capture daemon still on v1 keeps flowing while the gateway is
  upgraded first — upgrade in that order. The daemon's own listener speaks v2
  only. `docs/ingest-protocol.md` is rewritten for it.

- **Reading the archive by protocol.** `inventory`, `time-bounds` and `frames`
  take `?protocol=`, defaulting to `can` so every deployed desktop sees exactly
  what it did; a misspelt protocol is a 400 rather than an empty result.

- **The gateway migrates its own databases.** Each capture database carries a
  `schema_version`, and on start the gateway brings any that are behind up to
  the current one — the default database inline, the rest in a background sweep
  once it is already serving. A database being migrated refuses reads and writes;
  a capture server refused that way treats it as an outage, caches to disk and
  drains when it clears, so nothing is lost.

  It only touches databases that already carry the capture schema. A cluster can
  hold databases nothing to do with this gateway, and creating hypertables in
  them would be vandalism rather than migration.

  The admin UI shows a schema version per database with a badge while one is
  migrating, and Health answers the deployment-wide question — `v1 — all 3
  databases`, expanding to name the odd ones out when they disagree.
  `/v1/health` carries a one-word `schema` consensus for external monitors,
  without database names, since it is the one endpoint that needs no API key.

  `WIRETAP_AUTO_MIGRATE=false` turns it off, for an operator who would rather
  snapshot a large archive first and run
  `schema/migrations/0001_capture_frame.sql` by hand.

  **Migrating rebuilds the hourly rollup, and has to.** The migration recreates
  the aggregate empty; its maintenance policy then materialises only a recent
  window and advances the watermark past it, and a real-time aggregate serves
  buckets *below* the watermark from the materialisation table alone — it does
  not recompute the gaps, it omits them. Measured: a 5 760-frame archive reported
  **121** after one policy run. So the migration backfills before returning. That
  is 16 s per 88 M compressed rows and minutes on a very large archive, spent
  while the database is already refusing traffic. The admin UI keeps a Rebuild
  button for repairing one by hand.

  On an idle archive the policy writes nothing and the fault never appears, which
  is why it survives testing and waits for a live capture.

  As a safety net the startup sweep also repairs any rollup that does not reach
  back to its archive's first frame — a restored backup, one migrated by an older
  build, or one the policy has already holed. A rollup merely *behind* the present
  is left alone: that is the policy doing its job.

  It asks whether the *earliest* stored bucket reaches the earliest frame,
  because the obvious measures cannot see the fault. A count of materialised
  chunks is non-zero the moment the policy writes its first window, and the lag
  to the newest frame is small for the same reason — so a rollup missing four
  days of history reports healthy on both.

  The migration itself is 5.3 s for 87.8 M compressed rows and decompresses
  nothing.

- **An access log, and the Logging tab that reads it.** Every request is logged
  with its method, path, status, duration and peer — and its key's *name*, never
  the key. Until now a request that was served, refused or never arrived left no
  trace at all, and the difference between those three had to be established by
  replaying the query by hand on the host.

  The level follows the outcome: 5xx at ERROR, 4xx at WARN, the rest at INFO. The
  traffic nobody asks about — the 15-second container healthcheck, the admin
  bundle, every route the admin tabs poll — goes to DEBUG on success, because
  at INFO it would evict everything worth reading within minutes. Which routes
  those are is declared on the route table, so the next polled endpoint cannot
  be forgotten by a list somewhere else. `RUST_LOG` still governs.

  The same records fill a bounded in-memory ring that `/v1/admin/logs` serves to
  the admin UI, so the recent log is readable without a shell on the host. It is
  a tail, not an archive: `WIRETAP_LOG_BUFFER` records (2000 by default), lost on
  restart, with the container's stdout still the durable copy.

### Changed

- A config file's device set is exactly what it names. A file with no
  `[server] iface` and no `[[device]]` used to capture `can0` by default; it now
  captures nothing and says so. A hand run with no config file still captures
  `can0`. Every shipped and deployed file names `iface`, so none is affected.

- `tools/test_ingest_client.py --conformance` takes `--daemon` when the listener
  is a capture daemon's, which refuses a v1 HELLO where a gateway still accepts
  one.

### Notes

- A serial tap is read-only by construction: the descriptor cannot be written,
  and the packaged unit grants the adapter read-only. `tx_packets` on the CAN
  interfaces remains the proof there.
- Verified on a live 9600 8N1 RS-485 line to Sungrow inverters: every message
  framed CRC-valid, seven conversations found without a register map, nothing
  dropped on either pipeline.

## [0.1.0] — 2026-09-10

First release. Two programs: a capture server that reads CAN buses and forwards
what it sees, and a gateway that stores it and answers queries.
`packaging/make-deb.sh` builds the `.deb`, and the gateway builds from source
through its own Compose stack.

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

[Unreleased]: https://github.com/Wired-Square/WireTAP-Server/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/Wired-Square/WireTAP-Server/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Wired-Square/WireTAP-Server/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Wired-Square/WireTAP-Server/releases/tag/v0.1.0
