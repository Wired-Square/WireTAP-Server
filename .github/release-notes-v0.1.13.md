## Highlights

The gateway can now export a capture database to a file and import one back,
whole or a time window, so a database can be copied, moved or seeded without
`pg_dump`. Its database password and admin key can be rotated from the admin UI
or the CLI, with the new value shown once to paste where it is kept. **The
gateway's own database, `wiretap`, no longer takes captures: set every capture
server's `[forward] database` before upgrading the gateway** (see Upgrading).

### New

- **Export a database** from its ⋮ menu on the Databases tab: the whole of it,
  the last 24 hours, 7 or 30 days, a calendar day or month, or a custom range,
  with a size estimate first. It downloads as a `.wtar` archive while ingest
  carries on.
- **Import an archive** as a new database (Import… beside Create), or in place
  of an existing one (its ⋮ menu). A failed import changes nothing, and frames
  arriving during a replace land in the new database afterwards.
- **Rotate the database password and the admin key** from Keys → Secrets, or
  `wiretap-backend rotate db-password|admin-key`. No superuser is needed and
  ingest carries on. The gateway app image saves the new values itself. **On
  compose, copy the new value into `.env` before the next restart.** A rotated
  admin key stays retired even if the gateway restarts with the old one.
- **Each API key has a ⋮ menu**: Edit (name and database pin), Rotate, Disable
  or Enable, and Revoke.
- The capture server's ApplianceOS UI can add TOTP with recovery codes and
  passkeys to an account.

### Changed

- **`wiretap` is the gateway's own database and takes no captures.** It is no
  longer listed, and a capture server that names no database is refused unless
  its key is pinned to one; the server's log says why. Capture rows an older
  gateway put in `wiretap` stay where they are but are no longer served, and
  the gateway logs how many at each start.
- **Revoke and Restore are now Disable and Enable**, and **Revoke now deletes a
  key for good**.
- Behind an ApplianceOS console, issuing or destroying a credential, rotating a
  secret, deleting a database or replacing one by import asks for the password
  confirmed in the last five minutes.
- The Logging tab redacts the credentials it can recognise.

### Upgrading

1. **Before upgrading the gateway, count the frames in its `wiretap`
   database.** If it holds captures you still need, copy them out with
   `pg_dump` first: `0.1.13` leaves them in place but no longer serves them.
2. **Set `[forward] database` on every capture server** that forwards to it,
   unless its API key is pinned to a database, and restart it.
3. Upgrade the gateway. The capture servers can stay on `0.1.12`; upgrade them
   for the clearer refusal message.

---

**Packages:** `wiretap-server_0.1.13_amd64.deb`, `wiretap-server_0.1.13_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.13`

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
