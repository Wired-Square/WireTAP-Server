# Porting notes: Python → Rust

Where the Rust capture server behaves differently from the Python one it
replaces, and why. The Python is kept runnable at [tools/oracle/](../tools/oracle/)
as the reference the port is tested against, so every entry here is a place
where a byte-for-byte comparison would legitimately disagree — or would agree
for a reason worth writing down.

Two rules govern this file. **A behaviour is replicated, not improved, unless
the change is listed here.** And a quirk that looks like a bug is replicated
anyway if the acceptance gate is byte-equality; fixing it is a separate,
deliberate change made after cutover, not a silent one made during a port.

## Deliberate changes

### The direct-to-PostgreSQL sink is not ported

The Python could write frames to PostgreSQL itself (`[postgres].enable`,
`COPY` or the legacy `ingest_can_frame` function). The Rust server forwards to
a gateway and nothing else — the gateway owns the database, as its own
documentation always said.

A config with `[postgres].enable = true` makes the server **exit rather than
start**. Ignoring the setting would present a working capture that archives
nothing, which is worse than a daemon that will not run. `tools/migrate_to_timescale.py`
moves an existing archive.

This also removes `tokio-postgres` and its TLS surface from the server
entirely, which is what keeps the `.deb` a static musl binary with no libpq.

### Remote-transmission frames are dropped

An RTR frame carries a data length code but no data. The Python passed them
through to GVRET clients and to the archive, where the payload came out as
whatever the receive buffer held — in practice zeros. The Rust skips them.

Skipped in `CanReader::recv`. This is a **behaviour change**, listed here
because the rule at the top of this file requires it: replicating the Python
would mean archiving a frame whose payload is an artefact of a buffer, not of
the bus.

Error frames are *not* a deviation, despite looking like one in the same match
arm. Neither implementation sets `CAN_RAW_ERR_FILTER`, and the kernel's default
error mask is zero, so no error frame is ever delivered to either. If bus-health
reporting is wanted later it needs the socket option first; discarding them is
not what stops them arriving.

## Replicated quirks

Each is pinned by a test that names it.

### `E7 E7` inside a payload is swallowed, even in binary mode

The Python rescans the whole receive buffer for the `E7 E7` handshake on every
read, including after binary mode is latched, deleting everything up to and
including each occurrence. A `F1 00` transmit command whose CAN payload happens
to contain the bytes `E7 E7` therefore loses them and desynchronises the
stream.

Replicated in `Decoder::scan_for_handshake`. Test:
`gvret::codec::tests::sync_bytes_are_consumed_even_in_binary_mode`.

Host-to-device transmit is rare, which is presumably why this has never been
noticed. Worth fixing after cutover by scanning only while not yet in binary
mode — against a Rust server already known-good, rather than smuggled into the
port.

### An over-long transmit length consumes what it declared

`F1 00` carries a declared payload length. The Python uses the **declared**
value to decide how many bytes to consume from the buffer, then clamps the
payload it actually reads to 8. A client declaring 10 therefore has 18 bytes
consumed and contributes an 8-byte frame.

Replicated in `Decoder::take_transmit`. Test:
`gvret::codec::tests::an_overlong_declared_length_consumes_all_of_it`.

Clamping the consume instead would be more obviously correct — but then the two
implementations desynchronise *differently* on the same malformed input, and the
parallel run that is meant to earn the cutover would diverge for reasons that
have nothing to do with the port.

## Confirmed identical

Recorded so a future reader does not go looking.

- **The GVRET reply bytes.** Device info, bus parameters, bus count, timebase
  and keepalive are asserted against golden byte vectors taken from the Python
  (`gvret::codec::tests`). Device info's constants — build 400, EEPROM version
  1 — are advertised values with no derivation; they are what clients parse.
- **The FD data length code in the low nibble.** The byte packing bus and DLC
  carries the *code*, not the byte count, so 32 bytes is 13 and 64 bytes is 15.
  The desktop's parser depends on this. Test:
  `fd_frame_packs_the_dlc_code_not_the_length`.
- **Kernel receive timestamps.** `socketcan` asks for `SO_TIMESTAMPNS` where
  the Python asked for `SO_TIMESTAMP` — nanosecond timespec against microsecond
  timeval, the same kernel software receive time, truncated to microseconds
  either way. One `recvmsg` per frame in both.
- **Bitrate detection, now without pyroute2.** `nl::CanInterface::details()`
  replaces the hand-rolled `IFLA_CAN_BITTIMING` parsing. The fallback matches
  the Python case for case, which is subtler than "any error yields
  (500_000, 0)": a *missing* nominal falls back to 500 kbit/s without
  discarding a data rate that was read, and a nominal of **zero** — an
  interface that is up but was never given a bitrate — is reported as zero
  rather than replaced by the fallback.
- **Binary-mode resync.** Leading bytes that cannot start a command are skipped
  rather than stalling the connection — deliberate stream recovery in the
  original, kept. The Rust skips to the next candidate in one step where the
  Python deletes a byte at a time; same result, and the Python's form is
  quadratic on a buffer of noise.
- **Config parsing.** The shipped `wiretap-server.toml` parses unchanged with no
  unknown keys, and a key the file does not mention arrives as `None` rather
  than as a default — the distinction the config-over-CLI merge depends on.
  Tests in `wiretap_model::config`.

## Planned changes, not yet implemented

Listed so they are not mistaken for regressions when they land.

- **GVRET fan-out becomes non-blocking.** The Python holds a lock and does a
  blocking `sendall` per client inside the capture loop, so one stalled
  SavvyCAN back-pressures archiving for everyone. The Rust server will use a
  broadcast channel with one subscriber task per client and log
  `RecvError::Lagged(n)`. A slow viewer will no longer cost archived frames;
  it will drop its own instead, which is the right trade for a lossy monitor
  protocol.
