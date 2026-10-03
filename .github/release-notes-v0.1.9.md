## Highlights

A Modbus catalogue can now be assigned to a capture daemon's serial line from
the gateway, instead of being copied onto the appliance by hand. Pick the
daemon and line in the admin UI's new **Daemons** tab, upload the catalogue,
and the daemon pulls it, checks it against its hash, caches it, and starts
framing with it at the next read, with no restart. The tab then shows whether
each line is running the assigned catalogue, is still pending, or refused it
and why.

### New

- **Daemons tab** in the gateway's admin UI: each daemon's devices, the
  catalogue assigned to each and what the daemon reports it is running. Assign,
  clear and view, with a guard against two admins overwriting each other.
- Admin API: `GET /v1/admin/daemons`, `PUT` and `DELETE /v1/admin/assignments`,
  `GET /v1/admin/catalogs/{sha}`, all admin role.
- `[forward] daemon_id` names the daemon to the gateway. It defaults to the
  host's short name, so **give each daemon its own id if two hosts share a
  name**, such as two Pis left as `raspberrypi`.
- A line frames with the gateway's catalogue, else the `catalog` file in
  `/etc`, else none. A daemon restarted with the gateway down frames with the
  copy it kept. `--check-config` shows which one each line uses.

### Changed

- The daemon speaks ingest protocol v3 to every gateway, falling back to v2
  against a gateway older than `0.1.7`, which then assigns nothing.

### Upgrading

**Upgrade the gateway first.** Its first start creates five tables in the
`wiretap_meta` schema of the default database, beside the API keys. No capture
schema change and no configuration change. Then upgrade the daemons.

---

**Packages:** `wiretap-server_0.1.9_amd64.deb`, `wiretap-server_0.1.9_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.9`

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
