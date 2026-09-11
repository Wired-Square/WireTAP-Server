# WireTAP Binary Ingest Protocol (v2)

A compact TCP protocol for pushing batches of captured frames — CAN, or the
messages a serial tap recovers — into wiretap-server from microcontroller-class
capture devices (ESP32, STM32, etc.). The server feeds them into the same
pipeline as local capture: batching, relay to the gateway, and the SQLite disk
cache for outage resilience. It is also the protocol the server speaks as a
*client* in `[forward]` mode, so one codec covers both ends.

Design priorities, in order: tiny client footprint (fixed little-endian
layouts pack directly as C structs — no varint or text encoding), bounded
buffer sizes known at compile time, and at-least-once delivery with explicit
backpressure.

A Python reference client and loopback test suite is
[tools/test_ingest_client.py](../tools/test_ingest_client.py). The server half
is `crates/wiretap-backend/src/ingest/`; a second implementation lived in the
retired Python server until 2026-09-10.

## Transport and framing

One TCP connection per device. **All integers are little-endian.** Every
message, in both directions, is framed as:

| offset | size | field    | notes                                          |
|--------|------|----------|------------------------------------------------|
| 0      | 2    | `length` | u16 — bytes from `type` to end of body (CRC excluded) |
| 2      | 1    | `type`   | u8 message type                                |
| 3      | N    | body     | `length - 1` bytes                             |
| 3+N    | 4    | `crc32`  | u32 — IEEE CRC-32 over `type` + body           |

Limits: `length` ≤ 65535, so a message is at most ~64 KiB on the wire. A
message that fails its CRC is not processed (a corrupt `BATCH` gets a
`status = 1` ACK so the client can resend; anything else is ignored).

Message types (high bit set = server → client):

| type   | name        | direction        |
|--------|-------------|------------------|
| `0x01` | `HELLO`     | client → server  |
| `0x81` | `HELLO_ACK` | server → client  |
| `0x02` | `BATCH`     | client → server  |
| `0x82` | `ACK`       | server → client  |
| `0x03` | `PING`      | client → server  |
| `0x83` | `PONG`      | server → client  |

Unknown message types are ignored by the server (forward compatibility).

## Session start: HELLO / HELLO_ACK

The client must send `HELLO` first; a `BATCH` before successful HELLO causes
the server to drop the connection.

`HELLO` body:

| offset | size | field           | notes                                  |
|--------|------|-----------------|----------------------------------------|
| 0      | 4    | magic           | ASCII `"WTAP"`                         |
| 4      | 1    | `proto_version` | 2                                      |
| 5      | 1    | `flags`         | bit 0 = `TIME_RELATIVE` (see below)    |
| 6      | 1    | `token_len`     | 0–255                                  |
| 7      | n    | token           | API key / shared secret, compared constant-time |
| 7+n    | 1    | `db_len`        | 0–63 (optional — absent means 0)       |
| 8+n    | m    | database        | target capture database name (`[a-z0-9_]+`) |

The database field selects which capture database the frames land in.
Empty (or absent, for older clients) means the server's default database.
Against the WireTAP backend gateway, an unknown database name is
**auto-created** (schema applied) when the server allows it and the API key
permits — so a freshly flashed ingestor for a new capture provisions its own
database on first connect. A key may be pinned to one database server-side;
HELLO naming any other database is rejected as bad auth. The standalone
Python wiretap-server accepts and logs the field but always writes to its
single configured DSN.

`HELLO_ACK` body:

| offset | size | field              | notes                                |
|--------|------|--------------------|---------------------------------------|
| 0      | 1    | `status`           | 0 = ok, 1 = bad auth, 2 = bad version, 3 = bad database (invalid name, or auto-create disabled) |
| 1      | 1    | `accepted_version` | server protocol version               |
| 2      | 8    | `server_time_us`   | u64 — server wall clock, epoch µs     |

On any non-zero status the server closes the connection after the ACK.
`server_time_us` lets a clock-capable device synchronise before sending
absolute timestamps.

**Versions.** A client speaks one version and there is no negotiation. A
server refuses a version it does not speak with `status = 2`, and
`accepted_version` says which it does, so the refused end can say which side
is behind. The gateway accepts version 1 as well as 2 **for one release**, so a
capture daemon that has not been upgraded keeps flowing through a gateway that
has; the capture daemon's own listener accepts 2 only.

## Frame delivery: BATCH / ACK

`BATCH` body:

| offset | size | field        | notes                                       |
|--------|------|--------------|----------------------------------------------|
| 0      | 4    | `seq`        | u32 — client-chosen, echoed in the ACK      |
| 4      | 8    | `base_ts_us` | u64 — epoch µs (0 when `TIME_RELATIVE`)     |
| 12     | 2    | `count`      | u16 — records that follow (≤ 256 default)   |
| 14     | …    | records      | `count` records, in any order               |

Each record:

