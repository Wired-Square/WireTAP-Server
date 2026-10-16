//! A capture database, or a window of it, as one stream: what Export writes
//! and Import reads back. The format is `docs/archive.md`.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Utc};
use flate2::write::{GzDecoder, GzEncoder};
use futures_util::{SinkExt, Stream, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_postgres::{Client, IsolationLevel};

use crate::db::Databases;
use crate::ingest::writer::refused_the_rows;
use crate::schema;

pub const MAGIC: &[u8; 4] = b"WTAR";
pub const FORMAT_VERSION: u8 = 1;
pub const CONTENT_TYPE: &str = "application/vnd.wiretap.archive";

/// Every table an archive carries, in its order, with the columns it carries.
/// Each has `ts`, which a window filters on.
const TABLES: [(&str, &[&str]); 2] = [
    (
        "capture_frame",
        &[
            "ts",
            "ingest_ts",
            "protocol",
            "id",
            "dlc",
            "data_bytes",
            "bus",
            "flags",
            "unit",
            "func",
            "crc_valid",
        ],
    ),
    (
        "events",
        &[
            "id",
            "ts",
            "duration_us",
            "note",
            "created_at",
            "updated_at",
        ],
    ),
];

/// Above any chunk PostgreSQL's COPY sends, so only a corrupt length meets it.
const MAX_CHUNK: usize = 16 << 20;
const MAX_MANIFEST: usize = 1 << 20;
/// Compressed bytes inflated at a time: deflate expands by 1032 at most, so
/// a hostile upload cannot inflate more than about 8 MiB at once.
const INFLATE_AT: usize = 8 << 10;
/// Compressed bytes gathered before they are sent.
const SEND_AT: usize = 256 << 10;
/// COPY bytes gathered into a section.
const PIECE: usize = 64 << 10;

/// Archive bytes per row, for the estimate only. A frame with an 8-byte
/// payload measured 7.5 (counters) to 15 (random bytes).
const BYTES_PER_FRAME: i64 = 16;
const BYTES_PER_EVENT: i64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub from_us: i64,
    pub to_us: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: i32,
    pub gateway_version: String,
    pub created_at: DateTime<Utc>,
    pub database: String,
    pub window: Option<Window>,
    pub tables: Vec<TableManifest>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TableManifest {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: u64,
}

/// Why an import did not happen, by whose fault.
#[derive(Debug)]
pub enum ImportError {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
    Unavailable(String),
}

impl From<String> for ImportError {
    fn from(message: String) -> Self {
        Self::Unavailable(message)
    }
}

fn bad(message: impl Into<String>) -> ImportError {
    ImportError::BadRequest(message.into())
}

/// A data or integrity error is the archive's; anything else the database's.
fn copy_refused(table: &str) -> impl Fn(tokio_postgres::Error) -> ImportError + '_ {
    move |e| {
        let detail = format!("{table}: {}", schema::db_error_detail(&e));
        if refused_the_rows(e.code()) {
            ImportError::BadRequest(detail)
        } else {
            ImportError::Unavailable(detail)
        }
    }
}

fn at(us: i64) -> String {
    format!("'epoch'::timestamptz + INTERVAL '{us} microseconds'")
}

fn filter(window: Option<Window>) -> String {
    window.map_or(String::new(), |w| {
        format!(" WHERE ts >= {} AND ts < {}", at(w.from_us), at(w.to_us))
    })
}

fn pg(e: tokio_postgres::Error) -> String {
    schema::db_error_detail(&e)
}

/// What an export would carry, and the database's own bounds to choose from.
#[derive(Debug, Serialize)]
pub struct Estimate {
    pub min_ts_us: Option<i64>,
    pub max_ts_us: Option<i64>,
    /// From the hourly rollup, so to the hour and without remote frames.
    pub frames: i64,
    pub events: i64,
    pub approx_bytes: i64,
    /// Exact: an empty window cannot be exported.
    pub empty: bool,
}

