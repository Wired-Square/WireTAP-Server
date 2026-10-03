# WireTAP Binary Ingest Protocol

The spec is `crates/wiretap-protocol/docs/ingest.md` in
[wiretap-lib-rs](https://github.com/Wired-Square/wiretap-lib-rs), at the tag the
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

The gateway and the capture daemon's listener both take versions 2 and 3. The
daemon's `[forward]` sink sends version 3 only to a database some serial
device's raw chunks land in, and version 2 to the rest, so a gateway that
predates v3 still takes a daemon that captures no raw serial, ingest listener
or not. Catalogue assignment is not built yet, so a v3 `HELLO_ACK` carries no
assignments and every `CATALOG_GET` is answered `status = 1` (unknown).

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