| offset | size  | field         | notes                                       |
|--------|-------|---------------|----------------------------------------------|
| 0      | 4     | `delta_ts_us` | u32 — µs offset from `base_ts_us`           |
| 4      | 1     | `kind`        | u8 — 0 = CAN, 1 = Modbus; anything else is malformed |
| 5      | 1     | `flags`       | u8 — per kind, below                        |
| 6      | 1     | `bus`         | u8 — bus number (the device's interface index) |
| 7      | 2     | `len`         | u16 — payload length, capped per kind       |
| 9      | 4     | `id_flags`    | u32 — per kind, below; bit 31 is always dir (0 = rx, 1 = tx) |
| 13     | `len` | payload       | raw bytes                                   |

| kind | `id_flags`                                              | `flags`             | payload, `len`                       |
|------|---------------------------------------------------------|---------------------|--------------------------------------|
| 0 CAN    | bits 0–28 arbitration id, bit 29 extended, bit 30 FD | 0                   | the frame's data, 0–64               |
| 1 Modbus | bits 8–15 unit (slave address), bits 0–7 function code | bit 0 = CRC valid | the whole RTU message, CRC included, 0–256 |

A Modbus record's `id_flags` is also the `id` the archive stores, so an
inventory groups the archive by conversation — `0x0120` is unit 1, function
`0x20`. The tap that produces them transmits nothing, so its records carry
`dir = 0`.

Per-record overhead is 13 bytes; a classic 8-byte frame costs 21 bytes on the
wire. The u32 delta limits a batch's span to ~71 minutes — irrelevant in
practice since batches should be flushed at least every few seconds.

**Two limits bound a batch, and a client must split at whichever comes
first.** `count` is capped at 256 by default (`max_batch_frames`). The frame's
`length` field is a u16, so the body is at most 65 534 bytes — and 256
full-size Modbus records are 68 878, so a batch of long messages splits by
bytes before it reaches the count. Past the limit the length prefix wraps
and the receiver loses framing; nothing refuses it for you.

**Timestamps.** With an absolute clock (NTP, GPS, or synced from
`server_time_us`): set `base_ts_us` to epoch µs and deltas relative to it.
Without a clock: set the `TIME_RELATIVE` HELLO flag, use any monotonic µs
counter (e.g. µs since boot) as the delta base, and send `base_ts_us = 0`.
The server stamps the **newest** record in the batch — the largest delta,
wherever it sits — with its arrival time and back-dates the others by their
delta differences: accurate to within network latency, with correct
inter-frame spacing. A device interleaving two buses need not sort.

`ACK` body:

| offset | size | field       | notes                                          |
|--------|------|-------------|--------------------------------------------------|
| 0      | 4    | `seq`       | u32 — echoes the BATCH seq                      |
| 4      | 1    | `status`    | 0 = ok (durably stored), 1 = CRC error, 2 = malformed, 3 = server can't store now (database unavailable) |
| 5      | 1    | `queue_pct` | u8 — reserved, always 0 (the gateway writes synchronously and keeps no queue) |

**ACK-after-write.** The gateway writes each batch to PostgreSQL **before**
replying. A `status = 0` ACK therefore means the frames are durably stored, not
merely buffered — there is no in-gateway queue that could be lost on a restart.
If the database is unavailable the gateway replies `status = 3`, which the
client treats as "retry later" (cache and back off).

**Delivery semantics (at-least-once).** Keep each batch buffered until its
seq is ACKed with status 0. Resend on: status 1/2 (after checking the
encoder for status 2), status 3 (after a backoff), no ACK within a timeout, or
reconnect. Occasional duplicate frames from resends are acceptable in the
archive; exact-once is deliberately not attempted. Sequence numbers only
correlate ACKs to batches — they need not be contiguous and the server does
not deduplicate.

## Keepalive: PING / PONG

Both bodies are empty. Send `PING` at the configured interval
(`keepalive_secs`, default 30 s) whenever no batches are flowing; the server
drops connections silent for 3× that interval. Any received message counts
as activity, so a busy device never needs to ping.

## Server configuration

```toml
[ingest]
enable = true
host = "0.0.0.0"
port = 9323
token = "CHANGE_ME"      # or env WIRETAP_INGEST_TOKEN; empty disables auth
keepalive_secs = 30
max_batch_frames = 256
```

Requires a configured sink — `[forward]` to a gateway. (The Python
implementation also accepted `[postgres].enable = true`, writing to a database
directly; the Rust port drops that path, so ingested frames are always relayed
onward.) Set `[server].iface = ""` for an ingest-only deployment with no local
CAN hardware. The token is sent in clear text — deploy on a trusted network or
wrap the connection in a VPN / stunnel if it crosses untrusted segments.

## Sizing guidance for clients

A worst-case CAN batch (256 × classic CAN, 8-byte payloads) is
`7 + 14 + 256 × 21 = 5397` bytes — one static buffer. With a 500 kbit/s bus
at full load (~4000 frames/s), flushing 256-frame batches means ~16 batches/s
≈ 84 KB/s of TCP traffic, comfortably inside ESP32 Wi-Fi capability. Flush
partial batches on a timer (e.g. 250 ms) so quiet buses still record promptly.
A device carrying Modbus messages needs a 64 KiB buffer, or a smaller
`count` of its own choosing.

---

## v1 → v2

Version 1 carried CAN only: a 10-byte record of `delta_ts_us u32 | id_flags
u32 | bus u8 | len u8`, with every bit of `id_flags` allocated and a payload of
at most 64 bytes. Version 2 changed the `0x02 BATCH` record in place rather
than adding a message type beside it: a `kind` byte says what the record is,
`flags` carries what the kind needs, and `len` grew to a u16 so a Modbus RTU
message fits whole. A CAN record's `id_flags` is bit-identical to v1's.

**Upgrade order is gateway first, then capture daemons.** The gateway accepts
both versions for one release, so a daemon still on v1 keeps flowing between
the two steps; a v2 daemon against a v1 gateway is refused at the handshake
and caches to disk until the gateway is upgraded. The daemon's log names both
versions when that happens.
