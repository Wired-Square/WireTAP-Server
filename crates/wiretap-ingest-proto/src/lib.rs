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
//!
//! Version 2 changed the `BATCH` record in place so it can carry more than a
//! CAN frame: each record names its [`RecordKind`]. The v1 layout is still
//! parsed, through [`batch_parser`], so a gateway can take a daemon that has
//! not been upgraded yet.

pub const PROTO_VERSION: u8 = 2;
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

/// A Modbus record's flag byte: bit 0 is whether the message's CRC matched.
/// A CAN record's flags are all in its `id_flags` word and its byte is 0.
pub const FLAG_CRC_VALID: u8 = 0x01;

/// What a record's `id_flags` and payload mean. The discriminant is the wire
/// byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    /// `id_flags` is the arbitration id with [`ID_EXTENDED`], [`ID_FD`] and
    /// [`ID_TX`] packed in; the payload is one frame's data.
    Can = 0,
    /// `id_flags` is [`modbus_id`] — unit and function code — with [`ID_TX`]
    /// meaning this server sent it; the payload is the whole message, CRC
    /// included.
    Modbus = 1,
}

impl RecordKind {
    pub fn from_wire(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(RecordKind::Can),
            1 => Some(RecordKind::Modbus),
            _ => None,
        }
    }

    /// The largest payload the kind can carry: one CAN FD frame, or the
    /// longest Modbus RTU message. Both ends enforce it.
    pub fn max_payload(self) -> usize {
        match self {
            RecordKind::Can => 64,
            RecordKind::Modbus => 256,
        }
    }
}

/// Records per `BATCH`. A client must chunk at this, because it is the default
/// a gateway checks against — over it, the batch is NACKed as malformed rather
/// than accepted and truncated.
pub const MAX_BATCH_RECORDS: usize = 256;

/// The largest body a message can carry: the length field is a `u16` that
/// counts the type byte too. A batch of full-size Modbus records reaches it
/// well before [`MAX_BATCH_RECORDS`], so a client has to chunk by bytes as
/// well — see [`record_wire_len`]. Past it the length prefix wraps and the
/// far end loses framing.
pub const MAX_BODY: usize = u16::MAX as usize - 1;

/// `seq u32 | base_ts_us u64 | count u16`.
pub const BATCH_HEADER: usize = 14;

/// `delta_us u32 | kind u8 | flags u8 | bus u8 | len u16 | id_flags u32`.
pub const RECORD_HEADER: usize = 13;

/// How many bytes of a `BATCH` body one record takes, once its payload is
/// clamped as [`encode_record_into`] clamps it.
pub fn record_wire_len(kind: RecordKind, payload_len: usize) -> usize {
    RECORD_HEADER + payload_len.min(kind.max_payload())
}

