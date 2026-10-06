-- 0004_capture_frame_flags.sql
--
-- Pack a frame's flags into one `flags` column: `wiretap_protocol::can::CanFlags`'
-- bits, RTR 1, BRS 2, ESI 4, EXT 8, FD 16 and TX 32, and only TX off any wire but
-- CAN. `extended`, `is_fd` and `dir` are backfilled into it and dropped; the
-- rollup is rebuilt on it, without remote frames, and `can_frame` serves the old
-- names from it.
--
-- **The gateway applies this itself** to a capture database behind, at Migrate
-- now on the admin UI or `POST /v1/databases/{db}/migrate`, or, with
-- WIRETAP_AUTO_MIGRATE=true, on start. Also runnable by hand:
--
--     psql -U postgres -d <db> -f 0004_capture_frame_flags.sql
--
-- One file, two runners, as 0001 to 0003. A run interrupted anywhere can be
-- repeated: the backfill resumes at the first chunk with a NULL `flags`.
--
-- **The database refuses reads for the whole run**, and the gateway buffers its
-- ingest until the run is done (see "Writing a long migration" in the README).
-- The run rewrites every chunk: each one compressed is decompressed by the
-- backfill and compressed again, one chunk to a transaction, oldest first.
-- Budget transient space of about 1.7 uncompressed chunks: measured, a 3.76 GB
-- chunk took 6.25 GB, decompressed copy included. An uncompressed chunk keeps
-- its old row versions, ~2 GB of dead tuples on that chunk, until a vacuum.
-- Time it on a copy of the archive first.

\set ON_ERROR_STOP on

-- timescale/timescaledb#10094, fixed in 2.28.1.
DO $$
DECLARE v text := (SELECT extversion FROM pg_extension WHERE extname = 'timescaledb');
BEGIN
  IF string_to_array(split_part(v, '-', 1), '.')::int[] < ARRAY[2, 28, 1] THEN
    RAISE EXCEPTION 'TimescaleDB % is older than 2.28.1, which this schema needs; '
                    'upgrade the engine and retry — nothing has been changed', v;
  END IF;
END $$;

\ir ../migration_progress.sql
SELECT public.wiretap_migration_progress(4, 'backfill');

-- No default: an unset `flags` is a row the backfill has not reached.
ALTER TABLE public.capture_frame ADD COLUMN IF NOT EXISTS flags smallint;

-- A procedure rather than a `DO`, so it can commit after each chunk. It returns
-- at once when `dir` is gone, which only a finished backfill drops.
CREATE OR REPLACE PROCEDURE public.wiretap_backfill_flags()
LANGUAGE plpgsql AS $$
DECLARE
  c record;
  t timestamptz;
  secs double precision;
  done int := 0;
  total int;
  compressed_left int;
  uncompressed_left int;
  rows_done bigint := 0;
  changed bigint;
  n_c int := 0;
  s_c double precision := 0;
  n_u int := 0;
  s_u double precision := 0;
