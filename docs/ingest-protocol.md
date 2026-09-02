# WireTAP Binary Ingest Protocol (v1)

A compact TCP protocol for pushing batches of CAN frames into wiretap-server
from microcontroller-class capture devices (ESP32, STM32, etc.). The server
feeds frames into the same pipeline as local SocketCAN capture: batching,
relay to the gateway, and the SQLite disk cache for outage resilience. It is
also the protocol the server speaks as a *client* in `[forward]` mode, so one
codec covers both ends.

Design priorities, in order: tiny client footprint (fixed little-endian
layouts pack directly as C structs — no varint or text encoding), bounded
buffer sizes known at compile time, and at-least-once delivery with explicit
backpressure.

The reference server implementation is `IngestTcpServer` in
[tools/oracle/wiretap-server.py](../tools/oracle/wiretap-server.py);
a Python reference client and loopback test suite is
[tools/test_ingest_client.py](../tools/test_ingest_client.py).

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
| 4      | 1    | `proto_version` | 1                                      |
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

## Frame delivery: BATCH / ACK

`BATCH` body:

| offset | size | field        | notes                                       |
|--------|------|--------------|----------------------------------------------|
| 0      | 4    | `seq`        | u32 — client-chosen, echoed in the ACK      |
| 4      | 8    | `base_ts_us` | u64 — epoch µs (0 when `TIME_RELATIVE`)     |
| 12     | 2    | `count`      | u16 — records that follow (≤ 256 default)   |
| 14     | …    | records      | `count` records, ascending `delta_ts_us`    |

Each record:

| offset | size  | field         | notes                                       |
|--------|-------|---------------|----------------------------------------------|
| 0      | 4     | `delta_ts_us` | u32 — µs offset from `base_ts_us`           |
| 4      | 4     | `id_flags`    | bits 0–28 arbitration id, bit 29 extended, bit 30 FD, bit 31 dir (0 = rx, 1 = tx) |
| 8      | 1     | `bus`         | u8 — GVRET bus number                       |
| 9      | 1     | `len`         | u8 — payload length, 0–64                   |
| 10     | `len` | payload       | raw data bytes                              |

Per-record overhead is 10 bytes; a classic 8-byte frame costs 18 bytes on the
wire. The u32 delta limits a batch's span to ~71 minutes — irrelevant in
practice since batches should be flushed at least every few seconds.

**Timestamps.** With an absolute clock (NTP, GPS, or synced from
`server_time_us`): set `base_ts_us` to epoch µs and deltas relative to it.
Without a clock: set the `TIME_RELATIVE` HELLO flag, use any monotonic µs
counter (e.g. µs since boot) as the delta base, and send `base_ts_us = 0`.
The server stamps the **last** record in the batch with its arrival time and
back-dates the others by their delta differences — accurate to within network
latency, with correct inter-frame spacing. Records must therefore be in
ascending delta order.

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

A worst-case batch (256 × classic CAN, 8-byte payloads) is
`7 + 14 + 256 × 18 = 4629` bytes — one static buffer. With a 500 kbit/s bus
at full load (~4000 frames/s), flushing 256-frame batches means ~16 batches/s
≈ 74 KB/s of TCP traffic, comfortably inside ESP32 Wi-Fi capability. Flush
partial batches on a timer (e.g. 250 ms) so quiet buses still record promptly.

---

## Proposed: v2 typed batches

> **Not implemented. Do not write firmware against this section.** No server
> accepts `BATCH_TYPED` today and no client sends it. It is recorded here so the
> shape is settled before Modbus capture lands, and because the upgrade path
> constrains what v1 clients may assume. The v1 protocol above is frozen and
> stays supported indefinitely.

Modbus capture needs to reach the archive through the same pipeline as CAN —
same batching, same disk cache, same ACK-after-write. A v1 record cannot carry
it: the layout is fixed at 10 bytes plus payload, and every bit of `id_flags` is
already allocated (0–28 arbitration id, 29 extended, 30 FD, 31 direction), so
there is no spare bit to mean "this is a register, not a frame", and nowhere to
put a unit id, an address and a register kind.

### A new message type, not a new version

