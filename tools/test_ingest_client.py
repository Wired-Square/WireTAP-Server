#!/usr/bin/env python3
"""
Reference client and conformance suite for the WireTAP binary ingest protocol
(docs/ingest-protocol.md). Always against a running server, over a real socket.

Conformance mode walks the protocol's edge cases — bad CRC, malformed batch,
oversize payload, HELLO ordering:

  ./test_ingest_client.py --host 127.0.0.1 --port 9323 --token SECRET --conformance

Live mode sends synthetic batches, which can then be checked in the archive:

  ./test_ingest_client.py --host pi.local --port 9323 --token SECRET --count 200

There was an in-process self-test mode that stood up the Python server's own
IngestTcpServer with a stub writer. It went with that server on 2026-09-10; the
conformance mode covers the same cases against whichever server is listening.
"""

import argparse
import socket
import struct
import sys
import time
import zlib


# ---------------------------------------------------------------------------
# Reference client
# ---------------------------------------------------------------------------
MSG_HELLO, MSG_BATCH, MSG_PING = 0x01, 0x02, 0x03
MSG_HELLO_ACK, MSG_ACK, MSG_PONG = 0x81, 0x82, 0x83
FLAG_TIME_RELATIVE = 0x01

ID_EXTENDED = 1 << 29
ID_FD = 1 << 30
ID_TX = 1 << 31


def frame_message(mtype: int, body: bytes = b"", corrupt_crc: bool = False) -> bytes:
    payload = bytes([mtype]) + body
    crc = zlib.crc32(payload) & 0xFFFFFFFF
    if corrupt_crc:
        crc ^= 0xDEADBEEF
    return (len(payload).to_bytes(2, "little") + payload
            + crc.to_bytes(4, "little"))


def encode_record(delta_us: int, arb_id: int, payload: bytes,
                  extended=False, fd=False, tx=False, bus=0) -> bytes:
    id_flags = (arb_id & 0x1FFFFFFF)
    if extended:
        id_flags |= ID_EXTENDED
    if fd:
        id_flags |= ID_FD
    if tx:
        id_flags |= ID_TX
    return struct.pack("<IIBB", delta_us, id_flags, bus, len(payload)) + payload


def encode_batch(seq: int, base_ts_us: int, records: list) -> bytes:
    return struct.pack("<IQH", seq, base_ts_us, len(records)) + b"".join(records)


class ReferenceClient:
    """Minimal protocol implementation, also the contract for MCU firmware."""

    def __init__(self, host: str, port: int, token: str = "",
                 database: str = "", time_relative: bool = False,
                 timeout: float = 5.0):
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.buf = bytearray()
        self.token = token.encode()
        self.database = database.encode()
        self.time_relative = time_relative

    def close(self):
        self.sock.close()

    def send_raw(self, data: bytes):
        self.sock.sendall(data)

    def recv_message(self):
        """Return (type, body) of the next message, or None on EOF/timeout."""
        while True:
            if len(self.buf) >= 2:
                length = int.from_bytes(self.buf[0:2], "little")
                total = 2 + length + 4
                if len(self.buf) >= total:
                    payload = bytes(self.buf[2:2 + length])
                    crc = int.from_bytes(self.buf[2 + length:total], "little")
                    del self.buf[:total]
                    assert (zlib.crc32(payload) & 0xFFFFFFFF) == crc, \
                        "server sent bad CRC"
                    return payload[0], payload[1:]
            try:
                chunk = self.sock.recv(4096)
            except socket.timeout:
                return None
            if not chunk:
                return None
            self.buf.extend(chunk)

    def hello(self):
        """Send HELLO; return (status, accepted_version, server_time_us)."""
        flags = FLAG_TIME_RELATIVE if self.time_relative else 0
        body = (b"WTAP" + bytes([1, flags, len(self.token)]) + self.token
                + bytes([len(self.database)]) + self.database)
        self.send_raw(frame_message(MSG_HELLO, body))
        mtype, ack = self.recv_message()
        assert mtype == MSG_HELLO_ACK, f"expected HELLO_ACK, got {mtype:#x}"
        return struct.unpack("<BBQ", ack)

    def send_batch(self, seq: int, records: list, base_ts_us: int = 0,
                   corrupt_crc: bool = False):
        """Send a BATCH; return (seq, status, queue_pct) from the ACK."""
        body = encode_batch(seq, base_ts_us, records)
        self.send_raw(frame_message(MSG_BATCH, body, corrupt_crc=corrupt_crc))
        mtype, ack = self.recv_message()
        assert mtype == MSG_ACK, f"expected ACK, got {mtype:#x}"
        return struct.unpack("<IBB", ack)

    def ping(self) -> bool:
        self.send_raw(frame_message(MSG_PING))
        mtype, _ = self.recv_message()
        return mtype == MSG_PONG


