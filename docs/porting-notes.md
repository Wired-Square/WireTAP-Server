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

### `--version` exists, and names the commit

The Python had no `--version` — argparse rejects it with a usage error. The Rust
server reports `wiretap-server 0.1.0 (g4bf526d44891)` and logs the same string as
the journal's first line, before any complaint about the configuration.

The commit half is there because the version alone cannot identify a build.
Every `0.1.0` package, file name, `dpkg-query -W` answer and `--version` string
is the same one, so a report that a binary ran clean for a week names nothing —
which was found by hashing the binary soaking on the trial box against the one
in the first release candidate and getting two different answers, both
`Version: 0.1.0`.

`build.rs` reads the commit from git and marks a tree with uncommitted changes
`-dirty`. git wins whenever there is a checkout to ask: `WIRETAP_BUILD_ID` is
the *fallback* for a tree with no `.git`, such as a `git archive` export or a
container build, and does not override a resolvable HEAD — a stray exported
variable silently outranking ground truth is the failure this exists to
prevent, not to add. Failing both it reports `unknown`, and
`packaging/make-deb.sh` refuses to package a binary that says so.

The package version and the gateway's `/v1/health` carry the same identity, for
the same reason; the sort-order rules behind the package version are in the
README, and are packaging rather than porting.

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

Two carry a different number, and are the one deviation from verbatim.
`drained N frames from queue to disk cache` and `shutdown: flushed N frames to
disk cache` both took `N` from the queue in the Python — `_write_to_cache`
returns nothing, so `_drain_queue_to_cache` adds `len(batch)` whether the write
succeeded or not. When the cache cannot be written that prints an `info` line
saying frames were drained immediately after the `error` line saying they were
dropped, and the reassuring one is the one an operator remembers. Here
`cache_batch` returns what it stored and both lines report that, saying nothing
when nothing landed. The counters were always right; only these two lines were
not. `disk cache write error:` now names the count it dropped, as the
cache-full message beside it already did. Found by installing the `.deb` on a
Debian host, where a purge and a reinstall left the cache owned by a uid the
daemon no longer had.

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

### A forward batch is based on its earliest frame, not its first

The Python's ingest client took `chunk[0]`'s timestamp as a batch's base and
clamped every delta with `max(0, ...)`. The port replicated both, and the
combination is lossy: two bus readers feed one archive queue, so a chunk's
first frame is **not** always its earliest, and every frame older than the head
was encoded with `delta_us = 0` — written at the head's time, moved forward,
and duplicating a timestamp that belonged to another frame.

Found by the parallel run on 2026-09-06, and it presented as a capture problem
rather than a wire-format one. Over one minute, `sungrow_multi_tower` and
`sungrow_parallel_py` agreed on rows per bus (9 043 and 5 941), on the payload,
`dlc` and flag digests, and disagreed on `ts`: 14 978 distinct against 14 981.
`SELECT ts FROM rust EXCEPT SELECT ts FROM python` was **empty** — the Rust
invented no value the oracle had not also recorded, which rules out clock skew,
rounding and truncation — while the three values only the oracle held each sat
4–37 µs *below* a Rust duplicate pair spanning two buses. In one, the oracle has
bus 1 / id `108` at `.736102` and bus 0 / id `1814faa1` at `.736139`; the Rust
has both at `.736139`, with the same `ingest_ts`, so one batch, head on bus 0.

Three duplicates against **19** inverted pairs in the same minute, which is the
number that makes the mechanism legible: only a frame older than its chunk's
*head* saturates, not every frame out of order with its predecessor. Per-bus
inversions were 0, as they always are — one reader, one socket, one clock.

`ForwardSink::send_chunk` now takes the chunk's minimum, and `write_batch`
splits on span as well as on the record count, because a delta is a `u32` of
microseconds and a disk-cache drain reads `ORDER BY id` across a whole outage —
on a bus averaging a frame every 17 seconds, 256 consecutive cached frames
reach 71.6 minutes and the delta would *wrap* rather than saturate.
`encode_record_into` `debug_assert`s the base, as a backstop for a hand-rolled
caller rather than as the mechanism. Pinned by
`an_out_of_order_chunk_keeps_every_timestamp` and
`a_batch_spanning_more_than_a_u32_of_microseconds_is_split`, each of which
fails against the code it replaced and passes against the other's mutation.

The Python is unchanged and still has this bug — `wiretap-server.py:1206` takes
`chunk[0]` as the base and `:1216` clamps with `max(0, ...)`. It did not show in
the parallel run because that run was configured to use the oracle's *other*
writer, the direct `copy_expert` at `:519`, which has no base at all. That is a
fact about how the run was set up rather than about either codebase, and is not
checkable from this repository.

The same belief — that a batch arrives in order — sat in both `TIME_RELATIVE`
re-basing sites, which took `records.last()` as the newest record and stamped
it at arrival. A sender interleaving two buses can end a batch with a frame
that is not its newest, and the real newest was then dated ahead of the arrival
it was pinned to. Both now take the largest delta. Milder than the base defect
— the whole batch shifts uniformly, so nothing collapses or duplicates — and
fixed here because leaving the belief in two more places is how the next one
starts. Pinned by `relative_timestamps_are_back_dated_from_arrival`, which now
sends its deltas out of order; it passed either way before.

### An FD frame is padded up to its data length code

