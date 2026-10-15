## Highlights

The archive now stores Modbus RTU the way the WireTAP desktop's own capture
store does, as `modbus_rtu`, so the two hold the same rows. That needs schema
v5, so **each archive waits for Migrate now again**, as it did for v4. This time
the migration rewrites only the archive's Modbus rows. An archive of CAN alone
has only its chunks read. The API is unchanged: you still ask for
`?protocol=modbus`.

**A capture daemon whose config file names no device at all now captures
`can0`**, where it used to capture nothing. See Changed.

### New

- **The gateway can serve under a base path.** Set `WIRETAP_BASE_PATH` (e.g.
  `/apps/wiretap-gateway`) and every route moves under it, the admin UI
  included. Unset, nothing moves.
- **`GET /healthz`** answers 200 only while Postgres is reachable, and the
  gateway image's health check now asks it. It used to ask `/v1/health`, which
  answers 200 regardless.
- **The capture daemon takes settings from a UI**, kept in
  `$STATE_DIRECTORY/ui.toml` beneath the config file, the environment and the
  flags, so your file still has the final say. `--check-config` lists each
  setting with where it came from. The UI talks to the daemon over a control
  socket, `$RUNTIME_DIRECTORY/control.sock`, open to the daemon's group.
- **`/frames` rows carry `crc_valid`** for Modbus.

### Changed

- **The daemon's config file is optional, and the installed one sets nothing.**
  An existing `/etc/wiretap-server/wiretap-server.toml` is left as it is. **If
  yours names no device at all, the daemon now captures `can0`; add
  `iface = ""` to keep CAN off.**
- **Assigning a catalogue now refuses the `cobs` and `length_prefixed`
  encodings**, and a serial header field that sets both `mask` and
  `start_byte`. **Check that your catalogues still assign** before you rely on
  a reassignment.
- A migration's progress is easier to read, and the Databases page offers one
  button for every database waiting.

### Upgrading

**After upgrading a gateway, migrate each archive to v5 from the Databases
page.** Until you do, its reads are refused and its capture servers' frames are
buffered, as for v4. The time v5 takes has not been measured on a large
archive. It reads every chunk, rewrites only the Modbus rows, and makes one
validating pass over the archive. A database migrates independently of the
others, so start with the small ones. `WIRETAP_AUTO_MIGRATE=true` migrates on
start instead.

Daemons upgrade in either order. One upgraded with Modbus messages still
cached sends them as before.

---

**Packages:** `wiretap-server_0.1.11_amd64.deb`, `wiretap-server_0.1.11_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.11`

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
