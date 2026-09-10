-- wiretap-schema-postgres.sql
-- Version: 20260910
-- Schema for raw capture frames + decoded signals (TimescaleDB hypertable).
-- CAN, Modbus and serial share one table, discriminated by `protocol`.
--
-- Requires the timescaledb extension (PostgreSQL 14+):
--   - add timescaledb to shared_preload_libraries and restart Postgres
--   - CREATE EXTENSION needs superuser
-- Existing pre-TimescaleDB databases should be migrated with
-- migrate_to_timescale.py rather than re-running this file.

SET client_min_messages = NOTICE;

CREATE EXTENSION IF NOT EXISTS timescaledb;

-- ----------------------------------------
-- The ingest role the GRANTs at the foot of this file target
-- ----------------------------------------
-- Here rather than in the caller, because this file has three callers now —
-- apply_capture_schema(), a bare psql session, and the migration's `\ir` — and
-- only the first could be relied on to create the role first. A cluster
-- predating the rename still holds it as `candor`; rename it *before* deploying
-- this, or these grants land on a fresh, unused role.
DO $$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'wiretap') THEN
    CREATE ROLE wiretap NOLOGIN;
  END IF;
END $$;

-- ----------------------------------------
-- Precondition: this file assumes the capture_frame names
-- ----------------------------------------
-- The gateway re-applies this file to an existing database on every start, so
-- it can meet an archive that predates the rename. Left unguarded that is a
-- destructive half-apply rather than an error: every statement here is
-- IF NOT EXISTS, so an empty second `capture_frame` hypertable gets created
-- beside the real `can_frame` table before anything fails — and once it exists,
-- the migration's `ALTER TABLE … RENAME TO capture_frame` cannot run either,
-- so the operator is wedged between two states.
--
-- Fail first, and name the fix.
DO $$ BEGIN
  IF (SELECT relkind FROM pg_class WHERE oid = to_regclass('public.can_frame')) = 'r' THEN
    RAISE EXCEPTION 'this database predates the capture_frame rename; apply '
                    'crates/wiretap-backend/schema/migrations/0001_capture_frame.sql first';
  END IF;
END $$;

-- ----------------------------------------
-- Helper: safe byte accessor for bytea
-- ----------------------------------------
CREATE OR REPLACE FUNCTION public.get_byte_safe(p bytea, n int)
RETURNS int
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
  SELECT CASE WHEN octet_length(p) > n THEN get_byte(p, n) ELSE NULL END
$$;

-- ----------------------------------------
-- Raw Frames
-- ----------------------------------------
-- Notes vs the pre-TimescaleDB schema:
--   - no row_id: nothing reads it, and a serial PK fights hypertable
--     partitioning (a PK must include the partition column)
--   - no stored id_hex/data_hex: redundant storage (100+ GB/year);
--     the convenience views below compute them on the fly
--
-- One table for every protocol, discriminated by `protocol`. It was called
-- can_frame until 2026-09-10, and `public.can_frame` remains as a CAN-only
-- view so anything reading the old name keeps working.
--
-- Why one table and not a modbus_register hypertable beside it: a Modbus tap
-- recovers *messages*, and on a real line most of them carry vendor function
-- codes no catalogue decodes into registers — 90.1% of a measured Sungrow
-- RS-485 line, as wiretap-catalog's own framer recovers it. A register-shaped table would discard them. What every
-- protocol does have is raw bytes plus an id, which is what this stores;
-- decoding stays in the desktop against the catalogue, exactly as CAN's does.
CREATE TABLE IF NOT EXISTS public.capture_frame (
  ts          timestamptz NOT NULL,               -- message/kernel receive time
  ingest_ts   timestamptz NOT NULL DEFAULT now(), -- ingest time
  protocol    text        NOT NULL DEFAULT 'can'  -- which wire this came off
                CHECK (protocol IN ('can', 'modbus', 'serial')),
  id          integer     NOT NULL,               -- CAN arbitration id, Modbus register, serial frame id
  extended    boolean     NOT NULL,               -- CAN: 11-bit if false or 29-bit if true
  dlc         smallint    NOT NULL                -- payload length: 0..8 CAN, 0..64 FD, 0..256 Modbus
                CHECK (dlc >= 0 AND dlc <= 256),
  is_fd       boolean     NOT NULL,               -- CAN FD flag
  data_bytes  bytea       NOT NULL,               -- raw payload
  bus         integer     NOT NULL DEFAULT 0,     -- gvret bus id / link index
  dir         text        NOT NULL DEFAULT 'rx'   -- gvret frame direction (rx/tx)
                CHECK (dir IN ('rx', 'tx')),
  unit        smallint,                           -- Modbus slave address; NULL otherwise
  func        smallint,                           -- Modbus function code, vendor codes included; NULL otherwise
  crc_valid   boolean                             -- Modbus: did the framer's CRC check out; NULL otherwise
);

