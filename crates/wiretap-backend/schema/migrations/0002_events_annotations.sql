-- 0002_events_annotations.sql
--
-- Reshape public.events into a user's annotations on the archive: a moment or
-- a span (`ts`, `duration_us`) with a `note`. The table it replaces was a
-- sketch of the same feature from the schema's first version — `kind`,
-- `source`, `key`, `value_json`, `meta` — that nothing ever read or wrote, so
-- it is empty on every deployment. Reshaped in place rather than given a twin.
--
-- **The gateway applies this itself** to a capture database behind, at Migrate
-- now or, with WIRETAP_AUTO_MIGRATE=true, on start. Also runnable by hand:
--
--     psql -U postgres -d <db> -f 0002_events_annotations.sql
--
-- One file, two runners, as 0001: psql needs the `\` meta-commands below, and
-- the gateway's SQL splitter drops them.
--
-- Nothing here touches capture_frame or its rollup. The gateway asks whether
-- the rollup still covers the archive before rebuilding it after a migration,
-- rather than assuming every migration holed it — see `Databases::migrate_from`.

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

-- This guard is the whole safety argument: the table is only reshaped because it
-- is empty, and a deployment where something did write to it keeps its rows and
-- gets an operator instead. The existence test makes a run interrupted between
-- the drop and the re-create repeatable.
DO $$ BEGIN
  IF to_regclass('public.events') IS NOT NULL
     AND EXISTS (SELECT FROM public.events)
  THEN
    RAISE EXCEPTION 'public.events holds rows; refusing to reshape';
  END IF;
END $$;

-- Takes events_id_seq, the three indexes and the grants with it.
DROP TABLE IF EXISTS public.events;

-- The new table, its index and its grants are exactly what init_schema.sql
-- creates, and the gateway applies that file itself after this one. Reached
-- through the next migration, as 0001's step 5 explains.
\ir 0003_capture_frame_protocol_columns.sql
