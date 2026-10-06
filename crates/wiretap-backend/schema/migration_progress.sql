-- migration_progress.sql
--
-- Where a long migration reports how far it has got, one row per target
-- version, for the gateway's Databases page and anyone with psql. A migration
-- calls `wiretap_migration_progress` after each committed step; arguments left
-- NULL keep what the row already holds. See "Writing a long migration" in
-- crates/wiretap-backend/README.md.
--
-- The gateway applies this before any migration; under psql a migration that
-- reports `\ir`s it.

CREATE TABLE IF NOT EXISTS public.wiretap_migration_progress (
  target_version      integer PRIMARY KEY,
  phase               text        NOT NULL,
  chunks_done         integer,
  chunks_total        integer,
  rows_done           bigint,
  current_chunk_start timestamptz,
  compressed_left     integer,
  uncompressed_left   integer,
  avg_s_compressed    double precision,
  avg_s_uncompressed  double precision,
  last_error          text,
  started_at          timestamptz NOT NULL DEFAULT now(),
  updated_at          timestamptz NOT NULL DEFAULT now()
);

-- `error` is the one argument that clears: a report without one is a run
-- still going.
CREATE OR REPLACE FUNCTION public.wiretap_migration_progress(
  target              integer,
  phase               text             DEFAULT NULL,
  chunks_done         integer          DEFAULT NULL,
  chunks_total        integer          DEFAULT NULL,
  rows_done           bigint           DEFAULT NULL,
  current_chunk_start timestamptz      DEFAULT NULL,
  compressed_left     integer          DEFAULT NULL,
  uncompressed_left   integer          DEFAULT NULL,
  avg_s_compressed    double precision DEFAULT NULL,
  avg_s_uncompressed  double precision DEFAULT NULL,
  error               text             DEFAULT NULL
) RETURNS void LANGUAGE sql AS $$
  -- Positional, because in the UPDATE an argument's name would be the column's.
  INSERT INTO public.wiretap_migration_progress AS p
    (target_version, phase, chunks_done, chunks_total, rows_done, current_chunk_start,
     compressed_left, uncompressed_left, avg_s_compressed, avg_s_uncompressed, last_error)
  VALUES ($1, coalesce($2, 'started'), $3, $4, $5, $6, $7, $8, $9, $10, $11)
  ON CONFLICT (target_version) DO UPDATE SET
    phase               = coalesce($2, p.phase),
    chunks_done         = coalesce($3, p.chunks_done),
    chunks_total        = coalesce($4, p.chunks_total),
    rows_done           = coalesce($5, p.rows_done),
    current_chunk_start = coalesce($6, p.current_chunk_start),
    compressed_left     = coalesce($7, p.compressed_left),
    uncompressed_left   = coalesce($8, p.uncompressed_left),
    avg_s_compressed    = coalesce($9, p.avg_s_compressed),
    avg_s_uncompressed  = coalesce($10, p.avg_s_uncompressed),
    last_error          = $11,
    updated_at          = now()
$$;