pub fn encode_message(mtype: u8, body: &[u8]) -> Vec<u8> {
    debug_assert!(
        body.len() <= MAX_BODY,
        "body of {} bytes overflows the length field",
        body.len()
    );
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
    pub kind: RecordKind,
    pub flags: u8,
    pub bus: u8,
    pub id_flags: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub struct Batch {
    pub seq: u32,
    pub base_ts_us: u64,
    pub records: Vec<Record>,
}

/// A BATCH parser: `Err(seq)` = malformed but seq was readable (NACK it);
/// outer `None` = too short to even carry a seq (drop client).
pub type BatchParser = fn(&[u8], usize) -> Option<Result<Batch, u32>>;

/// The parser for the version a client announced, or `None` for a version
/// nothing here speaks — which is the whole of a server's version check.
///
/// Version 1 is accepted for one release, so a capture daemon that has not
/// been upgraded keeps flowing through a gateway that has. Remove its arm
/// with the next bump.
pub fn batch_parser(version: u8) -> Option<BatchParser> {
    match version {
        PROTO_VERSION => Some(parse_batch),
        1 => Some(parse_batch_v1),
        _ => None,
    }
}

/// A record header as read from the body: everything but the payload, and
/// how long the payload is.
type RecordHeader = fn(&[u8]) -> Option<(Record, usize)>;

fn v2_header(b: &[u8]) -> Option<(Record, usize)> {
    let kind = RecordKind::from_wire(b[4])?;
    let plen = u16::from_le_bytes(b[7..9].try_into().unwrap()) as usize;
    let record = Record {
        delta_us: u32::from_le_bytes(b[0..4].try_into().unwrap()),
        kind,
        flags: b[5],
        bus: b[6],
        id_flags: u32::from_le_bytes(b[9..13].try_into().unwrap()),
        payload: Vec::new(),
    };
    Some((record, plen))
}

/// `delta_us u32 | id_flags u32 | bus u8 | len u8`: CAN only, so every record
/// comes back as [`RecordKind::Can`] and a gateway handles both versions
/// through one path.
fn v1_header(b: &[u8]) -> Option<(Record, usize)> {
    let record = Record {
        delta_us: u32::from_le_bytes(b[0..4].try_into().unwrap()),
        kind: RecordKind::Can,
        flags: 0,
        bus: b[8],
        id_flags: u32::from_le_bytes(b[4..8].try_into().unwrap()),
        payload: Vec::new(),
    };
    Some((record, b[9] as usize))
}

fn parse_records(
    body: &[u8],
    max_frames: usize,
    header_len: usize,
    header: RecordHeader,
) -> Option<Result<Batch, u32>> {
    if body.len() < BATCH_HEADER {
        return None;
    }
    let seq = u32::from_le_bytes(body[0..4].try_into().unwrap());
    let base_ts_us = u64::from_le_bytes(body[4..12].try_into().unwrap());
    let count = u16::from_le_bytes(body[12..14].try_into().unwrap()) as usize;
    if count > max_frames {
        return Some(Err(seq));
    }
    let mut records = Vec::with_capacity(count);
    let mut off = BATCH_HEADER;
    for _ in 0..count {
        let Some((mut record, plen)) = body.get(off..off + header_len).and_then(header) else {
            return Some(Err(seq));
        };
        off += header_len;
        if plen > record.kind.max_payload() || body.len() < off + plen {
            return Some(Err(seq));
        }
        record.payload = body[off..off + plen].to_vec();
        records.push(record);
        off += plen;
    }
    Some(Ok(Batch {
        seq,
        base_ts_us,
        records,
    }))
}

/// Parse a BATCH body. See [`BatchParser`] for the contract.
pub fn parse_batch(body: &[u8], max_frames: usize) -> Option<Result<Batch, u32>> {
    parse_records(body, max_frames, RECORD_HEADER, v2_header)
}

/// Parse a version 1 BATCH body. See [`v1_header`].
pub fn parse_batch_v1(body: &[u8], max_frames: usize) -> Option<Result<Batch, u32>> {
    parse_records(body, max_frames, 10, v1_header)
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
    /// The version the gateway speaks. A client speaks one version and there
    /// is no negotiation; this is reported so a refused client can say which
    /// side is behind.
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

/// The id word a CAN record carries: the arbitration id with its flags packed in.
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

/// The inverse of [`record_id_flags`]: `(arb_id, extended, is_fd, transmitted)`.
pub fn record_id_fields(id_flags: u32) -> (u32, bool, bool, bool) {
    (
        id_flags & ID_ARB_MASK,
        id_flags & ID_EXTENDED != 0,
        id_flags & ID_FD != 0,
        id_flags & ID_TX != 0,
    )
}

/// The id word a Modbus record carries: `unit << 8 | func`, which is also the
/// `id` the archive stores, so an inventory groups by conversation.
pub fn modbus_id(unit: u8, func: u8) -> u32 {
    (u32::from(unit) << 8) | u32::from(func)
}

/// The inverse of [`modbus_id`].
pub fn modbus_unit_func(id_flags: u32) -> (u8, u8) {
    ((id_flags >> 8) as u8, id_flags as u8)
}

/// Append one record: `delta_us u32 | kind u8 | flags u8 | bus u8 | len u16 |
/// id_flags u32 | payload`.
///
/// **The base must be the batch's earliest frame, and its span must fit a
/// `u32` of microseconds.** Neither is checked by the wire format: a delta
/// below the base saturates to zero and one above 71.6 minutes wraps, and both
/// file the frame at a time it did not happen. Callers, not this function, are
/// where those hold — see `ForwardSink::write_batch`.
#[allow(clippy::too_many_arguments)]
pub fn encode_record_into(
    out: &mut Vec<u8>,
    base_ts_us: u64,
    ts_us: u64,
    kind: RecordKind,
    flags: u8,
    bus: u8,
    id_flags: u32,
    payload: &[u8],
) {
    debug_assert!(
        ts_us >= base_ts_us,
        "the base must be the batch's earliest frame: {ts_us} is before {base_ts_us}"
    );
    let payload = &payload[..payload.len().min(kind.max_payload())];
    out.reserve(record_wire_len(kind, payload.len()));
    out.extend_from_slice(&(ts_us.saturating_sub(base_ts_us) as u32).to_le_bytes());
    out.push(kind as u8);
    out.push(flags);
    out.push(bus);
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(&id_flags.to_le_bytes());
    out.extend_from_slice(payload);
}

/// `BATCH` — a sequence number, the base timestamp, and `count` records.
///
/// `records` is the buffer [`encode_record_into`] was appended to, and `count`
/// is how many went into it; they are separate because the caller is the only
/// thing that knows both, and a record's length is not fixed.
pub fn encode_batch(seq: u32, base_ts_us: u64, count: u16, records: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(BATCH_HEADER + records.len());
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

    fn batch_header(seq: u32, base_ts_us: u64, count: u16) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&seq.to_le_bytes());
        b.extend_from_slice(&base_ts_us.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
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
        let mut body = batch_header(7, 1_000_000, 2);
        for (delta, id) in [(0u32, 0x123u32), (1000, 0x18FF50E5 | ID_EXTENDED)] {
            body.extend_from_slice(&delta.to_le_bytes());
            body.push(0); // kind: CAN
            body.push(0); // flags
            body.push(1); // bus
            body.extend_from_slice(&3u16.to_le_bytes());
            body.extend_from_slice(&id.to_le_bytes());
            body.extend_from_slice(&[1, 2, 3]);
        }
        let batch = parse_batch(&body, 256).unwrap().unwrap();
        assert_eq!(batch.seq, 7);
        assert_eq!(batch.records.len(), 2);
        assert_eq!(batch.records[1].kind, RecordKind::Can);
        assert_eq!(batch.records[1].id_flags & ID_ARB_MASK, 0x18FF50E5);
        assert!(batch.records[1].id_flags & ID_EXTENDED != 0);

        // count over the limit is malformed-with-seq
        let mut over = body.clone();
        over[12..14].copy_from_slice(&5000u16.to_le_bytes());
        assert!(matches!(parse_batch(&over, 256), Some(Err(7))));
    }

    #[test]
    fn an_unknown_kind_is_malformed_with_seq() {
        let mut body = batch_header(9, 0, 1);
        body.extend_from_slice(&0u32.to_le_bytes());
        body.push(7); // no such kind
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(parse_batch(&body, 256), Some(Err(9))));
    }

    /// The old layout, as a v1 daemon still sends it, comes through the v1
    /// parser as CAN records — which is what lets a gateway take both.
    #[test]
    fn a_v1_batch_parses_as_can_records() {
        let mut body = batch_header(3, 500, 1);
        body.extend_from_slice(&42u32.to_le_bytes());
        body.extend_from_slice(&(0x7E0 | ID_TX).to_le_bytes());
        body.push(2); // bus
        body.push(2); // len
        body.extend_from_slice(&[0xAA, 0xBB]);
        let batch = parse_batch_v1(&body, 256).unwrap().unwrap();
        let r = &batch.records[0];
        assert_eq!((batch.seq, r.delta_us, r.bus), (3, 42, 2));
        assert_eq!((r.kind, r.flags), (RecordKind::Can, 0));
        assert_eq!(r.id_flags, 0x7E0 | ID_TX);
        assert_eq!(r.payload, [0xAA, 0xBB]);

        // A v1 body is not a v2 body: the v2 parser must not accept it.
        assert!(matches!(parse_batch(&body, 256), Some(Err(3))));

        // And a malformed v1 body is refused the same way a v2 one is.
        assert!(matches!(
            parse_batch_v1(&body[..body.len() - 1], 256),
            Some(Err(3))
        ));

        // The version map hands out the right parser, and no parser at all
        // for a version nothing speaks.
        let v1 = batch_parser(1).expect("v1 is still accepted");
        assert!(v1(&body, 256).unwrap().is_ok());
        let v2 = batch_parser(PROTO_VERSION).expect("the current version");
        assert!(matches!(v2(&body, 256), Some(Err(3))));
        assert!(batch_parser(99).is_none());
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
            RecordKind::Can,
            0,
            0,
            record_id_flags(0x123, false, false, false),
            &[1, 2, 3],
        );
        encode_record_into(
            &mut records,
            BASE,
            BASE + 1000,
            RecordKind::Can,
            0,
            1,
            record_id_flags(0x18FF_50E5, true, true, true),
            &[0xAA; 64],
        );
        encode_record_into(
            &mut records,
            BASE,
            BASE + 2000,
            RecordKind::Modbus,
            FLAG_CRC_VALID,
            2,
            modbus_id(1, 0x20),
            &[
                0x01, 0x20, 0x01, 0xC8, 0x03, 0x11, 0x1A, 0x00, 0x02, 0xE4, 0xCA,
            ],
        );

        let mut buf = encode_batch(7, BASE, 3, &records);
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
        assert_eq!(second.kind, RecordKind::Can);
        assert_eq!(second.id_flags & ID_ARB_MASK, 0x18FF_50E5);
        assert!(second.id_flags & ID_EXTENDED != 0);
        assert!(second.id_flags & ID_FD != 0);
        assert!(second.id_flags & ID_TX != 0, "a frame this server sent");
        assert_eq!(second.payload.len(), RecordKind::Can.max_payload());

        let third = &batch.records[2];
        assert_eq!(
            (third.kind, third.flags, third.bus),
            (RecordKind::Modbus, FLAG_CRC_VALID, 2)
        );
        assert_eq!(modbus_unit_func(third.id_flags), (1, 0x20));
        assert_eq!(third.id_flags & ID_TX, 0, "a tap sends nothing");
        assert_eq!(third.payload.len(), 11);

        assert_eq!(
            record_id_fields(second.id_flags),
            (0x18FF_50E5, true, true, true),
            "the inverse of record_id_flags"
        );
    }

    /// The saturation this asserts against is what a field defect looked like:
    /// a batch based on its first frame rather than its earliest filed every
    /// older frame at the head's time. A release build still saturates rather
    /// than wrapping, which is why the caller is where the invariant lives and
    /// this is only the backstop.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "the base must be the batch's earliest frame")]
    fn a_record_before_the_base_is_a_caller_error() {
        encode_record_into(
            &mut Vec::new(),
            5_000,
            1_000,
            RecordKind::Can,
            0,
            0,
            0x123,
            &[],
        );
    }

    /// A payload longer than its kind allows is truncated on encode, not
    /// refused — and refused on parse, so the two ends agree on the cap.
    #[test]
    fn a_modbus_payload_may_be_256_bytes_and_a_can_one_may_not() {
        let mut records = Vec::new();
        encode_record_into(&mut records, 0, 0, RecordKind::Can, 0, 0, 0x1, &[0xFF; 200]);
        assert_eq!(records.len(), record_wire_len(RecordKind::Can, 200));
        assert_eq!(u16::from_le_bytes([records[7], records[8]]), 64);

        let mut records = Vec::new();
        encode_record_into(
            &mut records,
            0,
            0,
            RecordKind::Modbus,
            0,
            0,
            0x1,
            &[0xFF; 300],
        );
        assert_eq!(records.len(), record_wire_len(RecordKind::Modbus, 300));
        let batch = parse_batch(&[batch_header(1, 0, 1), records].concat(), 256)
            .unwrap()
            .unwrap();
        assert_eq!(batch.records[0].payload.len(), 256);

        // The same 256 bytes claimed by a CAN record are over its cap.
        let mut body = batch_header(4, 0, 1);
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&[0, 0, 0]);
        body.extend_from_slice(&256u16.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&[0; 256]);
        assert!(matches!(parse_batch(&body, 256), Some(Err(4))));
    }

    /// The arithmetic `ForwardSink::batch_len` has to respect: a batch of
    /// full-size Modbus records does not fit the length field, and a batch of
    /// full-size CAN records does — which is why the byte bound was never hit
    /// before there was a second kind.
    #[test]
    fn a_full_modbus_batch_would_not_fit_a_frame() {
        let modbus = BATCH_HEADER + MAX_BATCH_RECORDS * record_wire_len(RecordKind::Modbus, 256);
        let can = BATCH_HEADER + MAX_BATCH_RECORDS * record_wire_len(RecordKind::Can, 64);
        assert!(
            modbus > MAX_BODY,
            "{modbus} fits, so the byte bound is dead code"
        );
        assert!(
            can <= MAX_BODY,
            "{can} does not fit, so CAN batching would have to split"
        );
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
