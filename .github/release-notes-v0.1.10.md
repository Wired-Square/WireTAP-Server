## Highlights

The archive now keeps everything a CAN frame carries: remote frames (RTR), with
the length they request, and a CAN FD frame's BRS and ESI, served by `/frames`
beside `is_fd`. That needs schema v4, which rewrites every chunk of an archive,
and on a large archive that takes hours. So **a gateway no longer migrates on
start.** Each archive waits until you click **Migrate now** on the Databases
page, which shows the migration's progress. While an archive waits or
migrates, its capture servers keep sending and their frames are buffered and
added afterwards; only reads of that archive stop.

This release also brings the Raspberry Pi appliance, a card image you
administer from a browser, and a package that brings CAN adapters up by itself.

### New

- **Migrate now**, per database, on the gateway's Databases page or
  `POST /v1/databases/{db}/migrate`, with the phase, chunks done, rows, an
  estimate of the time left, and resume after a failure.
- **Ingest is buffered while a database waits or migrates**, and drained into
  it before it serves again. A capture server sees no outage.
- **RTR, BRS and ESI are archived** and served as `is_rtr`, `is_brs` and
  `is_esi`. A remote frame is stored with its requested length code as `dlc`
  and no data, and the analytical queries, inventory and the rollup leave it
  out.
- **`wiretap-can@<interface>.service`** configures a CAN interface from
  `/etc/wiretap-server/can.d/<interface>.conf`, and a gs_usb adapter with a
  conf comes up when plugged in.
- **The appliance card image**, built by hand for now (`appliance/README.md`).

### Changed

- **Schema v4** packs `extended`, `is_fd` and `dir` into one `flags` column.
  `can_frame` and its byte views still serve them by name; a plain INSERT
  through `can_frame` no longer works, and `ingest_can_frame` does.
- **The capture import body is versioned** (a `WTIM` header, version 2) and
  carries the flags. An older client is refused with a 400 that says so.
- **An import into a database waiting for migration is refused** with a 409:
  migrate it first.

### Fixed

- A CAN interface or serial device missing at startup is waited for, where
  `0.1.9` exited and was restarted every five seconds.
- A gs_usb adapter that cannot restart from bus-off comes up.
- A catalogue reassignment is logged on the daemon as a reconnect, not an
  outage.
- `--check-config` run without `STATE_DIRECTORY` says where it looked for the
  gateway's catalogue assignments.

### Upgrading

**Upgrade every gateway before any WireTAP desktop that sends the new import
body.** An older gateway reads it wrongly without an error.

**After upgrading a gateway, migrate each archive from the Databases page.**
Nothing migrates until you do, and until then its reads are refused. Measured
on real data: about 13 minutes per compressed day and 3 per uncompressed day
at ~22 M rows/day, and about 1.7× one uncompressed chunk of free space at the
peak. A database migrates independently of the others, so start with the small
ones. `WIRETAP_AUTO_MIGRATE=true` restores migrating on start.

Daemons upgrade in either order.

---

**Packages:** `wiretap-server_0.1.10_amd64.deb`, `wiretap-server_0.1.10_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.10`

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
