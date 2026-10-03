## Highlights

The gateway now speaks ingest protocol v3, the groundwork for assigning a
catalogue to a capture device from the gateway rather than by copying a file
onto the appliance. Nothing assigns one yet: this release only makes the
gateway and the daemon's listener ready for it. **The gateway no longer
accepts protocol v1**, as the v2 release said it would; see Changed. A GVRET
transmit also waits for room on the bus rather than being dropped.

### Changed

- **A v1 ingest client is refused by the gateway.** Every WireTAP Server since
  `0.1.1` forwards with v2, so only firmware or a capture daemon older than
  that is affected. **Upgrade any such client before moving the gateway.**
- The gateway and the daemon's listener take v2 and v3. A v3 client is told
  it has no catalogue assignments, and a batch carrying raw serial bytes is
  refused, until those features are built.
- The daemon still forwards with v2, so a `0.1.7` daemon works against an
  older gateway.

### Fixed

- A transmit from SavvyCAN or the desktop while the interface's send queue is
  full waits for room instead of being refused. Up to `0.1.6` it was dropped
  with a warning in the journal.
- A forwarding key too long for the protocol now fails to connect, naming its
  length. Up to `0.1.6` it was sent with a wrapped length byte, which the
  gateway misread.

### Upgrading

Upgrade the gateway first, then the capture daemons, as for any protocol
release. No schema or configuration change.

---

**Packages:** `wiretap-server_0.1.7_amd64.deb`, `wiretap-server_0.1.7_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.7`

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