-- 1-day chunks: ~5-10M rows/chunk at typical capture rates, good
-- chunk exclusion for hour/day analysis windows, ~365 chunks/year.
SELECT create_hypertable('public.capture_frame', 'ts',
  chunk_time_interval => INTERVAL '1 day', if_not_exists => TRUE);

-- The hypertable provides a (ts) index per chunk automatically; the only
-- other predicate used by WireTAP is per-id over time, within one protocol.
CREATE INDEX IF NOT EXISTS idx_capture_frame_id_ts
  ON public.capture_frame (protocol, id, ts DESC);

-- Columnar compression: frames of one id are highly self-similar, so segment
-- by it and order by ts (delta-of-delta). protocol leads the key because every
-- query carries it, and it is near-constant within a chunk so it costs nothing.
ALTER TABLE public.capture_frame SET (
  timescaledb.compress,
  timescaledb.compress_segmentby = 'protocol, id',
  timescaledb.compress_orderby   = 'ts'
);

-- Compress chunks once they are comfortably past any plausible
-- outage + disk-cache drain window (late writes land uncompressed).
SELECT add_compression_policy('public.capture_frame',
  compress_after => INTERVAL '7 days', if_not_exists => TRUE);

-- Optional retention: uncomment to discard frames older than 24 months.
-- SELECT add_retention_policy('public.capture_frame', INTERVAL '24 months');

-- ----------------------------------------
-- Hourly rollup (continuous aggregate)
-- ----------------------------------------
-- Makes frame inventory / catalog coverage near-instant instead of
-- scanning the raw table. Maintained incrementally by a background job;
-- materialized_only = false folds in the not-yet-materialised tail.
-- `protocol` is in the GROUP BY, not a WHERE: the rollup has to serve every
-- protocol, and a bare can_frame_hourly over a mixed table would report Modbus
-- frames as CAN to anything that trusted the name.
CREATE MATERIALIZED VIEW IF NOT EXISTS public.capture_frame_hourly
WITH (timescaledb.continuous, timescaledb.materialized_only = false) AS
SELECT
  time_bucket(INTERVAL '1 hour', ts) AS bucket,
  protocol, id, extended, bus,
  count(*)  AS frame_count,
  min(ts)   AS first_ts,
  max(ts)   AS last_ts,
  max(dlc)  AS max_dlc
FROM public.capture_frame
GROUP BY 1, 2, 3, 4, 5
WITH NO DATA;

SELECT add_continuous_aggregate_policy('public.capture_frame_hourly',
  start_offset      => INTERVAL '3 hours',
  end_offset        => INTERVAL '1 hour',
  schedule_interval => INTERVAL '30 minutes',
  if_not_exists     => TRUE);

-- ----------------------------------------
-- Compatibility: the CAN-only names this schema used before 2026-09-10
-- ----------------------------------------
-- The gateway's own SQL reads `capture_frame` and filters on protocol. These
-- exist for everything else — psql sessions, dashboards, the runbooks — so the
-- rename is not a breaking change for anyone outside this repo.
--
-- `can_frame` is a simple single-table view with a WHERE, so PostgreSQL makes
-- it auto-updatable: a plain INSERT through it works, and `protocol` comes from
-- the column default.
--
-- INSERT works; **COPY does not** — `cannot copy to view`, and enabling it
-- would take an INSTEAD OF trigger. That is deliberate: these views are for
-- readers. The gateway's own bulk path COPYs `public.capture_frame`, and the
-- last outside bulk writer went with the Python oracle on 2026-09-10.
--
-- The two byte views below derive from this one rather than re-filtering the
-- base table, so the CAN rows are defined here and nowhere else.
-- `can_frame_hourly` reads the rollup and needs its own filter. All four are
-- the same CAN-only family: they are dropped together or not at all.
CREATE OR REPLACE VIEW public.can_frame AS
SELECT ts, ingest_ts, id, extended, dlc, is_fd, data_bytes, bus, dir
FROM public.capture_frame
WHERE protocol = 'can';

CREATE OR REPLACE VIEW public.can_frame_hourly AS
SELECT bucket, id, extended, bus, frame_count, first_ts, last_ts, max_dlc
FROM public.capture_frame_hourly
WHERE protocol = 'can';

