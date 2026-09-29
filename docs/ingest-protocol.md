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