pub async fn estimate(client: &Client, window: Option<Window>) -> Result<Estimate, String> {
    let rollup = window.map_or(String::new(), |w| {
        format!(
            " WHERE bucket >= date_trunc('hour', {}) AND bucket < {}",
            at(w.from_us),
            at(w.to_us)
        )
    });
    let within = filter(window);
    let row = client
        .query_one(
            &format!(
                "SELECT least((SELECT min(ts) FROM public.capture_frame), \
                              (SELECT min(ts) FROM public.events)), \
                        greatest((SELECT max(ts) FROM public.capture_frame), \
                                 (SELECT max(ts) FROM public.events)), \
                        (SELECT coalesce(sum(frame_count), 0)::int8 \
                           FROM public.capture_frame_hourly{rollup}), \
                        (SELECT count(*) FROM public.events{within}), \
                        NOT EXISTS (SELECT FROM public.capture_frame{within}) \
                          AND NOT EXISTS (SELECT FROM public.events{within})"
            ),
            &[],
        )
        .await
        .map_err(pg)?;
    let micros = |i| {
        row.get::<_, Option<DateTime<Utc>>>(i)
            .map(|t| t.timestamp_micros())
    };
    let (frames, events): (i64, i64) = (row.get(2), row.get(3));
    Ok(Estimate {
        min_ts_us: micros(0),
        max_ts_us: micros(1),
        frames,
        events,
        approx_bytes: frames * BYTES_PER_FRAME + events * BYTES_PER_EVENT,
        empty: row.get(4),
    })
}

pub type Chunk = Result<Bytes, std::io::Error>;

/// Write the archive to `out`, from one snapshot. A failure partway ends the
/// stream with an error, so the download fails rather than looking whole.
pub async fn export(
    mut client: Client,
    database: String,
    window: Option<Window>,
    out: mpsc::Sender<Chunk>,
) {
    if let Err(e) = write_archive(&mut client, &database, window, &out).await {
        tracing::warn!(database, "export failed: {e}");
        let _ = out.send(Err(std::io::Error::other(e))).await;
    }
}

async fn write_archive(
    client: &mut Client,
    database: &str,
    window: Option<Window>,
    out: &mpsc::Sender<Chunk>,
) -> Result<(), String> {
    let tx = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .map_err(pg)?;
    let within = filter(window);
    let mut tables = Vec::new();
    for (name, columns) in TABLES {
        let row = tx
            .query_one(&format!("SELECT count(*) FROM public.{name}{within}"), &[])
            .await
            .map_err(pg)?;
        tables.push(TableManifest {
            name: name.into(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows: row.get::<_, i64>(0) as u64,
        });
    }
    let manifest = Manifest {
        schema_version: schema::detect_version(&tx)
            .await?
            .ok_or("no capture schema")?,
        gateway_version: crate::VERSION.into(),
        created_at: Utc::now(),
        database: database.into(),
        window,
        tables,
    };

    send(out, [&MAGIC[..], &[FORMAT_VERSION]].concat()).await?;
    let mut gz = Compressed {
        encoder: GzEncoder::new(Vec::new(), flate2::Compression::default()),
        out,
    };
    let manifest = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    gz.section(&manifest).await?;
    for (name, columns) in TABLES {
        let rows = tx
            .copy_out(&format!(
                "COPY (SELECT {} FROM public.{name}{within}) TO STDOUT (FORMAT binary)",
                columns.join(", ")
            ))
            .await
            .map_err(pg)?;
        futures_util::pin_mut!(rows);
        // PostgreSQL sends a row at a time.
        let mut piece = BytesMut::new();
        while let Some(row) = rows.try_next().await.map_err(pg)? {
            piece.extend_from_slice(&row);
            if piece.len() >= PIECE {
                gz.section(&piece.split()).await?;
            }
        }
        if !piece.is_empty() {
            gz.section(&piece).await?;
        }
        gz.section(&[]).await?;
    }
    tx.commit().await.map_err(pg)?;
    gz.finish().await
}

struct Compressed<'a> {
    encoder: GzEncoder<Vec<u8>>,
    out: &'a mpsc::Sender<Chunk>,
}

impl Compressed<'_> {
    /// A length, then the bytes; an empty one ends a table.
    async fn section(&mut self, bytes: &[u8]) -> Result<(), String> {
        let len = u32::try_from(bytes.len()).map_err(|_| "a section past 4 GiB")?;
        self.encoder
            .write_all(&len.to_be_bytes())
            .and_then(|()| self.encoder.write_all(bytes))
            .map_err(|e| e.to_string())?;
        if self.encoder.get_ref().len() >= SEND_AT {
            send(self.out, std::mem::take(self.encoder.get_mut())).await?;
        }
        Ok(())
    }

    async fn finish(self) -> Result<(), String> {
        let rest = self.encoder.finish().map_err(|e| e.to_string())?;
        send(self.out, rest).await
    }
}