-- ----------------------------------------
-- Convenience view: expose b0..b7
-- ----------------------------------------
CREATE OR REPLACE VIEW public.can_frame_bytes AS
SELECT
  ts, ingest_ts, id,
  lpad(upper(to_hex(id)), CASE WHEN extended THEN 8 ELSE 3 END, '0') AS id_hex,
  extended, dlc, is_fd, data_bytes,
  upper(encode(data_bytes, 'hex')) AS data_hex,
  bus, dir,
  public.get_byte_safe(data_bytes, 0)  AS b0,
  public.get_byte_safe(data_bytes, 1)  AS b1,
  public.get_byte_safe(data_bytes, 2)  AS b2,
  public.get_byte_safe(data_bytes, 3)  AS b3,
  public.get_byte_safe(data_bytes, 4)  AS b4,
  public.get_byte_safe(data_bytes, 5)  AS b5,
  public.get_byte_safe(data_bytes, 6)  AS b6,
  public.get_byte_safe(data_bytes, 7)  AS b7
FROM public.can_frame;

-- ----------------------------------------
-- Convenience view: expose b0..b63
-- ----------------------------------------
CREATE OR REPLACE VIEW public.can_fd_frame_bytes AS
SELECT
  ts, ingest_ts, id,
  lpad(upper(to_hex(id)), CASE WHEN extended THEN 8 ELSE 3 END, '0') AS id_hex,
  extended, dlc, is_fd, data_bytes,
  upper(encode(data_bytes, 'hex')) AS data_hex,
  bus, dir,
  public.get_byte_safe(data_bytes, 0)  AS b0,
  public.get_byte_safe(data_bytes, 1)  AS b1,
  public.get_byte_safe(data_bytes, 2)  AS b2,
  public.get_byte_safe(data_bytes, 3)  AS b3,
  public.get_byte_safe(data_bytes, 4)  AS b4,
  public.get_byte_safe(data_bytes, 5)  AS b5,
  public.get_byte_safe(data_bytes, 6)  AS b6,
  public.get_byte_safe(data_bytes, 7)  AS b7,
  public.get_byte_safe(data_bytes, 8)  AS b8,
  public.get_byte_safe(data_bytes, 9)  AS b9,
  public.get_byte_safe(data_bytes, 10) AS b10,
  public.get_byte_safe(data_bytes, 11) AS b11,
  public.get_byte_safe(data_bytes, 12) AS b12,
  public.get_byte_safe(data_bytes, 13) AS b13,
  public.get_byte_safe(data_bytes, 14) AS b14,
  public.get_byte_safe(data_bytes, 15) AS b15,
  public.get_byte_safe(data_bytes, 16) AS b16,
  public.get_byte_safe(data_bytes, 17) AS b17,
  public.get_byte_safe(data_bytes, 18) AS b18,
  public.get_byte_safe(data_bytes, 19) AS b19,
  public.get_byte_safe(data_bytes, 20) AS b20,
  public.get_byte_safe(data_bytes, 21) AS b21,
  public.get_byte_safe(data_bytes, 22) AS b22,
  public.get_byte_safe(data_bytes, 23) AS b23,
  public.get_byte_safe(data_bytes, 24) AS b24,
  public.get_byte_safe(data_bytes, 25) AS b25,
  public.get_byte_safe(data_bytes, 26) AS b26,
  public.get_byte_safe(data_bytes, 27) AS b27,
  public.get_byte_safe(data_bytes, 28) AS b28,
  public.get_byte_safe(data_bytes, 29) AS b29,
  public.get_byte_safe(data_bytes, 30) AS b30,
  public.get_byte_safe(data_bytes, 31) AS b31,
  public.get_byte_safe(data_bytes, 32) AS b32,
  public.get_byte_safe(data_bytes, 33) AS b33,
  public.get_byte_safe(data_bytes, 34) AS b34,
  public.get_byte_safe(data_bytes, 35) AS b35,
  public.get_byte_safe(data_bytes, 36) AS b36,
  public.get_byte_safe(data_bytes, 37) AS b37,
  public.get_byte_safe(data_bytes, 38) AS b38,
  public.get_byte_safe(data_bytes, 39) AS b39,
  public.get_byte_safe(data_bytes, 40) AS b40,
  public.get_byte_safe(data_bytes, 41) AS b41,
  public.get_byte_safe(data_bytes, 42) AS b42,
  public.get_byte_safe(data_bytes, 43) AS b43,
  public.get_byte_safe(data_bytes, 44) AS b44,
  public.get_byte_safe(data_bytes, 45) AS b45,
  public.get_byte_safe(data_bytes, 46) AS b46,
  public.get_byte_safe(data_bytes, 47) AS b47,
  public.get_byte_safe(data_bytes, 48) AS b48,
  public.get_byte_safe(data_bytes, 49) AS b49,
  public.get_byte_safe(data_bytes, 50) AS b50,
  public.get_byte_safe(data_bytes, 51) AS b51,
  public.get_byte_safe(data_bytes, 52) AS b52,
  public.get_byte_safe(data_bytes, 53) AS b53,
  public.get_byte_safe(data_bytes, 54) AS b54,
  public.get_byte_safe(data_bytes, 55) AS b55,
  public.get_byte_safe(data_bytes, 56) AS b56,
  public.get_byte_safe(data_bytes, 57) AS b57,
  public.get_byte_safe(data_bytes, 58) AS b58,
  public.get_byte_safe(data_bytes, 59) AS b59,
  public.get_byte_safe(data_bytes, 60) AS b60,
  public.get_byte_safe(data_bytes, 61) AS b61,
  public.get_byte_safe(data_bytes, 62) AS b62,
  public.get_byte_safe(data_bytes, 63) AS b63
