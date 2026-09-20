-- 0003_capture_frame_protocol_columns.sql
--
-- Make capture_frame's per-protocol columns say which protocol they belong to.
-- `extended` and `is_fd` are CAN concepts a Modbus row has no answer for, so
-- they lose their NOT NULL and the writer stores NULL there from now on; in
-- exchange one CHECK ties every such column to `protocol`: a CAN row has both
-- and none of the Modbus three, a Modbus row has unit, func and crc_valid,
-- anything else has none of them. A Modbus row's CAN pair is left free rather
-- than required NULL, because every Modbus row written before this holds the
-- `false` the old NOT NULL demanded, and rewriting them is not worth what it
-- would cost. The read API says `false` for either.
--
-- **The gateway applies this itself**, on start, to every capture database it
-- finds behind. Also runnable by hand:
--
--     psql -U postgres -d <db> -f 0003_capture_frame_protocol_columns.sql
--
-- One file, two runners, as 0001 and 0002.
--
-- Measured before it was written, on a copy of a real archive — 83.6 M rows
-- over 5 compressed chunks, TimescaleDB 2.29 — because both statements touch a
-- compressed hypertable: the NOT NULL drop is 2 ms and catalogue-only; the
-- CHECK is a validating scan at ~18 M rows/s with every chunk left compressed
-- and the same size. Budget seconds per 100 M rows of `Migrating`, and expect
-- about two minutes on a 1.8 B-row archive. `NOT VALID` buys nothing here:
-- TimescaleDB refuses `VALIDATE CONSTRAINT` on a compressed hypertable, so the
-- validated form is the only one that ends validated.

\set ON_ERROR_STOP on

-- Idempotent by itself.
ALTER TABLE public.capture_frame
  ALTER COLUMN extended DROP NOT NULL,
  ALTER COLUMN is_fd    DROP NOT NULL;

-- ADD CONSTRAINT has no IF NOT EXISTS; a second run would fail on the name.
DO $$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_constraint
                 WHERE conrelid = to_regclass('public.capture_frame')
                   AND conname = 'capture_frame_protocol_columns_check')
  THEN
    ALTER TABLE public.capture_frame
      ADD CONSTRAINT capture_frame_protocol_columns_check CHECK (
        CASE protocol
          WHEN 'can'    THEN extended IS NOT NULL AND is_fd IS NOT NULL
                             AND unit IS NULL AND func IS NULL AND crc_valid IS NULL
          WHEN 'modbus' THEN unit IS NOT NULL AND func IS NOT NULL AND crc_valid IS NOT NULL
          ELSE               unit IS NULL AND func IS NULL AND crc_valid IS NULL
        END);
  END IF;
END $$;

-- The newest migration is the one that `\ir`s init_schema.sql; the next one
-- will take this line and this file will `\ir` it instead — see 0001's step 5.
\ir ../init_schema.sql