def conformance(host: str, port: int, token: str, database: str) -> int:
    """Protocol conformance against a LIVE server (Python or Rust gateway).
    Exercises the protocol against a running server, over a real socket."""
    failures = 0

    def check(name, cond):
        nonlocal failures
        status = "ok" if cond else "FAIL"
        if not cond:
            failures += 1
        print(f"  [{status}] {name}")

    base_us = int(time.time() * 1_000_000)
    rec = encode_record(0, 0x123, b"\x01\x02\x03")

    # bad token rejected and connection closed
    c = ReferenceClient(host, port, token="definitely-wrong", timeout=3.0)
    status, _, _ = c.hello()
    check("bad token rejected", status == 1)
    check("connection closed after bad auth", c.recv_message() is None)
    c.close()

    # bad protocol version rejected
    c = ReferenceClient(host, port, token=token, timeout=3.0)
    body = b"WTAP" + bytes([99, 0, len(token.encode())]) + token.encode()
    c.send_raw(frame_message(MSG_HELLO, body))
    mtype, ack = c.recv_message()
    check("bad version rejected", mtype == MSG_HELLO_ACK and ack[0] == 2)
    c.close()

    # BATCH before HELLO drops the connection
    c = ReferenceClient(host, port, token=token, timeout=3.0)
    c.send_raw(frame_message(MSG_BATCH, encode_batch(1, base_us, [rec])))
    check("batch before hello drops connection", c.recv_message() is None)
    c.close()

    # authenticated session: batch, CRC, malformed, ping
    c = ReferenceClient(host, port, token=token, database=database, timeout=5.0)
    status, version, server_us = c.hello()
    check("hello with database accepted", status == 0 and version == 1)
    check("server time plausible", abs(server_us / 1e6 - time.time()) < 10.0)

    seq, status, _ = c.send_batch(42, [rec], base_ts_us=base_us)
    check("absolute batch acked", (seq, status) == (42, 0))

    seq, status, _ = c.send_batch(43, [rec], base_ts_us=base_us, corrupt_crc=True)
    check("corrupt batch nacked with CRC status", (seq, status) == (43, 1))
    seq, status, _ = c.send_batch(43, [rec], base_ts_us=base_us)
    check("resend accepted", (seq, status) == (43, 0))

    c.send_raw(frame_message(MSG_BATCH, struct.pack("<IQH", 44, base_us, 5000)))
    mtype, ack = c.recv_message()
    check("oversized count nacked as malformed",
          mtype == MSG_ACK and struct.unpack("<IBB", ack)[1] == 2)

    check("ping/pong", c.ping())
    c.close()

    # TIME_RELATIVE session
    c = ReferenceClient(host, port, token=token, database=database,
                        time_relative=True, timeout=5.0)
    status, _, _ = c.hello()
    boot_us = 123_456_789
    records = [encode_record(boot_us, 0x200, b"\x01"),
               encode_record(boot_us + 50_000, 0x200, b"\x02")]
    seq, status, _ = c.send_batch(1, records)
    check("time-relative batch acked", status == 0)
    c.close()

    print(f"\n{'PASS' if failures == 0 else f'{failures} FAILURE(S)'}")
    return 1 if failures else 0


def live_send(host: str, port: int, token: str, count: int, database: str = ""):
    """Send `count` synthetic batches to a real server."""
    c = ReferenceClient(host, port, token=token, database=database)
    status, version, _ = c.hello()
    if status != 0:
        sys.exit(f"HELLO rejected: status={status}")
    print(f"connected (protocol v{version})")
    for i in range(count):
        base_us = int(time.time() * 1_000_000)
        records = [
            encode_record(j * 10_000, 0x7E0 + (j % 4),
                          struct.pack("<II", i, j), bus=0)
            for j in range(16)
        ]
        seq, status, pct = c.send_batch(i, records, base_ts_us=base_us)
        print(f"batch {seq}: status={status} queue={pct}%")
        time.sleep(0.2)
    c.close()
    print(f"sent {count} batches x 16 frames — check public.capture_frame "
          "for ids 0x7E0..0x7E3")


def main():
    ap = argparse.ArgumentParser(description="WireTAP ingest protocol test client")
    ap.add_argument("--host", help="Send to a live server instead of self-testing")
    ap.add_argument("--port", type=int, default=9323)
    ap.add_argument("--token", default="")
    ap.add_argument("--database", default="",
                    help="Target capture database (gateway routes/auto-creates)")
    ap.add_argument("--count", type=int, default=5, help="Batches in live mode")
    ap.add_argument("--conformance", action="store_true",
                    help="Run the protocol conformance suite against a live server")
    args = ap.parse_args()

    if not args.host:
        ap.error("--host is required; the in-process selftest went with the "
                 "Python server it drove")
    if args.conformance:
        sys.exit(conformance(args.host, args.port, args.token, args.database))
    live_send(args.host, args.port, args.token, args.count, args.database)


if __name__ == "__main__":
    main()
