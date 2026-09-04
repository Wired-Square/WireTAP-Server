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

Three flags went with it — `--pg-dsn`, `--pg-func`, `--pg-write-mode` — and are
warned about and ignored. **The other seven did not.** `ForwardSink` subclasses
`PostgresWriter`, so `main` hands a forward deployment `--pg-batch-size`,
`--pg-flush-interval`, `--pg-queue-size`, `--pg-dir`, `--pg-cache-path`,
`--pg-cache-max-mb` and `--pg-queue-flush-pct` exactly as it hands them to a
PostgreSQL one: they configure the batcher and the disk cache, which outlive the
sink they are named after. Treating them as retired would tell an operator their
queue size had stopped applying, which is the reverse of the truth.

`--pg-dir` is the one with teeth today. `main` resolves
`args.pg_dir or args.default_dir` *after* the config merge, so it sets the
direction every frame is tagged with — above `--default-dir` and above
`[server].default_dir`. It is now validated, where the Python would have written
any string it was given straight into the archive's `dir` column.

This also removes `tokio-postgres` and its TLS surface from the server
entirely, which is what keeps the `.deb` a static musl binary with no libpq.

### The GVRET fan-out no longer blocks the capture

The Python held a lock and did a blocking `sendall` per connected client
*inside* the capture loop, so one stalled SavvyCAN back-pressured archiving for
everyone — a viewer could cost frames that were meant to be recorded.

Each client now owns a task and a `broadcast::Receiver`. A client that stops
reading lags its own subscription; the frames it missed are counted and logged
(`Client 10.0.0.4:51000 is not keeping up, dropped 812 frames`) and the capture
side never waits for it. Losing frames on a live monitor protocol is the right
trade; losing them from the archive was not.

Two details follow from it rather than being separate decisions. Frames are
encoded per client, not once and shared, because the timestamp in each one
counts from that client's connection, exactly as the Python's per-connection
`t0` did — so the `F1 01` timebase a client reads back stays comparable with the
frames it is sent. And a burst that arrives while a client is being written to
leaves as a single `write` rather than one per frame.

**`--echo-console` is a subscriber on the same channel**, for the same reason.
The Python wrote its console line from inside the capture loop, so a terminal
over a slow SSH link or a pipe into `less` back-pressured the capture exactly as
a stalled client did. It can now drop lines, and logs how many.

### The ingest listener refuses to run without a gateway

The Python exited rather than start with `--ingest-enable` and no PostgreSQL
sink, because a device's frames would have had nowhere to go. The same
requirement now names `[forward]`: accepting a batch, acknowledging it, and
then dropping it is the one thing at-least-once delivery must never do, and a
device has no way to tell that happened.

Two smaller things follow the Python exactly. An empty token still disables
authentication, which is what a closed private network relies on. And the
`database` a client names in its `HELLO` is logged and not honoured — the
Python said the same about its single DSN, and here the gateway is what routes
a database.

### The batcher's log messages are kept, including the word "database"