| type   | name          | direction       |
|--------|---------------|-----------------|
| `0x04` | `BATCH_TYPED` | client → server |

`0x02 BATCH` is **frozen** and keeps its exact v1 meaning forever. A device that
only captures CAN never needs updating.

`BATCH_TYPED` body:

| offset | size | field        | notes                                        |
|--------|------|--------------|----------------------------------------------|
| 0      | 4    | `seq`        | u32 — as v1, echoed in the ACK               |
| 4      | 8    | `base_ts_us` | u64 — as v1 (0 when `TIME_RELATIVE`)         |
| 12     | 1    | `kind`       | u8 — record layout: 0 = CAN, 1 = register    |
| 13     | 1    | `reserved`   | u8 — must be 0                               |
| 14     | 2    | `count`      | u16 — records that follow                    |
| 16     | …    | records      | `count` records, ascending `delta_ts_us`     |

One `kind` per batch rather than per record: a device is almost always one
protocol, it keeps per-record overhead at zero, and it lets the gateway write a
whole batch to one table. A mixed source flushes one batch per kind.

`kind = 0` records are **byte-identical to v1's**, so the encoder and parser are
shared rather than reimplemented. In practice a CAN-only device just keeps using
`0x02`.

`kind = 1` — Modbus register:

| offset | size  | field         | notes                                             |
|--------|-------|---------------|---------------------------------------------------|
| 0      | 4     | `delta_ts_us` | u32 — µs offset from `base_ts_us`, as v1          |
| 4      | 2     | `address`     | u16 — register / coil address                     |
| 6      | 1     | `unit`        | u8 — Modbus unit (slave) id                       |
| 7      | 1     | `reg_kind`    | u8 — 0 coil, 1 discrete input, 2 holding, 3 input |
| 8      | 1     | `bus`         | u8 — link index (which RTU port or TCP endpoint)  |
| 9      | 1     | `flags`       | u8 — bit 0 direction (0 = read, 1 = write)        |
| 10     | 1     | `len`         | u8 — value length in bytes                        |
| 11     | `len` | `value`       | raw bytes, big-endian as Modbus delivers them     |

11 bytes of overhead, so 13 for a 16-bit holding register. Size is not the
constraint here — register polling runs at tens of samples per second against
CAN's thousands.

`ACK` is unchanged and applies to `BATCH_TYPED` exactly as to `BATCH`.

### Upgrading without breaking deployed devices

Both server implementations check the version with **strict equality**, not as a
floor — `version != PROTO_VERSION` is rejected with `HELLO_BAD_VERSION`. So a
client that announces 2 is locked out of every server that has not been upgraded
first, and the hardest things in the fleet to reflash are exactly the
microcontrollers.

**So clients never bump the version they announce.** `HELLO_ACK` already carries
`accepted_version` at offset 1, and every server already populates it. Capability
discovery uses that field:

1. The client sends `HELLO` with `proto_version = 1`, as it always has.
2. The server replies `status = 0`, `accepted_version = 2`.
3. The client sends `0x04` **only** if that byte is ≥ 2; otherwise it stays on
   `0x02`.

This reads the version byte as *"the minimum version I require"* rather than
*"the version I speak"* — compatible with every client already deployed, which
all send 1, and every server, which all accept 1. No handshake change and no
reflashing.

**Ordering.** A server must accept `0x04` before any client emits it. Nothing
about `0x02` or the handshake changes, so v1 devices are unaffected by that
upgrade and keep working indefinitely.

**The trap.** Unknown message types are *ignored*, not rejected — deliberately,
for forward compatibility. A client that skips the `accepted_version` check and
sends `0x04` to a v1 server therefore gets no ACK at all and, under
at-least-once delivery, resends forever: a silent stall rather than a clean
error. Treat the check as a hard precondition, and cover it in the conformance
suite. Servers should also relax the equality test to reject only
`version > PROTO_VERSION`, so a client that does announce a future version
degrades instead of being refused outright.

### Still open

The wire format is half the question. Where a `kind = 1` record *lands* —
a `protocol` discriminator on `can_frame`, or a separate `modbus_register`
hypertable — is undecided, and it is one change spanning this protocol, the
gateway's ingest writer and `init_schema.sql`.
