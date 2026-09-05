//! Wire codec for the WireTAP binary ingest protocol (docs/ingest-protocol.md).
//! All integers little-endian; every message is
//! `len u16 | type u8 | body | crc32 u32` with the CRC over type+body.
//!
//! **Both ends live here**, which is the whole reason this is a crate. The
//! gateway parses what the capture server encodes, and the protocol was
//! hand-written four times before this existed — once here, twice in the Python
//! (server side and forward-client side), and again in the test client. A
//! format where one side is `<IIBB>` and the other is four `to_le_bytes` calls
//! is a format that drifts.
//!
//! Message types are grouped by who sends them, not by name: a client sends
//! `HELLO`, `BATCH` and `PING`; a server answers `HELLO_ACK`, `ACK` and `PONG`.

pub const PROTO_VERSION: u8 = 1;
pub const MAGIC: &[u8; 4] = b"WTAP";

pub const MSG_HELLO: u8 = 0x01;
pub const MSG_BATCH: u8 = 0x02;
pub const MSG_PING: u8 = 0x03;
pub const MSG_HELLO_ACK: u8 = 0x81;
pub const MSG_ACK: u8 = 0x82;
pub const MSG_PONG: u8 = 0x83;

pub const HELLO_FLAG_TIME_RELATIVE: u8 = 0x01;

pub const HELLO_OK: u8 = 0;
pub const HELLO_BAD_AUTH: u8 = 1;
pub const HELLO_BAD_VERSION: u8 = 2;
pub const HELLO_BAD_DATABASE: u8 = 3;

pub const ACK_OK: u8 = 0;
pub const ACK_CRC: u8 = 1;
pub const ACK_MALFORMED: u8 = 2;
pub const ACK_OVERLOADED: u8 = 3;

// The id word's layout, shared rather than declared: the WireTAP desktop's
// HTTP import record packs its arbitration id and flags exactly this way and
// nothing else the same, so the four constants are what crosses between the
// two repositories and the framing below is not.
pub use wiretap_protocol::ingest::{ID_ARB_MASK, ID_EXTENDED, ID_FD, ID_TX};

/// The largest payload a record can carry: one CAN FD frame. The length is a
/// single byte on the wire, but 64 is the real limit and both ends enforce it.
pub const MAX_PAYLOAD: usize = 64;

/// Records per `BATCH`. A client must chunk at this, because it is the default
/// a gateway checks against — over it, the batch is NACKed as malformed rather
/// than accepted and truncated.
pub const MAX_BATCH_RECORDS: usize = 256;

pub fn encode_message(mtype: u8, body: &[u8]) -> Vec<u8> {
    let len = (1 + body.len()) as u16;
    let mut out = Vec::with_capacity(2 + 1 + body.len() + 4);
    out.extend_from_slice(&len.to_le_bytes());
    out.push(mtype);
    out.extend_from_slice(body);
    let mut h = crc32fast::Hasher::new();
    h.update(&out[2..]);
    out.extend_from_slice(&h.finalize().to_le_bytes());
    out
}

/// One parsed wire frame: type, body, and whether the CRC matched.
pub struct WireFrame {
    pub mtype: u8,
    pub body: Vec<u8>,
    pub crc_ok: bool,
}

/// Try to consume one complete frame from the front of `buf`.
/// `Ok(None)` = need more bytes; `Err` = unrecoverable garbage (drop client).
pub fn take_frame(buf: &mut Vec<u8>) -> Result<Option<WireFrame>, String> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    if len < 1 {
        return Err("zero-length frame".into());
    }
    let total = 2 + len + 4;
    if buf.len() < total {
        return Ok(None);
    }
    let payload = &buf[2..2 + len];
    let crc = u32::from_le_bytes([buf[2 + len], buf[3 + len], buf[4 + len], buf[5 + len]]);
    let mut h = crc32fast::Hasher::new();
    h.update(payload);
    let frame = WireFrame {
        mtype: payload[0],
        body: payload[1..].to_vec(),
        crc_ok: h.finalize() == crc,
    };
    buf.drain(..total);
    Ok(Some(frame))
}