Taken with `wiretap-protocol` `v0.15.2`. The Python emits `data[:data_len]`
after packing the *code* into the low nibble, so a CAN FD payload whose length
is not an exact DLC — 9 bytes, say, which rounds up to code 9 meaning 12 —
tells the client twelve bytes and sends nine. A client that trusts the code
then reads three bytes of the next frame. The codec now zero-pads to the code's
length.

**No byte this server emits changes**, and the guarantee is stronger than the
bus. `read_loop` is the only sender into the GVRET broadcast, so every payload
reaching the encoder arrived through `CanFdFrame::from(canfd_frame)`, which
*normalises*: it clamps to `CANFD_MAX_DLEN`, rounds the length up to the next
valid one, zero-fills, and rewrites `len`. So `data()` cannot return an inexact
length even if a kernel handed one over, and the padding here is always zero
bytes long. Frames arriving on the ingest listener, which is where an arbitrary
length could come from, are enqueued to the archive and never broadcast.

The entry is here because the codec is shared: the consumer that *can* reach it
is the desktop, and a reader comparing the two implementations would otherwise
find an undocumented difference. Pinned upstream by
`fd_frame_with_an_inexact_length_is_padded_up_to_its_code`.

## Replicated quirks

Each is pinned by a test that names it.

### `E7 E7` inside a payload is swallowed, even in binary mode

The Python rescans the whole receive buffer for the `E7 E7` handshake on every
read, including after binary mode is latched, deleting everything up to and
including each occurrence. A `F1 00` transmit command whose CAN payload happens
to contain the bytes `E7 E7` therefore loses them and desynchronises the
stream.

Replicated in `Decoder::scan_for_handshake`. Test:
`gvret::tests::sync_bytes_are_consumed_even_in_binary_mode`.

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
`gvret::tests::an_overlong_declared_length_consumes_all_of_it`.

Clamping the consume instead would be more obviously correct — but then the two
implementations desynchronise *differently* on the same malformed input, and the
parallel run that is meant to earn the cutover would diverge for reasons that
have nothing to do with the port.

## Confirmed identical

Recorded so a future reader does not go looking.

- **The GVRET reply bytes.** Device info, bus parameters, bus count, timebase
  and keepalive are asserted against golden byte vectors taken from the Python
  (`gvret::tests`). Device info's constants — build 400, EEPROM version
  1 — are advertised values with no derivation; they are what clients parse.
- **The FD data length code in the low nibble.** The byte packing bus and DLC
  carries the *code*, not the byte count, so 32 bytes is 13 and 64 bytes is 15.
  The desktop's parser depends on this. Test:
  `fd_frame_packs_the_dlc_code_not_the_length`.
- **Kernel receive timestamps, at the socket.** `socketcan` asks for
  `SO_TIMESTAMPNS` where the Python asked for `SO_TIMESTAMP` — nanosecond
  timespec against microsecond timeval, the same kernel software receive time,
  truncated to microseconds either way. One `recvmsg` per frame in both.

  **Identical at the socket only** — this entry was read for weeks as covering
  the timestamp end to end. It does not: the forward encoding was where the two
  archives diverged, under "A forward batch is based on its earliest frame"
  above.

  Two things at the socket are *not* identical, neither of which caused that:

  - **A missing control message is an error here and a fallback there.**
    `wiretap-server.py` computes `now = time.time()` for every frame and
    overwrites it only if the `SO_TIMESTAMP` cmsg is present, so a socket that
    stopped delivering them would silently archive a userspace clock.
    socketcan 3.6.2's `read_frame_with_timestamp` returns
    `InvalidData: no SO_TIMESTAMPNS control message received`; `read_loop` logs
    it and backs off a second, capturing nothing until it recovers. Loud is the
    better failure for an archive, but it is a difference, and it is the one
    branch here whose absence can only be observed in the field: the
    `debian-sungrow` journal carried zero such lines over the 2026-09-06 run.
  - **Same-microsecond frames are real and are not a defect.** Both archives
    show pairs sharing a microsecond on the *same* bus, which no 250 kbit/s bus
    can produce: the gs_usb driver stamps a USB completion, so frames delivered
    together carry one time. They agree on these, which is the point — "no
    duplicate timestamps" is the wrong success criterion, and "the same
    duplicates as the oracle" is the right one.
- **Bitrate detection, now without pyroute2.** `nl::CanInterface::details()`
  replaces the hand-rolled `IFLA_CAN_BITTIMING` parsing. The fallback matches
  the Python case for case, which is subtler than "any error yields
  (500_000, 0)": a *missing* nominal falls back to 500 kbit/s without
  discarding a data rate that was read, and a nominal of **zero** — an
  interface that is up but was never given a bitrate — is reported as zero
  rather than replaced by the fallback.

  Verified against real hardware on 2026-09-05, which the `vcan` drill cannot
  do: a virtual interface has no bit timing to report, so until then only the
  *fallback* branch had ever executed. Two gs_usb adapters on 250 kbit/s buses
  returned `rates=250000,250000`, and the bytes a GVRET client received were
  `f1 06 01 90 d0 03 00 01 90 d0 03 00` — the golden encoding for 250 k, where
  the fallback would have read `01 20 A1 07 00`. It also ran under the packaged
  unit's `RestrictAddressFamilies`, so `AF_NETLINK` there is now known to be
  necessary rather than assumed to be.
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
