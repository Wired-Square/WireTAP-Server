## Highlights

The capture server now taps a Modbus RTU line as well as CAN. Point a
`[[device]]` at the serial adapter, name the baud rate, and every message on
the line is archived whole, CRC included — without knowing the vendor's
function codes first, which was the catch, because a site's function codes are
the thing you are trying to find out. The line is opened read-only at the
descriptor and the packaged unit grants the adapter read-only, so a tap cannot
transmit however it is configured.

The gateway now migrates its own databases on start, and **the schema does
migrate in this release**: a large archive can refuse traffic for minutes while
its hourly rollup is rebuilt. Capture servers cache to disk through that and
drain when it clears — nothing is lost, but it is worth knowing before you
upgrade a busy one.

It has been: the first live RS-485 tap, on a 9600 baud line to two Sungrow
inverters — **every message framed CRC-valid, seven conversations found with
no register map, nothing dropped on either the CAN or the Modbus pipeline**,
and the CAN buses' `tx_packets` still zero.

### New

- **A passive Modbus RTU tap.** `kind = "serial"`, an interface, a baud rate,
  `framing = "modbus-rtu"`; every function code framed, broadcast allowed. Each
  conversation appears in the archive's inventory as `unit << 8 | func`.
- **Configuration per device.** `[[device]]` tables put each device's frames in
  the gateway database you name, with a queue, a disk cache and a gateway
  session of their own. `[server] iface` still works for CAN, so nothing
  deployed changes. `mode = "passive"` on a CAN bus refuses every transmit on it.
- **The archive can be read by protocol.** `inventory`, `time-bounds` and
  `frames` take `?protocol=modbus`; without it they answer for CAN, exactly as
  before, so the desktop sees what it always did.
- **The gateway migrates its databases itself**, shows the schema version per
  database in the admin UI, and reports one consensus word on `/v1/health`.
  `WIRETAP_AUTO_MIGRATE=false` if you would rather snapshot and run it by hand.
- **An access log, and a Logging tab in the admin UI.** Every request with its
  status, duration and the key's name; the polled routes stay out of the way at
  DEBUG.
- **The gateway image is public.** `ghcr.io/wired-square/wiretap-backend`,
  amd64 and arm64; `deploy/` now pulls it rather than building on the host.

### Changed

- **A config file with neither `[server] iface` nor a `[[device]]` now captures
  nothing** and says so, where it used to capture `can0`. Every file the package
  has written names `iface`, so a deployed box is unaffected — but check a
  hand-written one.
- The ingest wire protocol is at **v2**. A CAN record is bit-identical to v1's;
  a Modbus record is new.

### Upgrading

**Gateway first, then capture servers**, at every site. This release's gateway
still accepts a `0.1.0` capture server, so nothing stops flowing between the
two steps; a `0.1.1` capture server against a `0.1.0` gateway is refused at the
handshake and caches to disk until the gateway is upgraded — the log says which
end is behind.

**Snapshot a large archive before upgrading its gateway.** The migration
rebuilds the hourly rollup and the database refuses reads and writes while it
does; measured at 23 minutes for 1.8 billion rows. The next release will not
accept a `0.1.0` capture server, so finish the second step before then.

**Adding a serial tap** is a `[[device]]` in `/etc/wiretap-server/wiretap-server.toml`
— the installed reference file has a commented example — and a restart.
`wiretap-server --check-config` lists every device with its bus number and
database. Plug the adapter in before starting the unit: the device allow-list
is resolved at start, and with no adapter ever seen the driver is not loaded.
If something else is reading the same line, stop it first — two readers on one
tty each get some of the bytes and neither gets a message.

---

**Packages:** `wiretap-server_0.1.1_amd64.deb`, `wiretap-server_0.1.1_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.1` (and `:latest`).

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