async fn send(out: &mpsc::Sender<Chunk>, bytes: Vec<u8>) -> Result<(), String> {
    out.send(Ok(bytes.into()))
        .await
        .map_err(|_| "the download was abandoned".to_string())
}

/// The file name a download is saved under.
pub fn file_name(database: &str, window: Option<Window>) -> String {
    let at = |us: i64| {
        DateTime::from_timestamp_micros(us)
            .map_or(us.to_string(), |t| t.format("%Y%m%dT%H%MZ").to_string())
    };
    match window {
        None => format!("{database}.wtar"),
        Some(w) => format!("{database}_{}_{}.wtar", at(w.from_us), at(w.to_us)),
    }
}

/// An upload being read: the header, then everything after it decompressed.
pub struct Reader<S> {
    body: S,
    header: Vec<u8>,
    /// Received and not yet decompressed.
    received: Bytes,
    decoder: GzDecoder<Vec<u8>>,
    decoded: BytesMut,
    ended: bool,
}

impl<S, E> Reader<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    pub fn new(body: S) -> Self {
        Self {
            body,
            header: Vec::new(),
            received: Bytes::new(),
            decoder: GzDecoder::new(Vec::new()),
            decoded: BytesMut::new(),
            ended: false,
        }
    }

    /// More decoded bytes, or false at the end of the upload.
    async fn fill(&mut self) -> Result<bool, ImportError> {
        while !self.ended {
            if self.received.is_empty() {
                self.receive().await?;
            } else {
                let piece = self.received.split_to(self.received.len().min(INFLATE_AT));
                self.decoder
                    .write_all(&piece)
                    .map_err(|e| bad(format!("the archive is corrupt: {e}")))?;
            }
            if self.take_decoded() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn receive(&mut self) -> Result<(), ImportError> {
        let Some(mut bytes) = self
            .body
            .try_next()
            .await
            .map_err(|e| bad(format!("the upload failed: {e}")))?
        else {
            self.ended = true;
            if self.header.len() < MAGIC.len() + 1 {
                return Err(bad("this is not a WireTAP archive: it is too short"));
            }
            self.decoder
                .try_finish()
                .map_err(|e| bad(format!("the archive is truncated or corrupt: {e}")))?;
            return Ok(());
        };
        let wanted = (MAGIC.len() + 1).saturating_sub(self.header.len());
        if wanted > 0 {
            let head = bytes.split_to(wanted.min(bytes.len()));
            self.header.extend_from_slice(&head);
            self.check_header()?;
        }
        self.received = bytes;
        Ok(())
    }

    fn check_header(&self) -> Result<(), ImportError> {
        let magic = &self.header[..self.header.len().min(MAGIC.len())];
        if magic != &MAGIC[..magic.len()] {
            return Err(bad("this is not a WireTAP archive"));
        }
        match self.header.get(MAGIC.len()) {
            Some(&v) if v != FORMAT_VERSION => Err(bad(format!(
                "archive format v{v} is not one this gateway reads (v{FORMAT_VERSION}); \
                 upgrade the gateway"
            ))),
            _ => Ok(()),
        }
    }

    fn take_decoded(&mut self) -> bool {
        let out = self.decoder.get_mut();
        self.decoded.extend_from_slice(out);
        let any = !out.is_empty();
        out.clear();
        any
    }

    async fn take(&mut self, n: usize) -> Result<Bytes, ImportError> {
        while self.decoded.len() < n {
            if !self.fill().await? {
                return Err(bad("the archive is truncated"));
            }
        }
        Ok(self.decoded.split_to(n).freeze())
    }

    async fn section(&mut self, max: usize) -> Result<Bytes, ImportError> {
        let len = self.take(4).await?;
        let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
        if len > max {
            return Err(bad("the archive is corrupt: a section is too long"));
        }
        self.take(len).await
    }

    /// The manifest, checked against what this gateway can load.
    pub async fn manifest(&mut self) -> Result<Manifest, ImportError> {
        let raw = self.section(MAX_MANIFEST).await?;
        let manifest: Manifest = serde_json::from_slice(&raw)
            .map_err(|e| bad(format!("the archive's manifest is corrupt: {e}")))?;
        let (at, ours) = (manifest.schema_version, schema::SCHEMA_VERSION);
        if at > ours {
            return Err(bad(format!(
                "the archive is at capture schema v{at}, newer than this gateway's v{ours}; \
                 upgrade the gateway"
            )));
        }
        if at < ours {
            return Err(bad(format!(
                "the archive is at capture schema v{at}, and this gateway loads only v{ours}: \
                 its migrator takes a database forward, and there is no v{at} database to \
                 load the archive into"
            )));
        }
        let expected = TABLES.iter().map(|(name, columns)| (*name, *columns));
        let matches = manifest.tables.len() == TABLES.len()
            && manifest
                .tables
                .iter()
                .zip(expected)
                .all(|(t, (name, columns))| t.name == name && t.columns == columns);
        if !matches {
            return Err(bad(format!(
                "the archive's tables are not the ones a v{ours} archive carries"
            )));
        }
        Ok(manifest)
    }

    /// Every table into `client`, each holding the rows its manifest says,
    /// and nothing after the last.
    pub async fn load(&mut self, client: &Client, manifest: &Manifest) -> Result<(), ImportError> {
        for (table, (name, columns)) in manifest.tables.iter().zip(TABLES) {
            let refused = copy_refused(name);
            let sink = client
                .copy_in(&format!(
                    "COPY public.{name} ({}) FROM STDIN (FORMAT binary)",
                    columns.join(", ")
                ))
                .await
                .map_err(&refused)?;
            futures_util::pin_mut!(sink);
            loop {
                let chunk = self.section(MAX_CHUNK).await?;
                if chunk.is_empty() {
                    break;
                }
                sink.send(chunk).await.map_err(&refused)?;
            }
            let rows = sink.finish().await.map_err(&refused)?;
            if rows != table.rows {
                return Err(bad(format!(
                    "{name} carried {rows} rows, and the archive's manifest says {}",
                    table.rows
                )));
            }
        }
        if !self.decoded.is_empty() || self.fill().await? {
            return Err(bad("the archive has data after its last table"));
        }
        Ok(())
    }

    /// The rest of the upload, unread: a refusal answered before the body is
    /// read resets the connection, and the browser then never sees it.
    pub async fn discard(&mut self) {
        while let Ok(Some(_)) = self.body.try_next().await {}
    }
}

/// A loaded staging database made ready to serve: its sequence past the
/// imported ids, its rollup covering everything, its older chunks compressed
/// as the policy would have.
pub async fn settle(client: &Client) -> Result<(), String> {
    client
        .batch_execute(
            "SELECT setval('public.events_id_seq', coalesce(max(id), 0) + 1, false) \
               FROM public.events",
        )
        .await
        .map_err(pg)?;
    schema::refresh_rollup(client).await?;
    client
        .batch_execute(
            "SELECT compress_chunk(c, if_not_compressed => true) \
               FROM show_chunks('public.capture_frame', older_than => INTERVAL '7 days') c",
        )
        .await
        .map_err(pg)?;
    if matches!(
        schema::rollup_status(client).await?,
        schema::RollupState::Incomplete
    ) {
        return Err("the imported rollup does not cover the archive".into());
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct Imported {
    pub database: String,
    pub replaced: bool,
    /// The database the archive was exported from.
    pub source: String,
    pub window: Option<Window>,
    pub rows: BTreeMap<String, u64>,
    pub elapsed_ms: u64,
    /// Done, but not yet serving, and why.
    pub attention: Option<String>,
}

/// Load an upload into `target`, new or replacing it. The upload is read to
/// its end whatever happens.
pub async fn import<S, E>(
    dbs: Databases,
    target: String,
    replace: bool,
    mut reader: Reader<S>,
) -> Result<Imported, ImportError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let started = Instant::now();
    let result = async {
        let manifest = reader.manifest().await?;
        let staging = dbs.begin_import(&target, replace).await?;
        let loaded = async {
            let client = dbs.connect_raw(&staging).await?;
            reader.load(&client, &manifest).await?;
            settle(&client).await?;
            Ok::<_, ImportError>(())
        };
        if let Err(e) = loaded.await {
            dbs.abandon_import(&target).await;
            return Err(e);
        }
        let attention = dbs.finish_import(&target).await?;
        Ok((manifest, attention))
    }
    .await;
    let (manifest, attention) = match result {
        Ok(done) => done,
        Err(e) => {
            reader.discard().await;
            return Err(e);
        }
    };
    Ok(Imported {
        database: target,
        replaced: replace,
        source: manifest.database,
        window: manifest.window,
        rows: manifest
            .tables
            .into_iter()
            .map(|t| (t.name, t.rows))
            .collect(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        attention,
    })
}

/// Single-use download links, so a browser can fetch an archive without a
/// bearer key: issued to an admin, redeemed by whoever holds one in time.
#[derive(Default)]
pub struct Links(std::sync::Mutex<HashMap<String, Link>>);

#[derive(Clone)]
pub struct Link {
    pub database: String,
    pub window: Option<Window>,
    issued: Instant,
}

const LINK_LIFE: Duration = Duration::from_secs(60);

impl Links {
    pub fn issue(&self, database: &str, window: Option<Window>) -> String {
        let token = crate::secrets::random_hex();
        let mut links = self.0.lock().unwrap();
        links.retain(|_, l| l.issued.elapsed() < LINK_LIFE);
        links.insert(
            token.clone(),
            Link {
                database: database.into(),
                window,
                issued: Instant::now(),
            },
        );
        token
    }

    pub fn redeem(&self, token: &str) -> Option<Link> {
        let link = self.0.lock().unwrap().remove(token)?;
        (link.issued.elapsed() < LINK_LIFE).then_some(link)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn manifest_at(schema_version: i32) -> Manifest {
        Manifest {
            schema_version,
            gateway_version: "test".into(),
            created_at: Utc::now(),
            database: "source".into(),
            window: None,
            tables: TABLES
                .iter()
                .map(|(name, columns)| TableManifest {
                    name: name.to_string(),
                    columns: columns.iter().map(|c| c.to_string()).collect(),
                    rows: 0,
                })
                .collect(),
        }
    }

    /// An archive of `manifest` and, for each table, its sections.
    pub(crate) fn archive(manifest: &Manifest, tables: &[&[&[u8]]]) -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut section = |bytes: &[u8]| {
            gz.write_all(&(bytes.len() as u32).to_be_bytes()).unwrap();
            gz.write_all(bytes).unwrap();
        };
        section(&serde_json::to_vec(manifest).unwrap());
        for sections in tables {
            for s in *sections {
                section(s);
            }
            section(&[]);
        }
        [&MAGIC[..], &[FORMAT_VERSION], &gz.finish().unwrap()].concat()
    }

    fn reader(bytes: Vec<u8>) -> Reader<impl Stream<Item = Result<Bytes, String>> + Unpin> {
        // A byte at a time, so no read lands on a convenient boundary.
        Reader::new(futures_util::stream::iter(
            bytes.into_iter().map(|b| Ok(Bytes::from(vec![b]))),
        ))
    }

    async fn refusal(bytes: Vec<u8>) -> String {
        match reader(bytes).manifest().await {
            Err(ImportError::BadRequest(m)) => m,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_current_archive_reads_its_manifest_a_byte_at_a_time() {
        let bytes = archive(&manifest_at(schema::SCHEMA_VERSION), &[&[], &[]]);
        let manifest = reader(bytes).manifest().await.unwrap();
        assert_eq!(manifest.database, "source");
    }

    #[tokio::test]
    async fn an_archive_newer_or_older_than_the_schema_is_refused_saying_which() {
        let ours = schema::SCHEMA_VERSION;
        let newer = refusal(archive(&manifest_at(ours + 1), &[])).await;
        assert!(newer.contains("upgrade the gateway"), "{newer}");
        let older = refusal(archive(&manifest_at(ours - 1), &[])).await;
        assert!(older.contains(&format!("loads only v{ours}")), "{older}");
    }

    #[tokio::test]
    async fn an_upload_that_is_not_a_current_archive_is_refused_saying_why() {
        let current = archive(&manifest_at(schema::SCHEMA_VERSION), &[&[], &[]]);
        let mut newer_format = current.clone();
        newer_format[MAGIC.len()] = FORMAT_VERSION + 1;
        let mut other_tables = manifest_at(schema::SCHEMA_VERSION);
        other_tables.tables[1].columns.pop();
        for (bytes, says) in [
            (b"PGDMP\x01\x0e".to_vec(), "not a WireTAP archive"),
            (b"WTA".to_vec(), "too short"),
            (newer_format, "format v2"),
            (current[..current.len() / 2].to_vec(), "truncated"),
            (archive(&other_tables, &[]), "tables"),
        ] {
            let refused = refusal(bytes).await;
            assert!(refused.contains(says), "{says}: {refused}");
        }
    }

    /// Past the manifest, a section claiming 4 GiB, a stream of zeros that
    /// inflates a thousandfold, and bytes after the gzip stream.
    #[tokio::test]
    async fn a_hostile_archive_is_refused_without_inflating_it_whole() {
        let manifest = serde_json::to_vec(&manifest_at(schema::SCHEMA_VERSION)).unwrap();
        let hostile = |after: &[u8]| {
            let mut gz = GzEncoder::new(Vec::new(), flate2::Compression::default());
            gz.write_all(&(manifest.len() as u32).to_be_bytes())
                .unwrap();
            gz.write_all(&manifest).unwrap();
            gz.write_all(after).unwrap();
            [&MAGIC[..], &[FORMAT_VERSION], &gz.finish().unwrap()].concat()
        };
        let whole = |bytes: Vec<u8>| {
            Reader::new(futures_util::stream::iter([Ok::<_, String>(Bytes::from(
                bytes,
            ))]))
        };

        let mut huge = whole(hostile(&u32::MAX.to_be_bytes()));
        huge.manifest().await.unwrap();
        let refused = huge.section(MAX_CHUNK).await;
        assert!(
            matches!(&refused, Err(ImportError::BadRequest(m)) if m.contains("too long")),
            "{refused:?}"
        );

        let bomb = hostile(&vec![0; 64 << 20]);
        assert!(bomb.len() < 1 << 20);
        let mut bomb = whole(bomb);
        bomb.manifest().await.unwrap();
        assert!(bomb.fill().await.unwrap());
        assert!(bomb.decoded.len() < 16 << 20, "{}", bomb.decoded.len());

        let mut junk = archive(&manifest_at(schema::SCHEMA_VERSION), &[&[], &[]]);
        junk.extend_from_slice(b"junk");
        let refused = whole(junk).manifest().await;
        assert!(
            matches!(&refused, Err(ImportError::BadRequest(m)) if m.contains("corrupt")),
            "{refused:?}"
        );
    }

    /// Sections that compress both ways, so chunk edges fall within the
    /// header, a run of zeros, noise and the trailer.
    #[tokio::test]
    async fn any_chunking_of_an_archive_reads_the_same_sections() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % below as u64) as usize
        };
        let noise: Vec<u8> = (0..100_000).map(|_| next(256) as u8).collect();
        let zeros = vec![0; 100_000];
        let tables: [&[&[u8]]; 2] = [&[&noise, &zeros, b"x"], &[&noise[..PIECE]]];
        let bytes = archive(&manifest_at(schema::SCHEMA_VERSION), &tables);
        for round in 0..24 {
            let most = [1, 7, 64, 4096, INFLATE_AT * 3, bytes.len()][round % 6];
            let mut chunks = Vec::new();
            let mut at = 0;
            while at < bytes.len() {
                let len = (1 + next(most)).min(bytes.len() - at);
                chunks.push(Ok::<_, String>(Bytes::copy_from_slice(
                    &bytes[at..at + len],
                )));
                at += len;
            }
            let mut reader = Reader::new(futures_util::stream::iter(chunks));
            reader.manifest().await.unwrap();
            for sections in tables {
                for &expected in sections {
                    let read = reader.section(MAX_CHUNK).await.unwrap();
                    assert!(read == expected, "round {round}: a section differs");
                }
                assert!(reader.section(MAX_CHUNK).await.unwrap().is_empty());
            }
            assert!(reader.decoded.is_empty() && !reader.fill().await.unwrap());
        }
    }

    #[test]
    fn an_export_link_works_once() {
        let links = Links::default();
        let token = links.issue("site", None);
        assert!(links.redeem("not-the-token").is_none());
        assert_eq!(links.redeem(&token).unwrap().database, "site");
        assert!(links.redeem(&token).is_none());
    }

    #[test]
    fn a_window_filters_on_exact_microseconds() {
        let w = Window {
            from_us: 1_700_000_000_000_001,
            to_us: 1_700_000_000_000_002,
        };
        assert_eq!(
            filter(Some(w)),
            " WHERE ts >= 'epoch'::timestamptz + INTERVAL '1700000000000001 microseconds' \
             AND ts < 'epoch'::timestamptz + INTERVAL '1700000000000002 microseconds'"
        );
        assert_eq!(
            file_name("site", Some(w)),
            "site_20231114T2213Z_20231114T2213Z.wtar"
        );
    }
}
