## Highlights

A batch the gateway can never store no longer holds up the link. Up to 0.1.4
the gateway answered every failed write "overloaded", so a batch PostgreSQL
refused for its contents, such as a timestamp out of range, was cached and
resent for ever, and nothing captured after it got through. The gateway now
says "malformed" for those, and the capture daemon sets the batch aside in a
dead-letter file and carries on; nothing is dropped. The gateway also recovers
on its own when PostgreSQL comes up after it, as after a reboot, where 0.1.4
needed a restart. No schema change.

### Changed

- **A HELLO the gateway cannot serve yet is refused as "unavailable"** (status
  4), not "bad database" (status 3): a PostgreSQL outage, or a schema check or
  migration in progress. The daemon backs off and retries either way; its log
  now says the gateway's database is not available yet. Status 3 is left for
  an invalid name, or a database that does not exist while auto-create is off.
- A read or a capture import that fails in the database answers 503, not
  400. A request PostgreSQL refuses for its contents is still 400.
- A serial device's `parity` is read in any case, so `"Even"` is taken.

### Fixed

- **A batch the gateway refuses for its contents is kept in
  `cache.dead-letter.db`** beside the daemon's disk cache (per database,
  `cache-<db>.dead-letter.db`), in the cache's own format. **If a daemon's
  link has been stuck resending the same batch, check for this file after
  upgrading both ends**: it is what was blocking it. To replay it, point
  `cache_path` at it.
- A batch sent in pieces that failed partway is no longer stored twice. Up to
  0.1.4 the whole batch was resent, duplicating the pieces already stored.
- A database that failed its schema check while PostgreSQL was down is tried
  again 30 seconds later, instead of refusing reads and ingest until the
  gateway restarted.
- A connection to a PostgreSQL that accepts and never answers gives up after
  10 seconds, instead of holding the request, the HELLO or the schema sweep.
- The daemon refuses an ACK for a batch other than the one it sent, and
  retries.
- The Ingest view no longer lists sessions that have gone, and shows each
  device's protocol version.
- The gateway no longer keeps an idle connection open to every capture
  database.

### Upgrading

Install the package and restart; pull the gateway image and restart it. No
configuration or schema change. Either end can go first: a 0.1.4 daemon reads
the new gateway's status 4 as any refusal and backs off, and a "malformed"
answer as any failure, so it caches and retries exactly as before. The
dead-letter file needs both ends at 0.1.5.

---

**Packages:** `wiretap-server_0.1.5_amd64.deb`, `wiretap-server_0.1.5_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.5`

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
