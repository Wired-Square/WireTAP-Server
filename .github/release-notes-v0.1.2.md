## Highlights

A serial tap now stamps every message at the moment its last byte arrived.
Until now the messages the framer released together when it first synced —
after an open, or after a reopen — all carried the clock of the read that
released them, which on a slow line put the first ten or so messages after
every open up to six seconds late and on one timestamp. The tap remembers
when each read returned and stamps each message by the read that delivered
its last byte, so an archive no longer shows where the daemon was restarted.

### Changed

- **Serial tap timestamps** are the arrival of each message's last byte, to
  within one read, with the wire time of any bytes after it in the same read
  taken off; a stamp never reaches behind a read the tap had already seen
  return. The archive's `ts_us` for a line is therefore non-decreasing.
- The framer is asked for every function code with `wiretap-lib-rs` v0.16.5's
  `frame_any_function()` — identical framing to the release before, spelt
  once.

### Upgrading

`apt install ./wiretap-server_0.1.2_<arch>.deb` and the daemon restarts on its
existing configuration; nothing to change. The gateway is unchanged in
behaviour — its image is republished at `0.1.2` so the two halves stay on one
version — and a `0.1.1` gateway serves a `0.1.2` daemon without complaint.

**Known, and upstream:** where a single-register read response is followed by
a broadcast, one response in about 67 on a Sungrow line has a second reading
one byte longer that also passes its CRC, and it swallows the broadcast's
address byte — the broadcast is lost. Which reading wins depends on where the
serial read happened to end. The framer is `wiretap-lib-rs`'s and the fix
belongs there; until it lands, a tap's archive may hold fewer `0x0060` dispatch
and `0x0010` write broadcasts than the line carried.

---

**Packages:** `wiretap-server_0.1.2_amd64.deb`, `wiretap-server_0.1.2_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:0.1.2` (and `:latest`).

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