#[derive(Debug, PartialEq)]
pub struct Hello {
    pub version: u8,
    pub time_relative: bool,
    pub token: Vec<u8>,
    pub database: String,
}

pub fn parse_hello(body: &[u8]) -> Result<Hello, String> {
    if body.len() < 7 || &body[0..4] != MAGIC {
        return Err("bad magic".into());
    }
    let version = body[4];
    let flags = body[5];
    let token_len = body[6] as usize;
    if body.len() < 7 + token_len {
        return Err("truncated token".into());
    }
    let token = body[7..7 + token_len].to_vec();
    // Optional database field (absent for minimal clients = default db)
    let db_off = 7 + token_len;
    let database = if body.len() > db_off {
        let db_len = body[db_off] as usize;
        if body.len() < db_off + 1 + db_len {
            return Err("truncated database".into());
        }
        String::from_utf8_lossy(&body[db_off + 1..db_off + 1 + db_len]).into_owned()
    } else {
        String::new()
    };
    Ok(Hello {
        version,
        time_relative: flags & HELLO_FLAG_TIME_RELATIVE != 0,
        token,
        database,
    })
}

pub fn encode_hello_ack(status: u8, server_time_us: u64) -> Vec<u8> {
    let mut body = vec![status, PROTO_VERSION];
    body.extend_from_slice(&server_time_us.to_le_bytes());
    encode_message(MSG_HELLO_ACK, &body)
}

pub fn encode_ack(seq: u32, status: u8, queue_pct: u8) -> Vec<u8> {
    let mut body = Vec::with_capacity(6);
    body.extend_from_slice(&seq.to_le_bytes());
    body.push(status);
    body.push(queue_pct);
    encode_message(MSG_ACK, &body)
}

