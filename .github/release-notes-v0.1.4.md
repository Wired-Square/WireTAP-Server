## Highlights

The capture daemon now reads CAN and serial lines through `wiretap-lib-rs`,
so an unplugged CAN adapter is read again when it comes back, with no restart,
and a Modbus line is framed the same whatever sizes its reads return. A stop no
longer loses the last frames it had read, or blames a full queue for them. The
gateway now refuses a TimescaleDB too old to migrate on before it changes
anything, where 0.1.3 could leave a database half-migrated. No schema change.

### New

- **A `modbus-rtu` device can name a catalogue of its vendor codes** (`catalog`
  in the device's settings), so those messages are framed at their declared
  length. The package ships `sungrow-rs485.catalog.toml` as an example. On the
  trial line it frames 99.995% of the bytes, up from 99.975%.
- The gateway serves a CAN payload's length in bytes, `len` and `max_len`,
  beside `dlc` and `max_dlc`.

### Changed

- **The serial line is opened exclusively.** While the daemon holds it, a
  second reader such as a debugging `cat` gets `EBUSY` instead of silently
  taking bytes from the capture. **Stop the service before you read the line
  by hand.**
- A CAN interface that goes away is reopened by name. A loss is logged once,
  with `reopened` or `reading again` when it returns, not once a second.
- The daemon's own transmits are stamped by the kernel when they were sent.
- **A GVRET transmit that declares more than 8 bytes is refused** and logged.
  Up to 0.1.3 it was sent cut to its first 8 bytes, which on a live bus is a
  different message.
- A CAN interface that won't open now points at `ip link show` and
  `can_raw`, not at `CAP_NET_RAW`.

### Fixed

- **A stop no longer drops the frames it had just read** and logs
  `queue FULL: size=0` for them. It fired on 2 of 5 production restarts on 2026-09-12.
- **A migration no longer runs on TimescaleDB older than 2.28.1.** It stops
  first, the database stays at its old version, and the admin UI's *failed*
  badge names the engine version. 0.1.3 could leave such a database
  half-migrated without its `events` table.
- A serial line no longer frames a response and the broadcast behind it as
  one oversized request, depending on where a read ended: 66 more messages in
  1.39 million on the trial line's 34 MB capture.

### Upgrading

Install the package and restart; no configuration change is needed. The
gateway has no schema change, so pulling the image and restarting it runs no
migration. A `0.1.2` or `0.1.3` daemon keeps working against a `0.1.4`
gateway: its ingest traffic decodes to the same rows, and every reply the
gateway sends is one the daemon already handles.

**Coming from a `0.1.2` gateway, follow 0.1.3's Upgrading first:** the v3
migration still needs TimescaleDB 2.28.1 or later. `0.1.4` refuses an older
engine instead of failing halfway, but the engine still has to be moved
before the gateway.

---

**Packages:** `wiretap-server_0.1.4_amd64.deb`, `wiretap-server_0.1.4_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.4` (and `:latest`).

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