FROM public.can_frame;

-- ----------------------------------------
-- Import function (legacy)
-- ----------------------------------------
-- Kept for compatibility with older wiretap-server deployments
-- ([postgres].write_mode = "function"). New deployments use COPY directly.
-- Signature change from the pre-TimescaleDB schema: RETURNS void (row_id
-- no longer exists), so the old definition must be dropped first.
DROP FUNCTION IF EXISTS public.ingest_can_frame(
  timestamptz, boolean, boolean, integer, text, smallint, bytea, integer, text
);
CREATE FUNCTION public.ingest_can_frame(
  _ts timestamptz,
  _extended boolean,
  _is_fd boolean,
  _id integer DEFAULT NULL,
  _id_hex text DEFAULT NULL,
  _dlc smallint DEFAULT NULL,
  _data_bytes bytea DEFAULT NULL,
  _bus integer DEFAULT 0,
  _dir text DEFAULT 'rx'
) RETURNS void
LANGUAGE plpgsql AS $$
DECLARE
  v_id integer;
BEGIN
  -- Exactly one of id / id_hex must be provided
  IF (_id IS NULL) = (_id_hex IS NULL) THEN
     RAISE EXCEPTION 'Provide exactly one of id or id_hex';
  END IF;

  v_id := COALESCE(_id, public.hex_to_int(_id_hex));

  IF _data_bytes IS NULL THEN
    RAISE EXCEPTION 'Provide data_bytes';
  END IF;

  -- If dlc not provided, derive from payload length
  IF _dlc IS NULL THEN
    _dlc := octet_length(_data_bytes);
  END IF;

  -- Writes the base table rather than the can_frame view: the view is
  -- auto-updatable and would work, but naming the protocol here is clearer
  -- than relying on a column default reached through two layers.
  INSERT INTO public.capture_frame
    (ts, protocol, id, extended, dlc, is_fd, data_bytes, bus, dir)
  VALUES
    (_ts, 'can', v_id, _extended, _dlc, _is_fd, _data_bytes, _bus, _dir);
END $$;

-- ----------------------------------------
-- Helper conversion functions
-- ----------------------------------------
CREATE OR REPLACE FUNCTION public.hex_to_int(p_hex text)
RETURNS integer
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
  SELECT (('x' || upper(regexp_replace(p_hex, '^0x', '', 'i')))::bit(32))::int
$$;

-- ----------------------------------------
-- Events table
-- ----------------------------------------
CREATE TABLE IF NOT EXISTS public.events (
    id            BIGSERIAL PRIMARY KEY,
    ts            TIMESTAMPTZ       NOT NULL,
    kind          TEXT              NOT NULL,  -- e.g. 'modbus'
    source        TEXT              NOT NULL,  -- profile-local source name
    key           TEXT              NOT NULL,  -- signal/register identifier
    value_json    JSONB,                        -- raw/normalized value
    meta          JSONB                         -- arbitrary source metadata
);

-- Helpful indexes (time- and id-oriented, DESC for "recent first" queries)
CREATE INDEX IF NOT EXISTS events_ts_idx
  ON public.events (ts DESC);
CREATE INDEX IF NOT EXISTS events_kind_source_idx
  ON public.events (kind, source);
CREATE INDEX IF NOT EXISTS events_key_idx
  ON public.events (key);

-- ----------------------------------------
-- Permissions for ingestion role
-- ----------------------------------------
GRANT USAGE ON SCHEMA public TO wiretap;
GRANT INSERT, SELECT ON TABLE public.capture_frame TO wiretap;
GRANT SELECT ON public.capture_frame_hourly TO wiretap;
-- The pre-2026-09-10 names, still granted so an external reader on the old
-- name keeps working.
GRANT INSERT, SELECT ON TABLE public.can_frame TO wiretap;
GRANT SELECT ON public.can_frame_hourly TO wiretap;
GRANT EXECUTE ON FUNCTION public.ingest_can_frame(
  timestamptz, boolean, boolean, integer, text, smallint, bytea, integer, text
) TO wiretap;
GRANT EXECUTE ON FUNCTION public.hex_to_int(text) TO wiretap;
GRANT EXECUTE ON FUNCTION public.get_byte_safe(bytea, int) TO wiretap;
GRANT INSERT, SELECT ON TABLE public.events TO wiretap;
GRANT USAGE ON SEQUENCE public.events_id_seq TO wiretap;