#[derive(Debug)]
pub struct Record {
    pub delta_us: u32,
    pub id_flags: u32,
    pub bus: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub struct Batch {
    pub seq: u32,
    pub base_ts_us: u64,
    pub records: Vec<Record>,
}

/// Parse a BATCH body. `Err(seq)` = malformed but seq was readable (NACK it);
/// outer Option None = too short to even carry a seq (drop client).
pub fn parse_batch(body: &[u8], max_frames: usize) -> Option<Result<Batch, u32>> {
    if body.len() < 14 {
        return None;
    }
    let seq = u32::from_le_bytes(body[0..4].try_into().unwrap());
    let base_ts_us = u64::from_le_bytes(body[4..12].try_into().unwrap());
    let count = u16::from_le_bytes(body[12..14].try_into().unwrap()) as usize;
    if count > max_frames {
        return Some(Err(seq));
    }
    let mut records = Vec::with_capacity(count);
    let mut off = 14;
    for _ in 0..count {
        if body.len() < off + 10 {
            return Some(Err(seq));
        }
        let delta_us = u32::from_le_bytes(body[off..off + 4].try_into().unwrap());
        let id_flags = u32::from_le_bytes(body[off + 4..off + 8].try_into().unwrap());
        let bus = body[off + 8];
        let plen = body[off + 9] as usize;
        off += 10;
        if plen > MAX_PAYLOAD || body.len() < off + plen {
            return Some(Err(seq));
        }
        records.push(Record {
            delta_us,
            id_flags,
            bus,
            payload: body[off..off + plen].to_vec(),
        });
        off += plen;
    }
    Some(Ok(Batch {
        seq,
        base_ts_us,
        records,
    }))
}

// --- client side -----------------------------------------------------------
//
// The inverse of everything above: what a capture server sends and what it
// reads back. Asserted against the server half in the tests, so neither can
// move without the other.

/// `HELLO` — announce the protocol version, authenticate, and name a database.
///
/// An empty `database` means the gateway's default, and a name it does not know
/// is created where the gateway permits it, so a new capture server can
/// provision its own database on first connect.
///
/// `time_relative` is false for everything here: the Python's forward client
/// sends absolute timestamps, and a batch carries the base itself.
pub fn encode_hello(token: &[u8], database: &str, time_relative: bool) -> Vec<u8> {
    let mut body = MAGIC.to_vec();
    body.push(PROTO_VERSION);
    body.push(if time_relative {
        HELLO_FLAG_TIME_RELATIVE
    } else {
        0
    });
    body.push(token.len() as u8);
    body.extend_from_slice(token);
    body.push(database.len() as u8);
    body.extend_from_slice(database.as_bytes());
    encode_message(MSG_HELLO, &body)
}

/// What a gateway said about a `HELLO`.
#[derive(Debug, PartialEq, Eq)]
pub struct HelloAck {
    pub status: u8,
    /// The highest version this gateway speaks.
    ///
    /// Both server implementations compare the announced version with strict
    /// equality, so a client that bumped the byte it *sends* would lock itself
    /// out of every un-upgraded gateway. This field is how a client discovers a
    /// newer protocol instead: announce 1, read this back, and use the newer
    /// message types only if it is high enough.
    pub accepted_version: u8,
    pub server_time_us: u64,
}

pub fn parse_hello_ack(body: &[u8]) -> Result<HelloAck, String> {
    if body.len() < 10 {
        return Err("truncated HELLO_ACK".into());
    }
    Ok(HelloAck {
        status: body[0],
        accepted_version: body[1],
        server_time_us: u64::from_le_bytes(body[2..10].try_into().unwrap()),
    })
}

/// The id word a record carries: the arbitration id with its flags packed in.
///
/// Not the GVRET packing, which puts the extended bit at 31 — the same three
/// facts, three different layouts, which is exactly why this is written once.
pub fn record_id_flags(arb_id: u32, extended: bool, is_fd: bool, transmitted: bool) -> u32 {
    let mut id = arb_id & ID_ARB_MASK;
    if extended {
        id |= ID_EXTENDED;
    }
    if is_fd {
        id |= ID_FD;
    }
    if transmitted {
        id |= ID_TX;
    }
    id
}

/// Append one record: `delta_us u32 | id_flags u32 | bus u8 | len u8 | payload`.
///
/// `delta_us` saturates at zero, as the Python's `max(0, ...)` did: a batch
/// whose base is not its earliest frame would otherwise wrap into a delta three
/// hours in the future.
pub fn encode_record_into(
    out: &mut Vec<u8>,
    base_ts_us: u64,
    ts_us: u64,
    id_flags: u32,
    bus: u8,
    payload: &[u8],
) {
    let payload = &payload[..payload.len().min(MAX_PAYLOAD)];
    out.reserve(10 + payload.len());
    out.extend_from_slice(&(ts_us.saturating_sub(base_ts_us) as u32).to_le_bytes());
    out.extend_from_slice(&id_flags.to_le_bytes());
    out.push(bus);
    out.push(payload.len() as u8);
    out.extend_from_slice(payload);
}

/// `BATCH` — a sequence number, the base timestamp, and `count` records.
///
/// `records` is the buffer [`encode_record_into`] was appended to, and `count`
/// is how many went into it; they are separate because the caller is the only
/// thing that knows both, and a record's length is not fixed.
pub fn encode_batch(seq: u32, base_ts_us: u64, count: u16, records: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(14 + records.len());
    body.extend_from_slice(&seq.to_le_bytes());
    body.extend_from_slice(&base_ts_us.to_le_bytes());
    body.extend_from_slice(&count.to_le_bytes());
    body.extend_from_slice(records);
    encode_message(MSG_BATCH, &body)
}

/// What a gateway said about a `BATCH`.
#[derive(Debug, PartialEq, Eq)]
pub struct Ack {
    pub seq: u32,
    pub status: u8,
    /// How full the gateway's own queue is, as a percentage.
    pub queue_pct: u8,
}

pub fn parse_ack(body: &[u8]) -> Result<Ack, String> {
    if body.len() < 6 {
        return Err("truncated ACK".into());
    }
    Ok(Ack {
        seq: u32::from_le_bytes(body[0..4].try_into().unwrap()),
        status: body[4],
        queue_pct: body[5],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello_body(token: &[u8], database: &str, flags: u8) -> Vec<u8> {
        let mut b = MAGIC.to_vec();
        b.push(PROTO_VERSION);
        b.push(flags);
        b.push(token.len() as u8);
        b.extend_from_slice(token);
        b.push(database.len() as u8);
        b.extend_from_slice(database.as_bytes());
        b
    }

    #[test]
    fn frame_round_trip() {
        let msg = encode_message(MSG_PING, b"");
        let mut buf = msg.clone();
        let frame = take_frame(&mut buf).unwrap().unwrap();
        assert_eq!(frame.mtype, MSG_PING);
        assert!(frame.crc_ok);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_frame_waits_for_more() {
        let msg = encode_message(MSG_PING, b"");
        let mut buf = msg[..3].to_vec();
        assert!(take_frame(&mut buf).unwrap().is_none());
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn corrupt_crc_detected() {
        let mut msg = encode_message(MSG_BATCH, &[0u8; 14]);
        let n = msg.len();
        msg[n - 1] ^= 0xFF;
        let mut buf = msg;
        let frame = take_frame(&mut buf).unwrap().unwrap();
        assert!(!frame.crc_ok);
    }

    #[test]
    fn hello_with_database() {
        let h = parse_hello(&hello_body(
            b"sekrit",
            "vehicle_1",
            HELLO_FLAG_TIME_RELATIVE,
        ))
        .unwrap();
        assert_eq!(h.token, b"sekrit");
        assert_eq!(h.database, "vehicle_1");
        assert!(h.time_relative);
    }

    #[test]
    fn hello_minimal_no_database_field() {
        // Back-compat: token but no db_len byte at all
        let mut b = MAGIC.to_vec();
        b.extend_from_slice(&[PROTO_VERSION, 0, 3]);
        b.extend_from_slice(b"abc");
        let h = parse_hello(&b).unwrap();
        assert_eq!(h.database, "");
    }

    #[test]
    fn batch_parse_and_limits() {
        let mut body = Vec::new();
        body.extend_from_slice(&7u32.to_le_bytes());
        body.extend_from_slice(&1_000_000u64.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        for (delta, id) in [(0u32, 0x123u32), (1000, 0x18FF50E5 | ID_EXTENDED)] {
            body.extend_from_slice(&delta.to_le_bytes());
            body.extend_from_slice(&id.to_le_bytes());
            body.push(1); // bus
            body.push(3); // len
            body.extend_from_slice(&[1, 2, 3]);
        }
        let batch = parse_batch(&body, 256).unwrap().unwrap();
        assert_eq!(batch.seq, 7);
        assert_eq!(batch.records.len(), 2);
        assert_eq!(batch.records[1].id_flags & ID_ARB_MASK, 0x18FF50E5);
        assert!(batch.records[1].id_flags & ID_EXTENDED != 0);

        // count over the limit is malformed-with-seq
        let mut over = body.clone();
        over[12..14].copy_from_slice(&5000u16.to_le_bytes());
        assert!(matches!(parse_batch(&over, 256), Some(Err(7))));
    }

    // --- the two halves, against each other ------------------------------

    /// What the client encodes is what the server parses. Neither half can be
    /// changed without this failing, which is the point of them sharing a
    /// crate.
    #[test]
    fn a_hello_round_trips_through_the_server_half() {
        let msg = encode_hello(b"sekrit", "vehicle_1", false);
        let mut buf = msg;
        let frame = take_frame(&mut buf).unwrap().unwrap();
        assert_eq!(frame.mtype, MSG_HELLO);
        assert!(frame.crc_ok);

        let hello = parse_hello(&frame.body).unwrap();
        assert_eq!(hello.version, PROTO_VERSION);
        assert_eq!(hello.token, b"sekrit");
        assert_eq!(hello.database, "vehicle_1");
        assert!(!hello.time_relative, "the forward client sends absolute");

        // An empty database is the gateway's default, and must still carry its
        // length byte rather than being omitted.
        let mut buf = encode_hello(b"k", "", true);
        let frame = take_frame(&mut buf).unwrap().unwrap();
        let hello = parse_hello(&frame.body).unwrap();
        assert_eq!(hello.database, "");
        assert!(hello.time_relative);
    }

    #[test]
    fn a_batch_round_trips_through_the_server_half() {
        const BASE: u64 = 1_700_000_000_000_000;
        let mut records = Vec::new();
        encode_record_into(
            &mut records,
            BASE,
            BASE,
            record_id_flags(0x123, false, false, false),
            0,
            &[1, 2, 3],
        );
        encode_record_into(
            &mut records,
            BASE,
            BASE + 1000,
            record_id_flags(0x18FF_50E5, true, true, true),
            1,
            &[0xAA; 64],
        );

        let mut buf = encode_batch(7, BASE, 2, &records);
        let frame = take_frame(&mut buf).unwrap().unwrap();
        assert_eq!(frame.mtype, MSG_BATCH);
        assert!(frame.crc_ok);

        let batch = parse_batch(&frame.body, MAX_BATCH_RECORDS)
            .expect("carries a seq")
            .expect("well formed");
        assert_eq!((batch.seq, batch.base_ts_us), (7, BASE));
        assert_eq!(batch.records[0].delta_us, 0);
        assert_eq!(batch.records[0].payload, [1, 2, 3]);

        let second = &batch.records[1];
        assert_eq!(second.delta_us, 1000);
        assert_eq!(second.bus, 1);
        assert_eq!(second.id_flags & ID_ARB_MASK, 0x18FF_50E5);
        assert!(second.id_flags & ID_EXTENDED != 0);
        assert!(second.id_flags & ID_FD != 0);
        assert!(second.id_flags & ID_TX != 0, "a frame this server sent");
        assert_eq!(second.payload.len(), MAX_PAYLOAD);
    }

    /// The Python clamped a delta at zero rather than letting it wrap. A base
    /// that is not the earliest frame is a caller error, but a three-hour
    /// forward jump in the archive is a worse way to find out.
    #[test]
    fn a_record_before_the_base_gets_a_zero_delta() {
        let mut records = Vec::new();
        encode_record_into(&mut records, 5_000, 1_000, 0x123, 0, &[]);
        assert_eq!(u32::from_le_bytes(records[0..4].try_into().unwrap()), 0);
    }

    /// A payload longer than a CAN FD frame is truncated, not refused: the
    /// length byte could not describe it and the server would NACK the batch.
    #[test]
    fn an_oversized_payload_is_clamped_to_one_fd_frame() {
        let mut records = Vec::new();
        encode_record_into(&mut records, 0, 0, 0x1, 0, &[0xFF; 200]);
        assert_eq!(records[9] as usize, MAX_PAYLOAD);
        assert_eq!(records.len(), 10 + MAX_PAYLOAD);
    }

    #[test]
    fn acks_round_trip() {
        let mut buf = encode_hello_ack(HELLO_BAD_AUTH, 42);
        let frame = take_frame(&mut buf).unwrap().unwrap();
        assert_eq!(
            parse_hello_ack(&frame.body).unwrap(),
            HelloAck {
                status: HELLO_BAD_AUTH,
                accepted_version: PROTO_VERSION,
                server_time_us: 42,
            }
        );

        let mut buf = encode_ack(9, ACK_OVERLOADED, 87);
        let frame = take_frame(&mut buf).unwrap().unwrap();
        assert_eq!(
            parse_ack(&frame.body).unwrap(),
            Ack {
                seq: 9,
                status: ACK_OVERLOADED,
                queue_pct: 87,
            }
        );
    }

    #[test]
    fn a_truncated_reply_is_an_error_rather_than_a_panic() {
        assert!(parse_hello_ack(&[0, 1]).is_err());
        assert!(parse_ack(&[0, 0, 0]).is_err());
    }
}
