## Highlights

A small daemon release. A `modbus-rtu` line's catalogue is now read for the
only thing the daemon uses from it, its Modbus RTU length rules and its name,
which makes the static binary about 290 KB smaller. And an extended CAN frame
that happens to fall in the Test Pattern id range is no longer mistaken for a
Test Pattern request. No gateway or schema change.

### Changed

- **A catalogue's frames, `meta.version` and endianness are no longer checked
  at startup.** The daemon reads only its RTU rules and name, and still refuses
  a rule it cannot read. **A catalogue with an empty `meta.name` is now
  refused**; give it a name if yours has `name = ""`.
- The daemon is about 290 KB smaller (static aarch64 musl: 4.19 MB → 3.90 MB).

### Fixed

- An extended (29-bit) CAN frame whose id falls numerically inside the 11-bit
  Test Pattern ranges, such as extended `0x7E5`, is no longer answered during a
  Test Pattern run. Up to 0.1.5 it was echoed as a sweep request.

### Upgrading

Install the package and restart; no configuration change. The gateway image is
republished as `0.1.6` with no change from `0.1.5`, so there is no need to move
a gateway for this release.

---

**Packages:** `wiretap-server_0.1.6_amd64.deb`, `wiretap-server_0.1.6_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.6`

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
