## Highlights

A serial line can now be captured raw: the bytes exactly as each read returned
them, unframed, alongside or instead of the framed Modbus messages. A framing
bug, such as a vendor message the framer cut one byte short, no longer leaves
the archive permanently wrong: the bytes are kept, so a corrected framer can be
run over them later. The raw stream can go to a database of its own.
Framed stays the default, so nothing changes until a device opts in.

### New

- A serial `[[device]]` takes `capture = "framed"`, `"raw"` or `"both"`, and
  `raw_database` to send the raw chunks to a database of their own (absent, they
  go to the device's `database`). Raw chunks are stored as `protocol = 'serial'`
  rows in up to 256 bytes each, stamped when their last byte arrived.
- `--check-config` shows each stream of a serial device with its database.
- A pusher on the daemon's ingest listener can send raw serial records, relayed
  when a device on that daemon sends raw chunks to the default database.

### Upgrading

**Upgrade the gateway before turning on raw capture.** Raw chunks travel over
ingest protocol v3; a `0.1.7` gateway accepts the session but refuses the
chunks, and the daemon sets them aside in its dead-letter file. A daemon with
no raw device keeps forwarding v2 and works against any gateway. No schema or
configuration change.

---

**Packages:** `wiretap-server_0.1.8_amd64.deb`, `wiretap-server_0.1.8_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.8`

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
