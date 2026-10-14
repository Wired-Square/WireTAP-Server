-- 0005_capture_frame_modbus_rtu.sql
--
-- Store a Modbus RTU row's protocol as `modbus_rtu`, the tag the WireTAP
-- desktop's capture store uses (`wiretap_protocol::ingest::MODBUS_RTU_PROTOCOL`).
-- The HTTP API still takes `?protocol=modbus`: only what is stored changes.
--
-- **The gateway applies this itself** to a capture database behind, at Migrate
-- now on the admin UI or `POST /v1/databases/{db}/migrate`, or, with
-- WIRETAP_AUTO_MIGRATE=true, on start. Also runnable by hand:
--
--     psql -U postgres -d <db> -f 0005_capture_frame_modbus_rtu.sql
--
-- One file, two runners, as 0001 to 0004. A run interrupted anywhere can be
-- repeated: the relabel resumes at the first chunk still holding `modbus`.
--
-- Only Modbus rows are rewritten. `protocol` leads the compression segmentby,
-- so the UPDATE decompresses only a chunk's Modbus batches and `compress_chunk`
-- recompresses only those segments; a chunk without one is a read of its
-- segment metadata. Dead tuples: about one per Modbus row, until a vacuum. The
-- CHECK swap is one validating scan of the archive, as 0003's was.

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
SELECT public.wiretap_migration_progress(5, 'backfill');

-- Both CHECKs refuse the new tag, so both go before any row changes, and come
-- back together after the last one has. Asked of the columns check, the one
-- 0004 recreates when a run starts before it.
DO $$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_constraint
                 WHERE conrelid = to_regclass('public.capture_frame')
                   AND conname = 'capture_frame_protocol_columns_check'
                   AND pg_get_constraintdef(oid) LIKE '%modbus_rtu%')
  THEN
    ALTER TABLE public.capture_frame
      DROP CONSTRAINT IF EXISTS capture_frame_protocol_check,
      DROP CONSTRAINT IF EXISTS capture_frame_protocol_columns_check;
  END IF;
END $$;

CREATE OR REPLACE PROCEDURE public.wiretap_relabel_modbus_rtu()
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
  SELECT count(*), count(*) FILTER (WHERE is_compressed)
    INTO total, compressed_left
    FROM timescaledb_information.chunks
   WHERE hypertable_schema = 'public' AND hypertable_name = 'capture_frame';
  uncompressed_left := total - compressed_left;
  PERFORM public.wiretap_migration_progress(5, 'backfill', 0, total, 0, NULL,
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
    IF NOT EXISTS (SELECT FROM public.capture_frame
                   WHERE ts >= c.range_start AND ts < c.range_end AND protocol = 'modbus')
    THEN
      done := done + 1;
      CONTINUE;
    END IF;
    -- Committed before the work, so a run that dies names the chunk it died on.
    PERFORM public.wiretap_migration_progress(5, 'backfill', done, total, rows_done,
                                              c.range_start);
    COMMIT;
    t := clock_timestamp();
    UPDATE public.capture_frame SET protocol = 'modbus_rtu'
     WHERE ts >= c.range_start AND ts < c.range_end AND protocol = 'modbus';
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
    PERFORM public.wiretap_migration_progress(5, 'backfill', done, total, rows_done,
      c.range_start, compressed_left, uncompressed_left,
      s_c / nullif(n_c, 0), s_u / nullif(n_u, 0));
    COMMIT;
    RAISE NOTICE 'modbus_rtu: % (from %) in %', c.chunk, c.range_start, clock_timestamp() - t;
  END LOOP;
  PERFORM public.wiretap_migration_progress(5, 'backfill', done, total, rows_done, NULL, 0, 0);
END $$;

CALL public.wiretap_relabel_modbus_rtu();

DROP PROCEDURE public.wiretap_relabel_modbus_rtu();

SELECT public.wiretap_migration_progress(5, 'check');

-- One ALTER, so the two checks share one validating scan.
DO $$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_constraint
                 WHERE conrelid = to_regclass('public.capture_frame')
                   AND conname = 'capture_frame_protocol_columns_check'
                   AND pg_get_constraintdef(oid) LIKE '%modbus_rtu%')
  THEN
    ALTER TABLE public.capture_frame
      ADD CONSTRAINT capture_frame_protocol_check
        CHECK (protocol IN ('can', 'modbus_rtu', 'serial')),
      ADD CONSTRAINT capture_frame_protocol_columns_check CHECK (
        CASE protocol
          WHEN 'can'        THEN flags IS NOT NULL
                                 AND unit IS NULL AND func IS NULL AND crc_valid IS NULL
          WHEN 'modbus_rtu' THEN flags IS NOT NULL AND flags IN (0, 32)
                                 AND unit IS NOT NULL AND func IS NOT NULL AND crc_valid IS NOT NULL
          ELSE                   flags IS NOT NULL AND flags IN (0, 32)
                                 AND unit IS NULL AND func IS NULL AND crc_valid IS NULL
        END);
  END IF;
END $$;

-- A continuous aggregate cannot be updated, but its materialisation hypertable
-- can, and `protocol` is stored there as a group key. Relabelled in place
-- rather than rebuilt, which on a large archive is minutes against
-- milliseconds. The relabel's UPDATE also logged invalidations; any refresh
-- that reaches them recomputes the same rows. Absent when 0004 dropped it in
-- this run: init_schema.sql then recreates it over the relabelled rows.
SELECT public.wiretap_migration_progress(5, 'rollup');
DO $$
DECLARE mat text := (SELECT format('%I.%I', materialization_hypertable_schema,
                                            materialization_hypertable_name)
                       FROM timescaledb_information.continuous_aggregates
                      WHERE view_schema = 'public' AND view_name = 'capture_frame_hourly');
BEGIN
  IF mat IS NOT NULL THEN
    EXECUTE format('UPDATE %s SET protocol = %L WHERE protocol = %L',
                   mat, 'modbus_rtu', 'modbus');
  END IF;
END $$;

-- The version row. The newest migration is the one that `\ir`s
-- init_schema.sql — see 0001's step 5.
\ir ../init_schema.sql

\ir 0001_capture_frame_post.sql
