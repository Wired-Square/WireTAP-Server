-- 0001_capture_frame.sql
--
-- Take a pre-2026-09-10 archive to the multi-protocol schema: can_frame becomes
-- capture_frame, gains `protocol` and the Modbus addressing columns, and the old
-- CAN-only names come back as views so nothing outside this repo breaks.
--
-- MANUAL, like tools/migrate_to_timescale.py before it. apply_capture_schema()
-- only ever applies init_schema.sql, which is idempotent but assumes the new
-- names, so run this once against an existing database first:
--
--     psql -U postgres -d <db> -f 0001_capture_frame.sql
--
-- Every step is guarded, so a run interrupted between statements can simply be
-- repeated.
--
-- Safe on a compressed hypertable, and that was measured rather than assumed:
-- against TimescaleDB 2.30.0 with 2 000 000 rows over 24 compressed chunks,
-- every statement below ran in under 0.1 s with all 24 chunks left compressed
-- and no table rewrite. PostgreSQL adds a column with a non-volatile default as
-- catalogue metadata, and TimescaleDB 2.x permits ALTER on compressed chunks.

-- One command, one exit code: don't leave this to whether the operator
-- remembered `-v ON_ERROR_STOP=1`. Step 1 is destructive if the run stops
-- silently partway.
\set ON_ERROR_STOP on

BEGIN;

-- 1. The table.
DO $$ BEGIN
  IF (SELECT relkind FROM pg_class WHERE oid = to_regclass('public.can_frame')) = 'r' THEN
    ALTER TABLE public.can_frame RENAME TO capture_frame;
  END IF;
END $$;

ALTER TABLE public.capture_frame
  ADD COLUMN IF NOT EXISTS protocol  text NOT NULL DEFAULT 'can',
  ADD COLUMN IF NOT EXISTS unit      smallint,
  ADD COLUMN IF NOT EXISTS func      smallint,
  ADD COLUMN IF NOT EXISTS crc_valid boolean;

-- `ADD COLUMN` appends, so `protocol` sits at ordinal 10 on a migrated database
-- and at 3 on a fresh one. Correcting it would rewrite the table, and touching
-- no data is worth more than tidiness here. Nothing in this repo depends on
-- ordinal position — every COPY and INSERT names its columns — but a `SELECT *`
-- consumer sees a different order on the two, so name your columns.

-- 2. Constraints. The old dlc ceiling was 64 (one CAN FD frame); a Modbus RTU
-- message reaches 256.
ALTER TABLE public.capture_frame DROP CONSTRAINT IF EXISTS can_frame_dlc_check;
ALTER TABLE public.capture_frame DROP CONSTRAINT IF EXISTS capture_frame_dlc_check;
ALTER TABLE public.capture_frame
  ADD CONSTRAINT capture_frame_dlc_check CHECK (dlc >= 0 AND dlc <= 256);

ALTER TABLE public.capture_frame DROP CONSTRAINT IF EXISTS capture_frame_protocol_check;
ALTER TABLE public.capture_frame
  ADD CONSTRAINT capture_frame_protocol_check
  CHECK (protocol IN ('can', 'modbus', 'serial'));

-- Cosmetic: `dir`'s check was auto-named after the old table. RENAME CONSTRAINT
-- has no IF EXISTS, hence the guard.
DO $$ BEGIN
  IF EXISTS (SELECT FROM pg_constraint
             WHERE conrelid = 'public.capture_frame'::regclass
               AND conname = 'can_frame_dir_check')
  THEN
    ALTER TABLE public.capture_frame
      RENAME CONSTRAINT can_frame_dir_check TO capture_frame_dir_check;
  END IF;
END $$;

-- 3. Index and compression settings.
--
-- Dropped and rebuilt, not renamed: the old index is on (id, ts DESC) and the
-- new one leads with protocol. Renaming would give the old columns the new name,
-- CREATE INDEX IF NOT EXISTS would then skip, and a migrated database would
-- silently differ from a fresh one.
DROP INDEX IF EXISTS public.idx_can_frame_id_ts;
CREATE INDEX IF NOT EXISTS idx_capture_frame_id_ts
  ON public.capture_frame (protocol, id, ts DESC);

-- Cosmetic, for fidelity with a fresh install: TimescaleDB names the implicit
-- time index after whatever the table was called when it was created.
ALTER INDEX IF EXISTS public.can_frame_ts_idx RENAME TO capture_frame_ts_idx;

-- Metadata only: existing chunks keep the setting they were compressed under,
-- which is correct rather than a gap — a pre-migration chunk is CAN-only, so
-- `protocol` is constant in it and segmenting by `id` alone is already optimal.
ALTER TABLE public.capture_frame
  SET (timescaledb.compress_segmentby = 'protocol, id');

COMMIT;

-- 4. The rollup, which has to gain `protocol` in its GROUP BY. This drops
-- materialised history and re-derives it; queries stay correct throughout
-- because materialized_only = false folds in whatever is not yet materialised,
-- and are only slower until the policy catches up. Outside a transaction: a
-- continuous aggregate cannot be created inside one.
--
-- **Test the catalogue, not relkind.** A TimescaleDB continuous aggregate is a
-- plain view to PostgreSQL — `relkind = 'v'`, not `'m'` — so a relkind guard
-- silently never fires and leaves the old aggregate in place beside the new one,
-- still grouped without `protocol` and now summarising a mixed table. That is
-- the exact failure this step exists to prevent, and it is invisible to a schema
-- diff, because the stale aggregate and the compatibility view that should
-- replace it have the same name, relkind and column list.
DO $$ BEGIN
  IF EXISTS (SELECT FROM timescaledb_information.continuous_aggregates
             WHERE view_schema = 'public' AND view_name = 'can_frame_hourly')
  THEN
    DROP MATERIALIZED VIEW public.can_frame_hourly CASCADE;
  END IF;
END $$;

-- 5. Everything else — capture_frame_hourly and its policy, the compatibility
-- views, the legacy ingest function and the grants — is exactly what
-- init_schema.sql creates, and all of it is CREATE OR REPLACE / IF NOT EXISTS.
-- Restating any of it here would only let the two drift.
--
-- `\ir` rather than a note telling the operator to run a second command:
-- between step 1 and that command the database is genuinely wrong — no
-- public.can_frame at all — so the last third of the work belongs inside this
-- script's exit code. `\ir` resolves relative to this file, and psql without
-- `-1` does not wrap it in a transaction, so the aggregate is still created
-- outside one.
--
-- One transient to know about: this creates a continuous aggregate policy, and
-- its first job may still be running when the GRANTs land on the aggregate,
-- which can fail with `tuple concurrently updated`. It is a race, not a fault —
-- observed once in testing and never on a retry. Re-run this file if you see it.
\ir ../init_schema.sql
