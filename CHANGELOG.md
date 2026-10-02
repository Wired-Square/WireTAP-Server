# Changelog

All notable changes to this project are documented here. Entries go under
`[Unreleased]` until a release is cut.

## [Unreleased]

### Changed

- **Built on `wiretap-lib-rs` v0.23.0.** A catalogue whose `meta.name` is not a
  string is refused as such, rather than as an empty name. A CAN frame ready to
  be read now goes ahead of the next send, so a run of transmits no longer holds
  back capture. The wire format is unchanged.

## [0.1.6] — 2026-09-29

The capture daemon reads a line's catalogue for its Modbus RTU rules alone,
about 290 KB smaller, and no longer answers an extended frame as a Test Pattern
request. No gateway or schema change.

### Changed

- **Built on `wiretap-lib-rs` v0.20.2**, which takes the id width when
  recognising a Test Pattern frame and leaves the wire format alone.
- **A line's catalogue is read for its Modbus RTU rules and name only**, which
  makes the musl binary about 290 KB smaller. A catalogue's other sections,
  such as its frames, `meta.version` and endianness, are no longer checked at
  startup.

### Fixed

- **An extended frame whose id falls in the Test Pattern range is no longer
  answered.** A 29-bit frame numerically inside the 11-bit ranges, such as
  extended `0x7E5`, was taken for a Test Pattern frame, and during a run was
  echoed as a sweep request. The 11-bit ids are now matched on standard frames
  only.
- **A catalogue with an empty `meta.name` is refused at startup**, as one
  with no name at all already was, rather than listed nameless.

## [0.1.5] — 2026-09-29

A batch the gateway refuses for its contents is set aside rather than resent
for ever; a batch half-delivered is not stored twice; and the gateway tells
"not yet" from "never" at HELLO, retries a database that failed its schema
check, and gives up on a PostgreSQL that never answers. No schema change.

### Added

- **The admin UI's Ingest tab shows each device's protocol version**, from
  `protocol_version` on each entry of `GET /v1/admin/ingest-sessions`: the
  version the device's HELLO spoke.

### Changed

- **A serial device's `parity` is read in any case**, so `"Even"` is taken
  where only `"even"` was. Anything but none, even or odd is still refused.
- **A query that fails in the database answers 503, not 400.** `time-bounds`,
  `inventory`, `frames`, `payloads`, `events`, the `query/*` endpoints and
  `activity` now return 503 when PostgreSQL fails, as they already did when no
  connection could be had. The message is unchanged. A value PostgreSQL
  cannot take, such as an unparseable `start`, is still 400, as are a
  cancelled query and a backend the key may not signal.
- **A capture import that fails in the database answers 503, not 400.** Rows
  PostgreSQL refuses with a data exception or integrity violation (SQLSTATE
  class 22 or 23) are still 400.
- **Built on `wiretap-lib-rs` v0.19.12**, which adds ingest and import helpers
  and leaves the wire format alone. The ingest spec in the .deb names them.
  The HTTP API's types now come from its `wiretap-gateway`: responses carry
  the same fields and values, though some list their keys in another order.
- **`smoke_test.sh` takes all four arguments or none of its checks run.** It
  no longer falls back to the dev stack's addresses, and prints its usage
  instead.
- **A HELLO the gateway cannot serve yet answers "unavailable", not "bad
  database".** A PostgreSQL outage, or a schema check or migration in
  progress, now refuses the HELLO with status 4, which a client backs off and
  retries. Status 3 is left for an invalid database name, or a database that
  does not exist while auto-create is off. The capture server's log says the
  gateway's database is not available yet.

### Fixed

- **The capture server checks that an ACK is for the batch it sent.** One
  carrying another batch's sequence number was taken as this batch's answer;
  it now fails the exchange, and the batch is cached and retried. A
  "malformed" answer for sequence 0, which the gateway gives a batch it
  cannot read one from, is handled as before.
- **The Ingest view no longer lists sessions that have gone.** A device that
  sent a second HELLO, or whose connection failed mid-reply, left its entry
  behind until the gateway restarted.
- **`GET /v1/databases` no longer holds a connection open to every capture
  database.** Its rollup probe left one idle for as long as the gateway ran;
  it now connects for the probe and closes the connection after it.
- **A connection to a PostgreSQL that never answers gives up after 10
  seconds.** A server that accepted the connection but stayed silent, or an
  address that dropped packets, held a request, a HELLO or the schema sweep
  for as long as it lasted, and a database showed `migrating` all the while.
- **A database that failed its schema check is tried again.** One that failed
  while PostgreSQL was down, as when the gateway starts first after a reboot,
  refused reads and writes until a restart. With `WIRETAP_AUTO_MIGRATE` on, a
  request now retries it once 30 seconds have passed since the failure.
