## Highlights

A capture database now carries its own **events** — a user's annotations on
the archive, a moment or a span with a note — reached through
`/v1/db/{db}/events`, which is what the WireTAP desktop's Bookmarks become
for a backend source. And the schema says which columns belong to which
protocol: a Modbus row no longer has to pretend to a CAN `extended`/`is_fd`,
and one CHECK refuses a row whose columns disagree with its protocol.

### Added

- **Events**: `GET|POST /v1/db/{db}/events` and `PATCH|DELETE
  /v1/db/{db}/events/{id}` — `ts_us`, `duration_us`, `note`, microseconds
  since the epoch as `time-bounds` and `frames` already speak. Any key that
  may read a database may annotate it; a pinned key annotates only its own.

### Changed

- **Schema v3**, in two migrations the gateway applies on start as it did
  `0001`: `0002` reshapes the never-used `events` table in place (and refuses
  if it holds a row); `0003` drops NOT NULL from `capture_frame.extended` and
  `is_fd` and adds `capture_frame_protocol_columns_check`. A Modbus row
  written from now on stores NULL for the CAN pair; the read API keeps
  serving `false` for it, so nothing a client parses changes.
- **A migration rebuilds the hourly rollup only when it has to.** `0003`
  leaves the aggregate alone, and the gateway checks rather than assumes —
  the alternative was twenty-odd minutes of refused traffic on a large
  archive for a change that never touched it.
- A schema guard's reason now reaches the admin UI as the first line of the
  error, not `db error`.

### Upgrading

**Check your TimescaleDB first: this release needs 2.28.1 or later.**
`ALTER TABLE … ADD CONSTRAINT` on a compressed hypertable has a use-after-free
before that ([timescale/timescaledb#10094](https://github.com/timescale/timescaledb/pull/10094))
which fails the v3 migration at random and leaves the database half-migrated
and marked *failed*. `SELECT extversion FROM pg_extension WHERE extname =
'timescaledb'` tells you; if it is older, move the engine first, under the
gateway you already run — pin `timescale/timescaledb:2.29.2-pg16`, recreate,
run `ALTER EXTENSION timescaledb UPDATE` in every database that carries the
extension, restart the gateway — and only then move the gateway's tag. The
[README](https://github.com/Wired-Square/WireTAP-Server/blob/main/crates/wiretap-backend/README.md#migrating-an-existing-archive-into-the-container)
has the steps. TimescaleDB does not downgrade; snapshot first.

Then pull the gateway image and restart it; the migration runs on start. A
database being migrated refuses reads and writes and a capture daemon caches
to disk through it — budget **seconds per 100 million rows**: the CHECK in
`0003` validates every chunk once, measured at ~18 M rows/s on compressed
chunks, so about two minutes on a 1.8 B-row archive and under a second on
most. No rollup rebuild this time. The daemon packages are republished
unchanged so the two halves stay on one version; a `0.1.2` daemon is served by
a `0.1.3` gateway without complaint.

---

**Packages:** `wiretap-server_0.1.3_amd64.deb`, `wiretap-server_0.1.3_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.3` (and `:latest`).

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
