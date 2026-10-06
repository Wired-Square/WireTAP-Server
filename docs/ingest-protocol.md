# WireTAP Binary Ingest Protocol

The spec is `crates/wiretap-protocol/docs/ingest.md` in
[wslib-wiretap-rs](https://github.com/Wired-Square/wslib-wiretap-rs), at the tag the
workspace `Cargo.toml` pins; the .deb installs it as
`/usr/share/doc/wiretap-server/ingest-protocol.md`. Its reference client and
conformance suite is [tools/test_ingest_client.py](../tools/test_ingest_client.py).

```toml
[ingest]
enable = true
host = "0.0.0.0"
port = 9323
token = "CHANGE_ME"      # or env WIRETAP_INGEST_TOKEN; empty disables auth
keepalive_secs = 30      # clients silent for three times this are dropped
max_batch_frames = 256
```

Requires `[forward]`; `[server].iface = ""` for ingest-only. The token is sent
in clear text — deploy on a trusted network or wrap the connection in a VPN /
stunnel if it crosses untrusted segments.

The gateway and the capture daemon's listener both take versions 2 and 3.

The daemon's `[forward]` sink sends every database a v3 `HELLO` naming its
`daemon_id` and the devices whose records that database carries. A serial
line whose framed and raw streams land in different databases is named in
both. A gateway that predates v3 refuses it naming version 2; a database that
takes no raw chunks then reconnects with v2 and stays on v2 until the daemon
restarts, saying so once in the log. A database that takes raw chunks cannot
fall back, so its sink keeps failing, naming both versions, and caches to disk
until the gateway is upgraded.

The gateway records the devices of a named v3 `HELLO` in
`wiretap_meta.daemon_devices` and answers with each one's catalogue
assignment, keyed by daemon id and interface and sent on the bus this `HELLO`
named it on. It serves a `CATALOG_GET` from `wiretap_meta.catalog_blobs`
exactly as the catalogue was assigned, CRLF and all: `status = 1` (unknown)
for a SHA-1 it has no blob for, and `status = 2` (unavailable) when the
database cannot be read. When the meta database fails at a `HELLO`, the
gateway logs a warning and answers with no assignments rather than refusing
the session. An anonymous v3 `HELLO`, or a v2 one, records nothing and gets
no assignments. When a daemon's assignment changes, the gateway closes that
daemon's sessions whose `HELLO` named the interface, after any reply it owes,
and the reconnect reads the new assignment.

Before its first batch, a sink fetches any assigned catalogue it does not
have, checks the blob against its SHA-1, and parses it as it would a `catalog`
in `/etc`. It keeps the blob in `<state dir>/catalogs/<sha>.toml` and the
assignments in `<state dir>/assignments.json`, beside the disk cache. A
catalogue that cannot be fetched, checked or parsed is logged and the line
keeps what it frames with. The daemon's own listener has no assignments: it
answers a v3 `HELLO` with none and every `CATALOG_GET` with `status = 1`.

After those pulls, and before the first batch, a v3 sink sends one
`CATALOG_STATUS`: every bus its `HELLO` named, each with the catalogue it
frames with now (`assigned` or `local`, by the Git blob SHA-1 of the bytes,
or `none` for a CAN device, a raw-only line or a line with no catalogue), and
the assigned blob it last refused for that line, if any, with why: hash
mismatch, did not parse, or fetch failed. Taking an assigned catalogue, or the
assignment being cleared, ends the refusal. Whenever any line's catalogue or
refusal changes, by any session's pull, every v3 sink sends a fresh
`CATALOG_STATUS` ahead of its next batch or `PING`. Each is the whole state,
never a change, and none is answered. A v2 session sends none. The daemon's
listener takes one from a pusher and ignores it.

The gateway stores each `CATALOG_STATUS` in `wiretap_meta.daemon_active`, one
row per interface the session's `HELLO` named on a reported bus, and drops the
rows of that `HELLO`'s interfaces the report leaves out. A row's `since` moves
only when its catalogue does, not when a refusal comes or goes. A
`CATALOG_STATUS` from an anonymous session is ignored, and a meta database
failure is logged without ending the session. `GET /v1/admin/daemons` serves
it as each device's `active`.

The daemon's listener feeds the default database, so it relays a raw serial
record only when that database takes a device's raw chunks and is forwarded
with v3. Otherwise it ACKs the batch `status = 2` (malformed) and archives none
of it. The gateway stores a raw serial record as a `capture_frame` row with
`protocol = 'serial'`, `id = 0`, `dlc` the chunk's length, `dir` from the
record's transmitted bit, and the CAN and Modbus columns NULL. The id is not
the record's read sequence: compression segments by protocol and id, and an id
per read would make every row its own segment.

The gateway refuses a HELLO `status = 4` (unavailable) when it cannot serve
the database yet: PostgreSQL is down, or the database's schema is being
checked or migrated. `status = 3` (bad database) is only for an invalid name,
or a database that does not exist while auto-create is off.

The gateway ACKs a batch it fails to write `status = 3` (overloaded), unless
PostgreSQL refused the rows themselves with a data exception or an integrity
constraint violation (SQLSTATE class 22 or 23): that is `status = 2`
(malformed), because resending the same rows fails the same way. The capture
server's `[forward]` sink keeps such a batch beside its disk cache, in
`cache.dead-letter.db` by default, and moves on.