- **A batch PostgreSQL refuses no longer blocks the forward link.** The
  gateway answered every failed write "overloaded", so a batch refused for
  its contents, such as a timestamp out of range, was cached and resent for
  ever, and nothing behind it got through. The gateway now answers
  "malformed" to a data exception or integrity violation (SQLSTATE class 22
  or 23), and the capture server keeps such a batch beside its disk cache
  (`cache.dead-letter.db` by default), in the cache's own format, and carries
  on. If that file cannot be written, the batch is cached and retried as
  before.
- **A forwarded batch the gateway took only part of is not stored twice.** A
  batch sent in several pieces that failed partway, from the queue or from the
  disk cache, was cached and resent whole, so the gateway stored the pieces it
  had already acknowledged a second time. Only the pieces it did not take are
  now kept to send again.
- **A pool is no longer kept for a database whose migration has just
  started.** One cached in that moment kept serving reads and ingest through
  the migration.

## [0.1.4] — 2026-09-29

The capture daemon reads CAN and serial lines through `wiretap-lib-rs`, frames
a Modbus line the same whatever its reads return, and loses nothing at
shutdown; the gateway refuses an engine too old to migrate on. No schema
change.

### Added

- **The gateway serves a CAN payload's length in bytes beside its `dlc`**:
  `len` on each frame from `frames`, and `max_len` beside `max_dlc` on each
  `inventory` entry. `dlc` is unchanged: for CAN it is the length code, so a
  12-byte FD frame is 9, and for Modbus the byte count. `max_len` is read
  from `max_dlc` through the FD table, so the schema needs no migration.
- **A `modbus-rtu` serial device takes an optional `catalog`**: the path of a
  catalogue whose `[meta.modbus.function_code.<code>]` tables declare the
  line's vendor codes and their lengths. Those messages are framed at the
  declared length, where the CRC search could stop short; other codes are
  still searched. The package ships `sungrow-rs485.catalog.toml` as an
  example: on the trial line's capture it cuts no message of its three codes
  short, and frames 99.995% of the bytes, up from 99.975%. The file is
  validated and read once, at startup, and a finding stops the server,
  naming the key. The path must be absolute, with no `.` or `..`, and outside
  `/home`, `/root`, `/run/user`, `/tmp` and `/var/tmp`, which the unit hides.
  `--check-config` and the startup log name each device's catalogue.

### Changed

- **The serial tap is `wiretap-lib-rs`'s `RtuTap`**, which the
  server's own tap moved into. Stamps are unchanged, except after a reopen
  that follows a backward clock step mid-message: they now hold the newest
  clock of any earlier read, not the last read's.
- **The tap frames a line the same whatever its reads return.** A read
  response and the broadcast behind it could pass their CRC as one request
  asking for more registers than Modbus allows, depending on where a read
  ended, and the broadcast was lost. The framer now refuses such a request:
  66 more messages in 1.39 million on the trial line's 34 MB capture.
- **The serial line is read through v0.19.6's `wiretap-io`**, in place of
  the server's own reader. It is still opened read-only with flow control
  off, reopened a second after it goes away, and a loss is logged as before.
  The open is now exclusive (`TIOCEXCL`): while the server holds the line, a
  second reader such as a debugging `cat` is refused with `EBUSY` instead of
  taking bytes from the tap. Root is not refused.
- **CAN is read and written through v0.19.6's `wiretap-io`**, in place of
  the server's own SocketCAN reader. Kernel stamps, FD and remote-frame
  handling, the bitrate fallback, and refusing to start without the
  interface are unchanged. An interface that goes away is now reopened by
  name, so an adapter unplugged and plugged back is read again without a
  restart; a downed one is reported as it goes down, where the old reader
  may have heard of it only from the next frame; and a loss is logged once,
  with `reopened` or `reading again` on its return, not once a second.
- **The server's own transmits are archived as the kernel hands them back.**
  A GVRET client's frame and a Test Pattern reply are still archived as
  `tx`, and still reach no GVRET client or console, but each is stamped by
  the kernel when it was sent rather than with the time the write returned.
- **A CAN interface that won't open no longer sends the operator after
  `CAP_NET_RAW`**, which an `AF_CAN` socket never needed. A missing
  interface points at `ip link show`, and a refused address family at the
  `can_raw` module and the unit's `RestrictAddressFamilies=`.
- **A GVRET transmit that declares more than 8 bytes is refused**, and
  logged with the client, bus, id and declared length. It used to be sent
  cut to its first 8 bytes, which on a live bus is a different message from
  the one the client sent. The connection carries on, and its next transmit
  goes out as usual.
