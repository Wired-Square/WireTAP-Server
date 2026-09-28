# WireTAP Binary Ingest Protocol

The protocol's normative spec, and both ends of its codec, live in
wiretap-lib-rs:
[`crates/wiretap-protocol/docs/ingest.md` at v0.19.5](https://github.com/Wired-Square/wiretap-lib-rs/blob/v0.19.5/crates/wiretap-protocol/docs/ingest.md),
the tag the workspace `Cargo.toml` pins. This file keeps what that spec leaves
to WireTAP-Server.

A Python reference client and loopback test suite is
[tools/test_ingest_client.py](../tools/test_ingest_client.py).

## Capture daemon configuration

```toml
[ingest]
enable = true
host = "0.0.0.0"
port = 9323
token = "CHANGE_ME"      # or env WIRETAP_INGEST_TOKEN; empty disables auth
keepalive_secs = 30      # clients silent for three times this are dropped
max_batch_frames = 256
```

Requires a configured sink — `[forward]` to a gateway. (The Python
implementation also accepted `[postgres].enable = true`, writing to a database
directly; the Rust port drops that path, so ingested frames are always relayed
onward.) Set `[server].iface = ""` for an ingest-only deployment with no local
CAN hardware. The token is sent in clear text — deploy on a trusted network or
wrap the connection in a VPN / stunnel if it crosses untrusted segments.