`PostgresWriter`'s worker said things like `database unavailable, caching
frames to disk` and `draining 5000 cached frames to database`. With
forward-to-gateway as the only sink there is no database in the server's world
any more — but the shipped `wiretap-server.toml` tells operators to grep for
these lines, so every message at `info` and above is verbatim. A better word
would cost more than it is worth.

Three `debug` lines are gone, being progress chatter the Rust states once
(`cache check: …`, `read_batch returned …`, `executing batch insert …`), and
three are new, all naming a disk-cache failure the Python swallowed: a failed
read, delete or reset.

### A cache left in `$HOME` is taken over, then removed

Because the default moved, an upgrade would otherwise strand whatever an outage
had put in `~/.wiretap-server-cache.db`. At startup, before a frame is enqueued,
that file becomes the cache: on the ordinary upgrade — an empty destination
created moments earlier — it is a `rename`, and otherwise its rows are copied a
thousand at a time, deleted as they go so an interrupted transfer resumes rather
than duplicating. Either way the old file is then removed.

Doing it *first* is what keeps the archive's order intact: those frames are
older than anything this run will capture, and the batcher already sends the
cache ahead of the queue. `--check-config` reports the file it is about to take
over, so an upgrade can be inspected before it happens.

### The disk cache counts its write-ahead log

`size_bytes()` stat'd the database file alone. In WAL mode — which the Python
set — frames land in `cache.db-wal` and stay there until a checkpoint, so during
an outage, exactly when the limit matters, the cache under-reported its own size
and sailed past `cache_max_mb`. The three files are added up now.

Everything else about the cache is the Python's: the same table, the same column
types, the same `journal_mode=WAL` and `synchronous=NORMAL`. That is not
conservatism, it is the requirement — an existing Pi has a populated cache and
an upgrade *during* an outage has to drain it. Both directions are tested: a
database the Python wrote is read back frame for frame
(`cache::tests::a_cache_the_python_wrote_reads_back_intact`, built from a
verbatim `sqlite3 .dump`), and the Python was run against a database this wrote
to confirm it reads and drains it.

### The batcher is configured under `[forward]`, and the cache moves

Those six settings — batch size, flush interval, queue size, cache path, cache
size limit, spill threshold — lived under `[postgres]` in the Python, because
the forward sink subclassed the PostgreSQL writer and inherited its queue. But
`apply_config_overrides` read that section only when `enable = true`, and this
server refuses to start in exactly that case, so under `[postgres]` they could
never take effect at all. They are `[forward]` keys now. A migrated file's
copies stay where they are and stay ignored, which is what the Python did with
them once the sink was off.

The **default cache path** moves from `~/.wiretap-server-cache.db` to
`$STATE_DIRECTORY/cache.db` when systemd provides one. The Python opened the
`$HOME` path unconditionally while the shipped unit sets
`ProtectHome=read-only`, so archiving under that unit could not have worked —
anyone doing it had edited the unit or was not using it. Without a state
directory the Python's path is still what is used, so a hand-run upgrade finds
an existing cache rather than starting an empty one beside it. `PG_CACHE_PATH`
is unchanged and still beats both.

### A bus offset no longer breaks `F1 06`

`bus_count` is `bus_offset + interfaces`, and `reply_canbus_params` advertised
two buses whenever that reached 2 — then indexed `bus_speeds`, which only ever
had one entry per interface. So `--bus-offset 1` with a single interface raised
`IndexError` inside the client thread, killing the connection of any client that
asked for bus parameters. Verified by calling the method directly on the oracle.

The Rust reports a speed of zero for a bus it has no interface for. This is a
place a byte comparison legitimately disagrees: the Python sent nothing at all.

### Remote-transmission frames are dropped

An RTR frame carries a data length code but no data. The Python passed them
through to GVRET clients and to the archive, where the payload came out as
whatever the receive buffer held — in practice zeros. The Rust skips them.

Skipped in `CanReader::recv`. This is a **behaviour change**, listed here
because the rule at the top of this file requires it: replicating the Python
would mean archiving a frame whose payload is an artefact of a buffer, not of
the bus.

Pinned by `a_remote_frame_is_skipped_and_does_not_stall_the_reader` in
`tests/vcan_loopback.rs`, which puts a remote frame on the bus ahead of a data
frame and requires the data frame to arrive with the remote frame never
surfacing — so it checks the skip and that skipping did not swallow what
followed. It needs a real `vcan`, so it runs in CI rather than on a
development machine.

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

### `--echo-console` cannot show BRS or ESI

The Python printed its console line from the raw frame bytes, so it could tag a
CAN FD frame with `B` for bit rate switch and `E` for error state indicator.
The Rust prints from a `CanSample`, which carries neither: nothing else in the
pipeline has anywhere to put them — the ingest protocol has no spare bit and the
archive has no column — so carrying them would be dead weight everywhere but
this one diagnostic line.

The `R` and `!` tags are gone for a different reason: remote frames are dropped
and error frames never arrive, both above.

## Planned changes, not yet implemented

Listed so they are not mistaken for regressions when they land.

*(Nothing outstanding: the ingest listener was the last of these, and it
landed.)*
