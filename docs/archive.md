# The database archive

A gateway exports a capture database, or a window of it, as one file, and
imports it back as a new database or in place of an existing one. The admin UI's
Databases tab does both; the API is in the gateway's
[README](../crates/wiretap-backend/README.md#export-and-import). This is the
file's format, version 1. The extension is `.wtar`, and the media type
`application/vnd.wiretap.archive`.

## Layout

```
"WTAR"                    4 bytes, the magic
format version            1 byte: 1
gzip stream               RFC 1952, one member, to the end of the file
  section: manifest       UTF-8 JSON
  per table, in the manifest's order:
    section ...           the table's rows: PostgreSQL binary COPY, in pieces
    section, empty        the end of the table
```

A **section** is a 4-byte big-endian length, then that many bytes. An empty
section ends a table, so a table with no rows is one empty section after its
COPY header and trailer. Nothing follows the last table's empty section. A
table's rows may be cut into sections anywhere: the gateway writes about 64 KiB
of COPY to a section, and reads one of 16 MiB at most, and a manifest of 1 MiB.

The magic and the version are outside the compression, so a reader refuses a
file that is not an archive, or of a format it does not read, from its first
five bytes. The manifest is the stream's first section, so a reader needs only
the head of the stream to show what an archive holds: the admin UI reads it in
the browser, with the browser's own gzip decoder, before it uploads anything.

## Manifest

```json
{
  "schema_version": 5,
  "gateway_version": "0.1.12 (g1234abcd5678)",
  "created_at": "2026-10-16T09:30:00Z",
  "database": "site_a",
  "window": { "from_us": 1791500000000000, "to_us": 1792100000000000 },
  "tables": [
    { "name": "capture_frame",
      "columns": ["ts", "ingest_ts", "protocol", "id", "dlc", "data_bytes", "bus",
                  "flags", "unit", "func", "crc_valid"],
      "rows": 500001 },
    { "name": "events",
      "columns": ["id", "ts", "duration_us", "note", "created_at", "updated_at"],
      "rows": 2 }
  ]
}
```

- `schema_version` is the capture schema the rows are shaped by
  (`schema_version` in the database, `SCHEMA_VERSION` in `schema.rs`). It moves
  independently of the format version.
- `database` is the database it was exported from.
- `window` is `null` for the whole database, or `[from_us, to_us)` in
  microseconds since the epoch. It applies to `ts` in every table, and every
  table has one: an event is in the window when it starts in it.
- `rows` is each table's row count, from the same snapshot as the rows.

## Tables

Each table's sections, joined, are exactly what
`COPY (SELECT <columns> FROM public.<table> [WHERE ts >= from AND ts < to]) TO STDOUT (FORMAT binary)`
writes, header and trailer included: PostgreSQL's
[binary COPY format](https://www.postgresql.org/docs/16/sql-copy.html#id-1.9.3.55.9.4).
The columns are the manifest's, in its order. An archive carries:

| Table | Why |
| --- | --- |
| `capture_frame` | The frames, every protocol. |
| `events` | The user's annotations, with their ids. |

It does not carry `schema_version` (the manifest has it), the hourly rollup (an
import rebuilds it from the frames), an ingest buffer or a migration's progress
(an export is only of a database at the current schema, with nothing buffered),
or anything of the gateway's own database: its keys and catalogues never leave
it.

## Writing one

The whole archive is read in one `REPEATABLE READ`, read-only transaction, so
the counts and the rows are one snapshot, while ingest carries on. It is
streamed: the gateway gathers a section of rows and sends the compressed
stream 256 KiB at a time, and a browser downloads it through a single-use link that lasts 60 seconds (`POST
/v1/databases/{db}/export`, then `GET` the `url` it returns). A failure partway
ends the download with an error, so a truncated file never looks whole: its
gzip trailer is missing.

## Reading one

An import refuses:

- a file without the magic, or of another format version;
- an archive at a newer capture schema than the gateway's (upgrade the
  gateway), or at an older one. A gateway loads only its own schema version: its
  migrator takes a database forward, and there is no database of an older shape
  to load an older archive into. Archives began at schema v5, so none is older
  yet;
- tables or columns other than the ones above, in that order;
- rows PostgreSQL refuses, a table holding other than its manifest's count, a
  corrupt or truncated gzip stream, or anything after the last table.

It is loaded into a new staging database, `__import_<hex>`, which then has its
`events` id sequence moved past the imported ids, its rollup refreshed over its
whole range, its chunks older than 7 days compressed as the policy would, and
its rollup checked to cover every frame. Then:

- **as a new database**, staging is renamed to the name, refused if one of that
  name exists;
- **in place of one**, the existing database is renamed to `__old_<hex>` and
  staging to its name, in one transaction, and the old one is dropped. During
  the whole replace, the database's reads are refused and its ingest is held in
  its buffer (`capture_frame_pending`); at the swap the held rows are moved to
  the new database's buffer and drained into it. A windowed archive replaces
  the whole database: afterwards it holds only that window.

A failed import drops staging and stores the held ingest where it would have
gone. A gateway stopped partway drops every `__import_*` and `__old_*` at its
next start, except one whose import still holds its advisory lock on
`postgres` (another gateway's import on the same cluster), and its sweep drains the held ingest into the database it was
held for. The swap is one transaction, so after a stop the name is either the old
database or the new one, never neither.

## Choices

- **Binary COPY, not text**: a timestamp is 8 bytes rather than 30 characters
  and a payload its own bytes rather than escaped hex, with nothing to parse
  on the way back in, and it is the same across PostgreSQL versions for these
  types.
  TimescaleDB plays no part in it, so an archive moves between TimescaleDB
  versions and deployments.
- **gzip at its default level, not zstd**: the browser can read the manifest
  with its own `DecompressionStream`, and `flate2`'s Rust backend adds no C
  library. On a million seeded frames gzip's default level made 7.5 MB from
  86 MB of COPY in 0.7 s, its fastest level 11.1 MB in 0.4 s, its best 7.4 MB in
  2.1 s, and zstd -3 would have made 6.2 MB.