- **The ingest codec is `wiretap-protocol`'s `ingest` module**, which the
  server's own `wiretap-ingest-proto` crate moved into, with its wire output
  unchanged byte for byte. Both listeners take a `TIME_RELATIVE` batch's base
  from it, so the gateway and the capture daemon stamp such a batch by one
  rule. The spec moved with it: `docs/ingest-protocol.md` now points to it and
  keeps only the `[ingest]` settings, and the .deb installs the spec itself.
- **A schema migration refuses TimescaleDB older than 2.28.1 before it
  changes anything.** 0.1.3 ran `0003`'s CHECK on such an engine anyway,
  where it fails at random (see 0.1.3's Notes) and leaves the database
  *failed* with `events` already dropped. Each migration now opens with the
  check, under the gateway and under psql alike, so the database stays at
  its old version, and the admin UI's *failed* badge names the engine version
  it found. A fresh database is still created on an older engine.

### Fixed

- **A frame read during shutdown is no longer lost as `queue FULL: size=0`.**
  The CAN readers and serial taps are now stopped before the archive closes,
  so every frame they have read is flushed. A frame that still arrives after
  the close is not counted in `dropped` or logged as `queue FULL`; it is
  logged once as `frame arrived after shutdown began, not archived`. An
  ingest batch that arrives after the close is refused with status 3, which
  a device answers by resending it later.

## [0.1.3] — 2026-09-20

The gateway gains per-database events and a schema that says which columns
belong to which protocol; the capture daemon is unchanged.

### Added

- **Events: a user's annotations on a capture database.** A moment or a span
  with a note — `ts_us`, `duration_us`, `note` — kept in the database it
  describes, protocol-agnostic, and reached through
  `GET|POST /v1/db/{db}/events` and `PATCH|DELETE /v1/db/{db}/events/{id}`.
  Any key that may read a database may annotate it; a pinned key annotates
  only its own. This is what the desktop's Bookmarks become for a backend
  source.

### Changed

- **Schema version 3, in two migrations the gateway applies on start as it
  did `0001`.** `0002_events_annotations.sql` reshapes `public.events` — an
  unused sketch from the schema's first version, empty on every deployment —
  in place, and refuses to run if the table holds a row.
  `0003_capture_frame_protocol_columns.sql` ties `capture_frame`'s
  per-protocol columns to `protocol` with one CHECK — a CAN row carries
  `extended` and `is_fd` and none of `unit`/`func`/`crc_valid`, a Modbus row
  carries those three — and drops NOT NULL from the two CAN columns, which a
  Modbus row has no honest value for: a Modbus row written from now on stores
  NULL there, rows written before keep the `false` the old NOT NULL demanded,
  and the read API says `false` for both, so nothing a client parses changes.
  `init_schema.sql` refuses either old shape by name rather than stepping
  over it. Measured on a copy of a real archive: the NOT NULL drop is
  catalogue-only, and the CHECK validates compressed chunks in place at
  roughly 18 M rows/s — expect seconds per 100 M rows of refused traffic,
  about two minutes on the largest deployed archive. **Needs TimescaleDB
  2.28.1 or later** — see Notes.

- **A migration rebuilds the hourly rollup only when it has to.** The rebuild
  after a migration now runs only if the rollup no longer covers the archive
  from its first frame — `0001` recreates the aggregate empty and so needs it;
  `0002` leaves it alone. Without this, bringing the largest deployed archive
  to v2 would have refused its traffic for the twenty-odd minutes the previous
  migration's rebuild took, for a change that never touched the aggregate.

### Notes

- **Schema v3 needs TimescaleDB 2.28.1 or later.** `ALTER TABLE … ADD
  CONSTRAINT` on a compressed hypertable has a use-after-free before that
  ([timescale/timescaledb#10094](https://github.com/timescale/timescaledb/pull/10094))
  which fails the migration at random with `unrecognized node type`, leaving
  the database half-migrated and marked *failed*. Found on a deployment
  running 2.27.2 by probing a throwaway database before the archives; the
  engine was moved to 2.29.2 first. Check
  `SELECT extversion FROM pg_extension WHERE extname = 'timescaledb'` before
  upgrading a gateway to this release; the README says how to move the
  engine. The shipped compose files now pin `timescale/timescaledb:2.29.2-pg16`.

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

[Unreleased]: https://github.com/Wired-Square/WireTAP-Server/compare/v0.1.6...HEAD
[0.1.6]: https://github.com/Wired-Square/WireTAP-Server/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/Wired-Square/WireTAP-Server/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/Wired-Square/WireTAP-Server/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Wired-Square/WireTAP-Server/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Wired-Square/WireTAP-Server/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Wired-Square/WireTAP-Server/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Wired-Square/WireTAP-Server/releases/tag/v0.1.0
