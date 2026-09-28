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
