# WireTAP Backend

A self-contained Docker stack that owns the long-term CAN capture database and
fronts it with an API, so **nothing connects to PostgreSQL directly** — not the
WireTAP desktop app, not microcontroller capture devices, not the Raspberry Pi
`wiretap-server`. The stack is:

- **TimescaleDB** (PostgreSQL 16 + TimescaleDB **2.28.1 or later**) — capture
  storage, one database per capture. Not published to the network.
- **`wiretap-backend`** (Rust / axum) — the only process that talks to Postgres.
  Two listeners plus a built-in admin UI:
  - **Binary ingest** (TCP 9323) — the protocol in wiretap-protocol's
    `docs/ingest.md` (linked from
    [docs/ingest-protocol.md](../../docs/ingest-protocol.md)), for MCUs and the
    Pi's forward mode. Writes are **ACK-after-write**: a batch is stored in Postgres
    before the device is acknowledged, so a database outage back-pressures the
    device into its own disk cache (nothing is buffered in gateway RAM). Rows
    PostgreSQL refuses for their contents (SQLSTATE class 22 or 23) are
    answered malformed, since no retry would store them.
  - **HTTP API** (8423) — the analytical query surface the desktop uses, plus
    events (a user's annotations on the archive), capture import, database
    management, export and import, and health.
  - **Admin UI** at `/admin` — API keys, databases, live ingest sessions,
    capture daemons and their catalogues, activity, the recent server log,
    health.
- **pgBackRest** (optional) — scheduled physical backups with PITR.

```
 MCU ────binary 9323───▶┌─ wiretap-backend ─┐
 Pi (forward mode) ────▶│  ingest + HTTP    │───▶ timescaledb  (not published)
 WireTAP desktop ──HTTP▶│  + /admin SPA     │      [+ pgbackrest, optional]
 Browser → /admin ─────▶└───────────────────┘
```

## Quick start

```bash
cd crates/wiretap-backend
cp .env.example .env
# edit .env: set POSTGRES_PASSWORD and WIRETAP_ADMIN_KEY (openssl rand -hex 32)
export CARGO_REGISTRIES_WIRED_SQUARE_MIRROR_TOKEN="$(../../scripts/cargo-mirror-token.sh)"
docker compose up -d --build
curl -fsS http://localhost:8423/v1/health      # {"status":"ok",...}
open http://localhost:8423/admin               # sign in with WIRETAP_ADMIN_KEY
```

The default capture database (`wiretap`) and the API-key store are created on
first start.

The build fetches the `wslib-*` crates from the private Cargo registry's mirror,
with its read token as a build secret: the variable, or the
`wired-square-mirror` token in `~/.cargo/credentials.toml`.

`/v1/health` reports `0.1.0 (unknown)` for an image built this way, because a
container build has no `.git` to read. That is honest and fine for development.
To build one that names its commit — worth it for anything you might still be
running next week:

```bash
WSLIB_BUILD_ID=$(../../packaging/build-id.sh) docker compose up -d --build
```

## API keys & roles

Keys live in the database (`wiretap_meta.api_keys`, sha256-hashed); the
plaintext is shown once at creation in the admin UI. `WIRETAP_ADMIN_KEY` from
the environment is a break-glass admin key that can't be disabled or revoked
from the UI, and works until it is
[rotated](#rotating-the-database-password-and-the-admin-key).

| Role | Can |
|------|-----|
| `read` | run queries, list databases, stream frames, annotate the archive with events |
| `ingest` | push frames (TCP or import); optionally pinned to one database |
| `admin` | everything, plus key/database management and activity control |

Create per-device `ingest` keys (optionally database-pinned) and per-user
`read` keys from the admin UI. Each key's ⋮ menu can **edit** its name and
database pin (not its role: make a new key for that), **rotate** it (a new
value for the same key, shown once, and the old one stops working at once),
**disable** and **enable** it (instant, reversible), or **revoke** it (deleted
for good).

`GET /v1/whoami` answers who a caller is (`via` `key` or `console`, `name`,
`role`); the admin UI asks it to decide whether to ask for a key.

### Behind an ApplianceOS console

With `WIRETAP_APP_SOCKET` set, the gateway also serves every route on that Unix
socket, under `WIRETAP_APP_BASE_PATH`, for the console's proxy. There a request
from `WIRETAP_CONSOLE_UID` is the user its `Appliance-Identity` header names,
and no key is asked for or read. A console `admin` or `hostadmin` is `admin`
here, and a `viewer` or `operator` is `read`. Any other uid on the socket gets
`/healthz`, `/v1/health` and the admin UI's files, and a 401 from every other
route; a malformed header from the console is a 400. The TCP listener ignores
the header and still needs a key.

Issuing or destroying a credential (creating, rotating or revoking a key, and
rotating either secret), and deleting or replacing a database, also needs the
console to say its user confirmed their password in the last five minutes (`reauth=` in the header, the chassis's
window). Otherwise it is a 403, `this needs the password confirmed in the last
5 minutes`, as the console's own such routes answer: confirm the password in the
console and try again. Editing, disabling and enabling a key are not asked, and
neither is a key on the TCP listener.

## Rotating the database password and the admin key

The admin UI's Keys tab has a Secrets section with a Rotate… for each. From a
shell,

```bash
docker compose exec backend wiretap-backend rotate db-password   # or admin-key
```

asks the running gateway to do the same, through
`POST /v1/admin/secrets/{db-password|admin-key}/rotate` (admin only, and
behind a console [a recent password](#behind-an-applianceos-console)), signed in
with the admin key from its own environment (`WIRETAP_ADMIN_KEY`, else
`WIRETAP_ADMIN_KEY_FILE`). After an admin key from the environment has been
rotated, that one is stale: pass the new one with
`docker compose exec -e WIRETAP_ADMIN_KEY=<new key> backend …`. Either way the
new value is shown once, in full: the CLI prints it on stdout, and what to do
next on stderr.

The gateway sets its role's new password with `ALTER ROLE CURRENT_USER`, from a
SCRAM verifier it computes itself, so the plaintext never reaches the server's
log and no superuser is needed. Open connections stay up, and new ones use the
new password at once. The old admin key stops working at once.

Where the secret comes from decides what is left to do:

- **A file**, `WIRETAP_PG_PASSWORD_FILE` or `WIRETAP_ADMIN_KEY_FILE`, as the
  gateway app image has it: the gateway saves the new value there, and nothing
  is left to do. A crash between the password change and the save is put right
  at the next start.
- **The environment**, `POSTGRES_PASSWORD` or `WIRETAP_ADMIN_KEY`, as the
  compose deploys have it: nothing is saved there. Put the new password in
  `.env` (or `gateway.env`) as `POSTGRES_PASSWORD`, for both the backend and the
  timescaledb services, before the gateway next restarts: until then a
  restarted gateway cannot reach the database. A new admin key is kept across
  restarts in the database, and the old one stays dead even if the gateway
  restarts with it; put the new one in `WIRETAP_ADMIN_KEY` too, so the env
  matches. Until you do, the gateway logs a warning at start and the Secrets
  section says the env holds a rotated key. Setting `WIRETAP_ADMIN_KEY` to a
  value it has never seen is a break-glass reset: that key becomes the admin
  key, and the previous one stops working.

In the shipped compose files the gateway logs in as the `postgres` superuser,
so a rotation changes that role's password: anything else that uses it, such as
a backup job or the `postgresql://postgres:$POSTGRES_PASSWORD@…` DSN in
[the migration runbook](#migrating-an-existing-archive-into-the-container),
needs the new one too. `psql -U postgres` inside the timescaledb container is
unaffected, because its local socket is trusted. If the gateway cannot reach the
database at all, set the password there by hand, then put it in `.env` and
`docker compose up -d`:

```bash
docker compose exec timescaledb psql -U postgres -c '\password postgres'
```

## Databases (one per capture)

Independent captures (a vehicle, a site, a bench rig) each get their own
database — drop a finished capture with `DROP DATABASE`, back them up
independently, and keep a runaway experiment from polluting the archive. Same
*system* with multiple CAN buses stays in one database, separated by the `bus`
column; so does a system with a CAN bus and a Modbus line, separated by the
`protocol` column — one time axis, so a command on one can be lined up against
the traffic it caused on the other. A capture daemon names a database per
device, so either is a configuration choice.

**Every read defaults to `protocol = can`.** `inventory`, `time-bounds` and
`frames` take `?protocol=modbus` to see the rest; without it a deployed desktop
sees exactly what it always did. A Modbus RTU row is stored as `modbus_rtu`, the
tag the desktop's own capture store uses, and its `id` is `unit << 8 | func`, its
`dlc` the message length, and `unit`, `func` and `crc_valid` are its own
columns.

**A CAN row's `dlc` is the length code, not a byte count**: 0–15, so a
12-byte FD frame is 9. The payload length in bytes is served beside it, as
`len` on each of `frames` and as `max_len` beside `max_dlc` in `inventory`.

**Each database carries its own events** — a user's annotations, a moment or a
span with a note, kept beside the frames they describe and independent of
protocol. `GET /v1/db/{db}/events?start=&end=&limit=` lists them oldest first
(`start`/`end` are ISO strings and both bounds are inclusive on `ts` — unlike
`frames`, whose `end` is exclusive; default limit 1000, no cap);
`POST /v1/db/{db}/events` with `{"ts_us", "duration_us"?, "note"?}` creates one
(201); `PATCH /v1/db/{db}/events/{id}` changes any of the three (200, 404 if
missing); `DELETE /v1/db/{db}/events/{id}` removes it (204, 404 if missing).
Times cross the wire as microseconds since the epoch, as `time-bounds` and
`frames` already do. A key that can read a database can annotate it.

`POST /v1/db/{db}/import` streams a local capture into a database, as an
`ingest` or `admin` key. The body is version 2 of wiretap-protocol's
`docs/import.md`: a `WTIM` header, then records carrying each frame's RTR,
BRS and ESI, stored as ingest stores them. A body without the header, from an
older desktop, or of another version is a 400 saying why. **Upgrade every
gateway before a desktop that sends version 2**: an older gateway misreads it
without an error. An import into a database behind the schema or migrating is
not buffered as ingest is: it is a 409 saying so, until the database is
migrated.

A database is created when: an admin creates it in the UI / API; an ingest
client names an unknown one in its HELLO (auto-create, when enabled); a
capture import targets one with `?create=true`; or an archive is imported as a
new one. Auto-create is gated by `WIRETAP_AUTO_CREATE` (default on).

**An ingest client must name its database**, unless its key is pinned to one.
There is no default: a HELLO that names none, from a key with no pin, is
refused as a bad database (status 3), and the gateway logs why. Every capture
daemon's `[forward] database` must be set, or its key pinned.

An admin can **delete** a capture database (its ⋮ menu, or
`DELETE /v1/databases/{db}`), after confirmation. It's refused while a device is
actively ingesting into it, migrating or being imported (409).

### The gateway's own database

`WIRETAP_DEFAULT_DB` (`wiretap`) is the gateway's own database: it holds
`wiretap_meta`, the API keys and the daemons' catalogues, and nothing else. It
is not a capture database. It is not listed, takes no captures, and every route
that names a database refuses it with a 403 (`'wiretap' is the gateway's own
database …`), as do create, delete, migrate, export, import and ingest. A fresh
install creates it without the capture schema. The variable keeps its name.

A gateway up to 0.1.12 captured into it whatever a client sent without naming
a database. Those rows are left where they are and no longer served,
and each start logs how many there are (`the gateway's own database holds
frames captured before …`, with `frames=`).

**Before upgrading a deployment from 0.1.12 or older**, count the frames its own
database holds, and check that every capture daemon names its database:

```bash
docker compose exec timescaledb psql -U postgres -d wiretap -tAc \
  "SELECT count(*) FROM public.capture_frame"
```

A count above zero is captures the upgrade stops serving, and the gateway
cannot export them: decide what to do with them first. An error that
`capture_frame` does not exist means there are none, unless the database
predates schema v1, whose frames are in `can_frame`. A daemon whose
`[forward] database` is empty, with a key that is not pinned, is refused from
the upgrade on, and keeps its frames in its disk cache until it names one.

### Export and import

Export writes a capture database, or a window of it, as an archive: one
gzip-compressed file of its frames and events, read from one snapshot while
ingest carries on. Import makes a new database from an archive, or replaces an
existing one with it. The format is [docs/archive.md](../../docs/archive.md).
All of it is the admin role's; behind a console, replacing needs a recently
confirmed password, and exporting or importing as new does not.

| Route | |
| --- | --- |
| `GET /v1/databases/{db}/export/estimate?from_us=&to_us=` | The database's bounds (`min_ts_us`, `max_ts_us`, across frames and events), and what an export of the window would hold: `frames` (from the hourly rollup, so to the hour), `events`, `approx_bytes`, and an exact `empty` |
| `POST /v1/databases/{db}/export` | Body `{}` for the whole database, or `{"from_us", "to_us"}` for `[from, to)`. Answers `{"url"}`: a single-use link, good for 60 s, that a browser fetches without a key. An empty window is a 400 |
| `GET /v1/exports/{token}` | The archive, streamed, as `<db>.wtar` or `<db>_<from>_<to>.wtar` |
| `POST /v1/databases/{db}/import` | The archive as the body, into a new database `{db}`: 409 if it exists |
| `POST /v1/databases/{db}/import?replace=true` | The archive in place of `{db}`: 404 if it does not exist, 409 unless it is at the current schema |

An import answers with what it loaded: `{"database", "replaced", "source",
"window", "rows": {"capture_frame", "events"}, "elapsed_ms", "attention"}`.
`attention` is null, or says why the database is in place but not serving yet:
its migration after the swap failed, and is retried. A bad archive is
a 400 saying why, read to its end first so the client sees it. Nothing is
touched until the archive has loaded and checked out in a staging database.
While a replace runs, the database refuses reads and holds its ingest, which
lands in the new database after the swap; a windowed archive leaves it holding
only that window. A gateway stopped partway drops the staging database at its
next start and stores the held ingest in the database it was held for.

Measured on the gateway's host build against TimescaleDB 2.29.2: a million
frames over 14 days (90% CAN, 10% Modbus, counting 8-byte payloads; the older
half compressed) exported in 0.7 s to a 7.5 MB archive, and imported as new in
2.9 s. The same with random 8-byte payloads is about 15 bytes a frame.

## Capture daemons and their catalogues

A capture daemon names itself and its devices in a v3 `HELLO`, and the gateway
can assign each serial line a catalogue to frame with (see
[docs/ingest-protocol.md](../../docs/ingest-protocol.md)). The admin role
manages it over HTTP, with `wiretap-gateway`'s types; a SHA is the
catalogue's Git blob SHA-1 as 40 hex characters, and times are epoch µs.

| endpoint | body | answers |
|----------|------|---------|
| `GET /v1/admin/daemons` | | 200 `DaemonList` |
| `PUT /v1/admin/assignments` | `AssignCatalog` | 200 `AssignedCatalog`, 400 `CatalogRejected`, 409 `AssignmentConflict` |
| `DELETE /v1/admin/assignments?daemon_id&interface&expected` | | 204, 409 `AssignmentConflict` |
| `GET /v1/admin/catalogs/{sha}` | | 200 `StoredCatalog`, 404 |

`daemons` lists every daemon that has named a device or been assigned a
catalogue. An interface assigned but never named has a null `bus`, `database`
and `last_seen_us`. Each device's `active` is the daemon's latest
`CATALOG_STATUS` for it, null before one.

A `PUT` stores `content` exactly as sent, CRLF and BOM included, and answers
400 with `findings` when `wiretap_catalog::validate` finds anything, when the
daemon's own reader would refuse the catalogue (`rtu_rules`, or an empty
`meta.name`), when the daemon id or interface could not be carried in a
`HELLO`, or when `provenance.blob_sha` is not the content's SHA. `warnings` is
always empty. `assigned_by` is the name of the key that made the change.

**`expected` guards a `PUT` or `DELETE`**: a SHA must be the one assigned now,
`""` means "only if unassigned", and absent means no guard. A mismatch is 409,
naming the SHA assigned now in `current`. The check and the write share one
transaction. A `DELETE` that passes its guard with nothing to clear is 404.
A change closes the daemon's sessions whose `HELLO` named the interface, so
they reconnect to it.

### Migration API

What the admin UI uses, and the stable hook for other clients.

`POST /v1/databases/{db}/migrate` (admin) starts migrating one database to the
gateway's schema and answers at once: 200 `{"status":"started"}`; 409 if it is
current, already migrating, or holds no capture schema; 404 if it does not
exist. Other databases may migrate at the same time. A database migrated by
hand is also started this way, and the run then only drains its buffer.

`GET /v1/databases` reports each database's state, with these fields beside
the rest:

| Field | Meaning |
|-------|---------|
| `schema_state` | `current`, `pending` (behind, waiting), `migrating`, `failed`, or `unknown` |
| `schema_version` | the version it is at; null while migrating |
| `busy_secs` | seconds the migration has run; null otherwise |
| `schema_error` | why the last migration failed; null otherwise |
| `buffered_rows` | frames this gateway has buffered for it since its last drain; null if none |
| `migration` | the migration's progress row, for a database not current; null if it keeps none |
| `migration.phase` | `backfill`, `check`, `rollup`, `drain`, `done`, or `failed` |
| `migration.chunks_done`, `chunks_total` | chunks converted, of the hypertable's total |
| `migration.rows_done` | rows converted in this run |
| `migration.current_chunk_start` | the chunk being worked on, or the one a failed run stopped at |
| `migration.compressed_left`, `uncompressed_left` | chunks still to visit, of each kind |
| `migration.avg_s_compressed`, `avg_s_uncompressed` | average seconds per chunk of each kind so far |
| `migration.last_error` | the error a failed run reported |
| `migration.updated_at` | when the row last changed |

The list also carries `schema_version`, the version every database is headed
for, and `auto_migrate`.

## Configuration (environment)

| Var | Default | Purpose |
|-----|---------|---------|
| `POSTGRES_PASSWORD` | — (required, unless `WIRETAP_PG_PASSWORD_FILE` is set) | Postgres superuser password (compose-internal) |
| `WIRETAP_ADMIN_KEY` | — | Break-glass admin key |
| `WIRETAP_PG_PASSWORD_FILE` | — | Read the Postgres password from this file, where a rotation saves the new one. `POSTGRES_PASSWORD` wins over it |
| `WIRETAP_ADMIN_KEY_FILE` | — | The same for the admin key. `WIRETAP_ADMIN_KEY` wins over it |
| `WIRETAP_DEFAULT_DB` | `wiretap` | The [gateway's own database](#the-gateways-own-database): keys and catalogues, never captures |
| `WIRETAP_AUTO_CREATE` | `true` | Allow ingest/import to auto-create databases |
| `WIRETAP_AUTO_MIGRATE` | `false` | Off, a capture database behind the schema waits for **Migrate now**, refusing reads and buffering ingest; on, every one is migrated on start. A new database is initialised either way |
| `WIRETAP_BASE_PATH` | — | Serve every HTTP route under this prefix, e.g. `/apps/wiretap-gateway` gives `/apps/wiretap-gateway/v1/…` and `/apps/wiretap-gateway/admin/`; anything outside it is a 404. Plain path segments only; empty serves at the root |
| `WIRETAP_APP_SOCKET` | — | Also serve on this Unix socket (an absolute path) for an ApplianceOS console |
| `WIRETAP_APP_BASE_PATH` | `/apps/wiretap-gateway` | The prefix the routes are under on the app socket |
| `WIRETAP_CONSOLE_UID` | `0` | The uid whose requests on the app socket may assert a caller; not `65534` |
| `RUST_LOG` | `wiretap_backend=info` | Log filter |

Full list (listen addresses, ingest keepalive/batch caps, the admin UI's
directory) is in [src/config.rs](src/config.rs).

`GET /healthz` (under each listener's base path) answers 200 while Postgres is
reachable and 503 when it is not, for a supervisor's probe. `/v1/health`
answers 200 either way and says which.

The Logging tab shows the newest 200 of the last 1000 records the gateway's own
crates logged, held in memory; stdout remains the durable log. A field named
like a credential shows as `[redacted]`, and a URL's `user:password@` as
`redacted@`. `GET /v1/admin/logs` answers them in
`wslib-telemetry`'s `LogSnapshot`, oldest first; `?level=warn` keeps that level
and louder, and `?limit=` sets how many (200).

## Optional: pgBackRest backups

```bash
docker compose --profile backup up -d
```

A sidecar initialises the `wiretap` stanza, switches WAL archiving on, and runs
diff backups daily (full on Sundays) into the `pgbackrest-repo` volume.
`archive_mode` is on from first boot, so enabling backups later needs no
restart. To restore, stop the backend, run `pgbackrest --stanza=wiretap restore`
against the data volume, and start again — rehearse this against a scratch
volume first.

## Migrating an existing archive into the container

If you already have months of captures in a host PostgreSQL, move them into the
container with [tools/migrate_to_timescale.py](../../tools/migrate_to_timescale.py).
It copies `public.can_frame` day-by-day from the source into the container's
hypertable, validating each day by row count + checksum and compressing as it
goes; it's resumable, so re-running skips finished days. (A plain `pg_dump`
won't work — the new hypertable drops the legacy `row_id`/`id_hex`/`data_hex`
columns, so the column sets don't match.)

**If the source archive predates 2026-09-10**, the frame table there is called
`can_frame` and has no `protocol` column. **The gateway migrates it for you**
when you press **Migrate now** — nothing to run by hand. That first step is fast
and rewrites nothing: 5.3 s for 87.8 M rows on a fully compressed hypertable,
with every chunk still compressed afterwards. The run carries on to the current
version, though, and v4 rewrites every chunk (below).

Two things to know when it does:

- **The hourly rollup is rebuilt as part of that migration**, which is why a
  migration takes longer than the schema change alone — 16 s per 88 M compressed
  rows, minutes on a very large archive. It is not optional: the aggregate comes
  back empty, and its maintenance policy would then materialise only a recent
  window and advance the watermark past the gap, leaving `inventory` silently
  under-reporting the archive's history.
- **A database refuses reads while it migrates, and buffers its ingest.** A
  batch from a capture server for a database behind or migrating is
  acknowledged and stored in `capture_frame_pending`, in the shape the gateway
  writes; it is moved into `capture_frame` and the rollup refreshed over it
  before the database serves again. A buffer, or a drained span not yet
  refreshed, that a crash left behind is finished on the next start. A capture
  import is refused instead.
- **An archive whose rollup does not reach back to its first frame is repaired
  on start**, whatever put it that way — a restored backup, an older build, or a
  maintenance run that materialised a recent window over an empty aggregate. That
  last case is why the check looks at the *earliest* stored bucket: a holed
  rollup has recent data and a small lag, so it reads as healthy on every obvious
  measure while omitting everything below the hole.

**Schema v3 needs TimescaleDB 2.28.1 or later.** Its migration adds a CHECK
to a compressed hypertable, and `ALTER TABLE … ADD CONSTRAINT` on older
TimescaleDB has a use-after-free (fixed by
[timescale/timescaledb#10094](https://github.com/timescale/timescaledb/pull/10094)
in 2.28.1) that fails the statement at random with `unrecognized node type`.
Every migration now refuses an older engine before it changes anything: the
database stays at its old version, marked *failed* in the admin UI with the
engine version it found, and **Migrate now** tries again.
0.1.3 did not check, and left such a database half-migrated and without its
`events` table. Check before upgrading a gateway that carries it:

```bash
docker compose exec timescaledb psql -U postgres -tAc \
  "SELECT extversion FROM pg_extension WHERE extname = 'timescaledb'"
```

If it is older, move the engine **first, under the gateway you already run**:
pin `timescale/timescaledb:2.29.2-pg16` (what the schema was measured on) in
the compose, recreate the stack, then in *every* database that carries the
extension — `postgres`, `template1`, the gateway's own database and each capture
database — run `ALTER EXTENSION timescaledb UPDATE` with `psql -X`, and restart
the gateway. Its start re-applies the schema through TimescaleDB functions,
which refuse a version mismatch until that `ALTER` has run. TimescaleDB does
not downgrade; snapshot first. Only then move the gateway's tag.

**An archive at schema v1** (any database the gateway has run since
2026-09-10) is taken to v3, then on to v4 and v5 below, in one pass:
[0002_events_annotations.sql](schema/migrations/0002_events_annotations.sql)
reshapes the never-used `events` table into the annotations table
(milliseconds; refuses to run if that table holds a row, and says so in the
admin UI), then
[0003_capture_frame_protocol_columns.sql](schema/migrations/0003_capture_frame_protocol_columns.sql)
ties `capture_frame`'s per-protocol columns to `protocol` with a CHECK and lets
a Modbus row leave `extended`/`is_fd` NULL; the CHECK scans every chunk once to
validate — measured at ~18 M rows/s on compressed chunks, so seconds per 100 M
rows, about two minutes on a 1.8 B-row archive.
The rollup is left alone: the gateway checks whether it still covers the
archive rather than rebuilding it after every migration.

**Schema v4 rewrites every chunk.**
[0004_capture_frame_flags.sql](schema/migrations/0004_capture_frame_flags.sql)
packs `extended`, `is_fd` and `dir` into one `flags` column, which also carries
a CAN frame's RTR, BRS and ESI, and drops the three; `can_frame` still serves
them by name. It backfills one chunk to a transaction, oldest first,
decompressing and recompressing each compressed one, so a run interrupted
anywhere resumes where it stopped. Then it swaps the CHECK (a scan) and rebuilds
the rollup without remote frames. Measured on 24 M rows: about 11 s per
2 M-row compressed chunk and 6 s per uncompressed one, 8 s for the CHECK and
13 s for the rollup; on a 645 M-row archive, 5 h 20 m. The database refuses
reads for all of it, and the gateway buffers its ingest. Budget transient space
of about 1.7 uncompressed chunks: a 3.76 GB chunk took 6.25 GB at its peak,
decompressed copy included. Updated uncompressed chunks keep their old row
versions until autovacuum reclaims them, about 2 GB on such a chunk. Time it on
a copy of the archive first.

**Schema v5 stores Modbus RTU as `modbus_rtu`.**
[0005_capture_frame_modbus_rtu.sql](schema/migrations/0005_capture_frame_modbus_rtu.sql)
relabels the Modbus rows chunk by chunk, decompressing only their own segments,
swaps both protocol CHECKs in one scan, and relabels the rollup's stored rows in
place rather than rebuilding it. A CAN-only archive pays only for the scan.

**A gateway does not migrate on start.** After an upgrade, each database
behind waits, refusing reads and buffering ingest, and the admin UI counts it in
a banner. Migrate each one when you choose, from the Databases page (**Migrate
now**) or with `POST /v1/databases/{db}/migrate` (see
[Migration API](#migration-api)). It runs in the background, the other
databases serve throughout, and the page shows its phase, chunks done of the
total, rows converted, the chunk it is on, the time so far and an estimate of
what is left. A run that fails shows its error and the chunk it stopped at;
**Migrate now** resumes it. `WIRETAP_AUTO_MIGRATE=true` migrates every
database on start instead, as earlier gateways did.

To do it by hand instead — worth it if you want to snapshot 30 GB first — keep
`WIRETAP_AUTO_MIGRATE=false` and run the migration for the
version the archive is *at* against the *source* archive, with `-f` and from
its own directory; each migration `\ir`s the next, so one command reaches the
current version from wherever it starts:

```bash
cd schema/migrations
psql "postgresql://user:pass@old-host:5432/legacy_archive" -f 0001_capture_frame.sql                  # from v0
psql "postgresql://user:pass@old-host:5432/legacy_archive" -f 0002_events_annotations.sql             # from v1
psql "postgresql://user:pass@old-host:5432/legacy_archive" -f 0003_capture_frame_protocol_columns.sql # from v2
psql "postgresql://user:pass@old-host:5432/legacy_archive" -f 0004_capture_frame_flags.sql            # from v3
psql "postgresql://user:pass@old-host:5432/legacy_archive" -f 0005_capture_frame_modbus_rtu.sql       # from v4
```

`-f`, not `< 0001_capture_frame.sql` and not `docker compose exec … psql`: the
files end in `\ir` lines that psql resolves relative to the script it is
reading. Fed on stdin there is no such path, and inside the container the file
is not there at all. Each sets its own `ON_ERROR_STOP`, so a non-zero exit means
it did not finish. The gateway notices the database is current on its next
request, drains what it buffered meanwhile, and serves it, without a restart.

### Writing a long migration

A migration gets buffered ingest, progress on the Databases page, resume and
"Migrate now" from the runner, without code of its own, if it:

- **works in chunks, each committed**: a procedure that `COMMIT`s per chunk, as
  0004's backfill does, never one transaction over the archive;
- **is idempotent and resumable**: a guard such as `flags IS NULL` skips work
  already done, so a second run, by the gateway or by psql, carries on;
- **reports through `wiretap_migration_progress`**
  ([migration_progress.sql](schema/migration_progress.sql)): `\ir
  ../migration_progress.sql` after the engine check, then call it with the
  target version, a phase, and chunks done, total, rows done, the current
  chunk, the compressed and uncompressed chunks left and the average seconds
  of each, as far as they apply. It is committed with each chunk, so a failed
  run names the chunk it stopped on;
- **assumes no reads during the run**: the database refuses them until it is
  current;
- **ends at what the new gateway writes**: ingest buffered meanwhile is in the
  shape the gateway writes (`writer::CREATE_PENDING`), and it is drained into
  `capture_frame` straight after the migration's last statement.

```bash
# 1. Bring the stack up (creates the target database + schema)
docker compose up -d

# 2. Switch new writes to the backend first (e.g. the Pi's [forward] mode) so
#    the source archive is static, then migrate. The container's Postgres is
#    not published by default — temporarily expose it:
#      uncomment "127.0.0.1:5432:5432" under timescaledb in docker-compose.yml
#      docker compose up -d
#    It binds to 127.0.0.1, so it is not reachable off the host — but re-comment
#    the line and `docker compose up -d` again once the migration is done. The
#    gateway is the only thing that should be talking to that database.
SRC=postgresql://user:pass@old-host:5432/legacy_archive
TGT=postgresql://postgres:$POSTGRES_PASSWORD@127.0.0.1:5432/wiretap
pip install psycopg2-binary    # one-off, for the migrator
../../tools/migrate_to_timescale.py --source-dsn "$SRC" --target-dsn "$TGT"

# 3. Verify counts, and that the rollup agrees with the base table. Compare the
#    two numbers: a real-time aggregate answers correctly over an *empty* rollup,
#    so its total alone proves nothing about materialisation — only a rollup with
#    a gap under its watermark disagrees, and that is what you are checking for.
docker compose exec timescaledb psql -U postgres -d wiretap \
  -c "SELECT protocol, count(*) FROM capture_frame GROUP BY 1;" \
  -c "SELECT count(*) FROM capture_frame;" \
  -c "SELECT sum(frame_count) FROM capture_frame_hourly;"
```

Then point the Pi at the gateway (`[forward]` in `wiretap-server.toml`) so new
frames land in the container rather than the old database.

### Pointing the desktop app at the gateway

The desktop app no longer connects to PostgreSQL directly — a database source is
a WireTAP Backend profile and nothing else. On first launch after upgrading, any
direct PostgreSQL profile is **removed from your settings and named in a
notice**, and its stored password is deleted from the keychain. Nothing else is
touched: captures, catalogues and every other profile are untouched.

To replace it:

1. In the admin UI, create an API key (per user, revocable). Copy it — the
   plaintext is shown once. **`read` covers querying, replay and analysis**,
   which is everything the migration path needs. Three app features want more:

   | App feature | Needs |
   |---|---|
   | Query engines, inventory, replay, cancelling a query, listing databases | `read` |
   | Query app's Database Activity view (list / cancel / terminate backends) | `admin` |
   | Importing a local capture into the backend | `ingest` or `admin` |
   | Creating a capture database from the app | `admin` |

   Test Connection only calls `/v1/health`, which is unauthenticated, so it
   succeeds with any key — it proves the gateway is reachable, not that the key
   is accepted. Run a query to confirm the key works.
2. In the app, **Settings → Data I/O → + Profile**, choose **WireTAP Backend**
   and fill in:
   - **Backend URL** — the gateway, e.g. `http://gateway.local:8423`
   - **API Key** — the key from step 1 (stored in the OS keychain)
   - **Capture Database** — the database this profile reads, e.g. `wiretap`.
     One profile per capture database; add several if you migrated more than one.
3. Optionally set a **Default Playback Speed** for replay.

Everything that worked against a PostgreSQL profile works against a backend
profile: the Query app's engines, bookmarks and time ranges, replay with speed
control, and the headless MCP analysis tools (`frame_inventory`,
`frame_byte_profile`, `frame_checksum_scan`, `catalog_coverage`, and the
`query_*` engines) — pass the backend profile's id as `profile_id` exactly as
before. See [docs/mcp-analysis-tools.md](https://github.com/Wired-Square/WireTAP/blob/main/docs/mcp-analysis-tools.md)
in the desktop repo, which also records how sampling differs between a capture
and an archive.

Once both read and ingest go through the gateway, retire the host PostgreSQL.

## Tests

```bash
cargo test                 # unit tests (proto codec, schema splitter, db names)
./parity_test.py           # backend SQL vs direct-psql ground truth
# protocol conformance against the running ingest listener:
python3 ../../tools/test_ingest_client.py \
    --host localhost --port 9323 --token "$WIRETAP_ADMIN_KEY" \
    --database conformance --conformance
```

`smoke_test.sh` needs the stack up **and a seeded database** — its read-path checks query
`frame_id` 2016 (`0x7E0`), and nothing here creates that data, so an unseeded run fails
four checks for the wrong reason. Seed it with the ingest client, which sends exactly the
ids those checks expect (`0x7E0`–`0x7E3`):

```bash
python3 ../../tools/test_ingest_client.py --host localhost --port 9323 \
    --token "$WIRETAP_ADMIN_KEY" --database vehicle_test --count 40
./smoke_test.sh http://localhost:8423 "$WIRETAP_ADMIN_KEY" vehicle_test localhost:9323
```

Expect **73 passed, 0 failed**. All four arguments are required, and the script refuses to
start without them. The third is the seeded database; the fourth is the ingest listener,
which the Modbus checks write through — it is the only path that carries a Modbus row. With
`PGHOST` and `PGPASSWORD` set (publish the port first, as the compose file's comment
shows), nine more reach PostgreSQL with `psql`, among them a database stamped a version
behind and migrated through `POST /v1/databases/{db}/migrate`, the last clearing the
catalogue checks' rows: 82.
