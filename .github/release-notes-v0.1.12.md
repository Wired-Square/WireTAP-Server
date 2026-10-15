## Highlights

A fix for `0.1.11`'s Migrate now to v5. It stopped with `tuple decompression
limit exceeded` on an archive with more than 100 000 Modbus rows in a day,
because TimescaleDB limits how many compressed rows one statement may
decompress. The migration now lifts that limit for its own relabel, one chunk at
a time. The capture server is unchanged.

### Fixed

- **Migrate now to v5 finishes on an archive with more than 100 000 Modbus rows
  in a day**, where `0.1.11` failed. **If it failed for you, upgrade the gateway
  and click Migrate now again**: it resumes where it stopped, and nothing was
  lost meanwhile. The frames captured since were buffered.

### Upgrading

Upgrade the gateway. The capture server can stay on `0.1.11`. Archives without
Modbus rows that `0.1.11` already migrated need nothing.

---

**Packages:** `wiretap-server_0.1.12_amd64.deb`, `wiretap-server_0.1.12_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.12`

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