BEGIN
  IF NOT EXISTS (SELECT FROM pg_attribute
                 WHERE attrelid = 'public.capture_frame'::regclass
                   AND attname = 'dir' AND NOT attisdropped)
  THEN
    RETURN;
  END IF;
  SELECT count(*), count(*) FILTER (WHERE is_compressed)
    INTO total, compressed_left
    FROM timescaledb_information.chunks
   WHERE hypertable_schema = 'public' AND hypertable_name = 'capture_frame';
  uncompressed_left := total - compressed_left;
  PERFORM public.wiretap_migration_progress(4, 'backfill', 0, total, 0, NULL,
                                            compressed_left, uncompressed_left);
  COMMIT;
  FOR c IN SELECT format('%I.%I', chunk_schema, chunk_name)::regclass AS chunk,
                  range_start, range_end, is_compressed
             FROM timescaledb_information.chunks
            WHERE hypertable_schema = 'public' AND hypertable_name = 'capture_frame'
            ORDER BY range_start
  LOOP
    IF c.is_compressed THEN
      compressed_left := compressed_left - 1;
    ELSE
      uncompressed_left := uncompressed_left - 1;
    END IF;
    -- A read, where a no-op UPDATE would decompress a finished chunk again.
    IF NOT EXISTS (SELECT FROM public.capture_frame
                   WHERE ts >= c.range_start AND ts < c.range_end AND flags IS NULL)
    THEN
      done := done + 1;
      CONTINUE;
    END IF;
    -- Committed before the work, so a run that dies names the chunk it died on.
    PERFORM public.wiretap_migration_progress(4, 'backfill', done, total, rows_done,
                                              c.range_start);
    COMMIT;
    t := clock_timestamp();
    -- Decompressed whole rather than through the UPDATE: recompressing a
    -- partly compressed chunk leaves its rows behind as dead tuples, a whole
    -- uncompressed chunk's worth per chunk until a VACUUM, where compressing a
    -- decompressed one truncates them.
    IF c.is_compressed THEN
      PERFORM decompress_chunk(c.chunk);
    END IF;
    EXECUTE $u$
      UPDATE public.capture_frame
         SET flags = CASE WHEN protocol = 'can'
                          THEN extended::int * 8 + is_fd::int * 16 ELSE 0 END
                     + (dir = 'tx')::int * 32
       WHERE ts >= $1 AND ts < $2 AND flags IS NULL
    $u$ USING c.range_start, c.range_end;
    GET DIAGNOSTICS changed = ROW_COUNT;
    IF c.is_compressed THEN
      PERFORM compress_chunk(c.chunk);
    END IF;
    secs := extract(epoch FROM clock_timestamp() - t);
    IF c.is_compressed THEN
      n_c := n_c + 1;
      s_c := s_c + secs;
    ELSE
      n_u := n_u + 1;
      s_u := s_u + secs;
    END IF;
    done := done + 1;
    rows_done := rows_done + changed;
    PERFORM public.wiretap_migration_progress(4, 'backfill', done, total, rows_done,
      c.range_start, compressed_left, uncompressed_left,
      s_c / nullif(n_c, 0), s_u / nullif(n_u, 0));
    COMMIT;
    RAISE NOTICE 'flags: % (from %) in %', c.chunk, c.range_start, clock_timestamp() - t;
  END LOOP;
  PERFORM public.wiretap_migration_progress(4, 'backfill', done, total, rows_done, NULL, 0, 0);
END $$;

CALL public.wiretap_backfill_flags();

DROP PROCEDURE public.wiretap_backfill_flags();

SELECT public.wiretap_migration_progress(4, 'check');

-- Each protocol's flags, in place of the CAN pair. Only CAN has bits of its
-- own, so the rollup's `flags & 8` can never read a Modbus or serial row as
-- extended. A validating scan, guarded so a repeated run does not pay for it
-- twice.
DO $$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_constraint
                 WHERE conrelid = to_regclass('public.capture_frame')
                   AND conname = 'capture_frame_protocol_columns_check'
                   AND pg_get_constraintdef(oid) LIKE '%flags%')
  THEN
    ALTER TABLE public.capture_frame
      DROP CONSTRAINT IF EXISTS capture_frame_protocol_columns_check;
    ALTER TABLE public.capture_frame
      ADD CONSTRAINT capture_frame_protocol_columns_check CHECK (
        CASE protocol
          WHEN 'can'    THEN flags IS NOT NULL
                             AND unit IS NULL AND func IS NULL AND crc_valid IS NULL
          WHEN 'modbus' THEN flags IS NOT NULL AND flags IN (0, 32)
                             AND unit IS NOT NULL AND func IS NOT NULL AND crc_valid IS NOT NULL
          ELSE               flags IS NOT NULL AND flags IN (0, 32)
                             AND unit IS NULL AND func IS NULL AND crc_valid IS NULL
        END);
  END IF;
END $$;

-- The rollup groups on `extended` and the views read the three columns, so all
-- of them go before the columns do. init_schema.sql recreates them on `flags`,
-- the rollup empty, and both runners then refresh it over its whole range — see
-- 0001's step 4 for why that is not optional.
SELECT public.wiretap_migration_progress(4, 'rollup');
DROP MATERIALIZED VIEW IF EXISTS public.capture_frame_hourly CASCADE;
DROP VIEW IF EXISTS public.can_fd_frame_bytes, public.can_frame_bytes, public.can_frame;

ALTER TABLE public.capture_frame
  DROP COLUMN IF EXISTS extended,
  DROP COLUMN IF EXISTS is_fd,
  DROP COLUMN IF EXISTS dir;

-- The rollup, the views, `ingest_can_frame` on `flags` and the version row.
-- The newest migration is the one that `\ir`s init_schema.sql — see 0001's
-- step 5.
\ir ../init_schema.sql

\ir 0001_capture_frame_post.sql
