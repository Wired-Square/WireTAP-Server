import {
  DatabaseEntry,
  ROLLUP_FRESH_SECS,
  formatElapsed,
  formatSpan,
  migrationEta,
} from "./api";

/**
 * How one database's schema state is drawn. Shared by Databases and Health so
 * the two cannot disagree about what "pending" looks like.
 *
 * Colour carries the meaning: green is current, orange is not settled — behind
 * or mid-migration, both of which resolve on their own — and red is reserved for
 * a migration that actually failed. The version number is on every one of them,
 * because "which schema is this archive on" is the question the column exists to
 * answer.
 */
export function SchemaBadge({ db, target }: { db: DatabaseEntry; target: number }) {
  const { schema_state: state, schema_version: version, busy_secs: busy } = db;

  switch (state) {
    case "current":
      return <span className="badge ok">v{version}</span>;
    case "pending":
      return (
        <span
          className="badge behind"
          title="Behind the current schema: refusing reads, and buffering ingest, until it is migrated"
        >
          v{version} → v{target}
        </span>
      );
    case "migrating":
      return (
        <span className="badge migrating" title="Reads are refused, and ingest buffered, until this finishes">
          Migrating{busy !== null && ` ${formatElapsed(busy)}`}
        </span>
      );
    case "failed":
      return (
        <span className="badge revoked" title={db.schema_error ?? undefined}>
          Failed
        </span>
      );
    default:
      return <span className="muted">—</span>;
  }
}

/** What each phase a migration reports is doing, in the operator's words. */
const PHASES: Record<string, string> = {
  started: "starting",
  backfill: "converting chunks",
  check: "validating",
  rollup: "rebuilding the rollup",
  drain: "draining buffered ingest",
  done: "finishing",
  failed: "stopped",
};

/** Whether the Databases page offers to migrate this one now, or again. */
export function canMigrate(db: DatabaseEntry): boolean {
  return db.schema_state === "pending" || db.schema_state === "failed";
}

/**
 * How far a migration has got: chunks, rows, the chunk it is on, the time it
 * has taken and is likely to take, and the ingest buffered meanwhile. For a run
 * that failed, the error and the chunk it stopped at.
 */
export function MigrationProgressView({ db }: { db: DatabaseEntry }) {
  const m = db.migration;
  const running = db.schema_state === "migrating";
  const buffered =
    db.buffered_rows !== null && `${db.buffered_rows.toLocaleString()} frames buffered`;
  if (!m || (!running && m.phase === "done")) {
    return buffered ? <div className="migration muted">{buffered}</div> : null;
  }
  const error = db.schema_error ?? m.last_error;
  const phase = PHASES[m.phase] ?? m.phase;
  const day = m.current_chunk_start && new Date(m.current_chunk_start).toLocaleDateString();
  const total = m.chunks_total ?? 0;
  const done = m.chunks_done ?? 0;
  const eta = running && m.phase === "backfill" ? migrationEta(m) : null;
  const facts = [
    total > 0 && `${done} of ${total} chunks`,
    m.rows_done !== null && `${m.rows_done.toLocaleString()} rows converted`,
    day && (error ? `stopped at ${day}` : `on ${day}`),
    running && db.busy_secs !== null && `${formatSpan(db.busy_secs)} so far`,
    eta !== null && `about ${formatSpan(eta)} left`,
    buffered,
  ].filter(Boolean);
  return (
    <div className="migration">
      <div className="muted">
        {running ? phase : error ? "Stopped" : `Last reported: ${phase}`}
      </div>
      {total > 0 && (
        <div className="progress" title={`${done} of ${total} chunks`}>
          <div style={{ width: `${(100 * done) / total}%` }} />
        </div>
      )}
      <div className="muted">{facts.join(" · ")}</div>
      {!running && error && <div className="error">{error}</div>}
    </div>
  );
}

/**
 * True while this database is changing by itself — migrating, queued behind
 * the startup sweep, or rebuilding its rollup. With automatic migration off a
 * `pending` database waits for an operator, so it is not polled for.
 */
export function isBusy(d: DatabaseEntry, autoMigrate: boolean): boolean {
  if (d.rollup_busy_secs !== null || d.schema_state === "migrating") return true;
  return autoMigrate && (d.schema_state === "pending" || d.schema_state === "failed");
}

/**
 * How far the hourly rollup's stored summaries fall short of the newest frame.
 *
 * Not a chunk count: a continuous aggregate chunks at ten times the base
 * interval, so a four-day archive occupies one chunk whether it is barely built
 * or finished. The distance in time is the thing an operator can act on.
 */
export function RollupBadge({ db }: { db: DatabaseEntry }) {
  if (db.rollup_busy_secs !== null) {
    return (
      <span className="badge migrating" title="Materialising the hourly rollup">
        Rebuilding {formatElapsed(db.rollup_busy_secs)}
      </span>
    );
  }
  switch (db.rollup_state) {
    case null:
      return <span className="muted">—</span>;
    case "empty":
      return <span className="muted">no frames</span>;
    case "incomplete":
      return (
        <span
          className="badge behind"
          title="Stored summaries do not reach the first frame, so rollup queries omit the gap"
        >
          incomplete
        </span>
      );
  }
  const lag = db.rollup_lag_secs;
  if (lag === null) return <span className="badge ok">up to date</span>;
  if (lag <= ROLLUP_FRESH_SECS) {
    return (
      <span
        className="badge ok"
        title={`Stored summaries reach to within ${formatSpan(lag)} of the newest frame`}
      >
        up to date
      </span>
    );
  }
  return (
    <span className="badge behind" title="Buckets newer than the last stored one are recomputed on every query">
      {formatSpan(lag)} behind
    </span>
  );
}

/** Whether rebuilding this rollup would actually achieve anything. */
export function rollupNeedsWork(db: DatabaseEntry): boolean {
  if (db.rollup_busy_secs !== null) return false;
  // An empty database has nothing to summarise, so a rebuild would write no rows
  // and the button would never stop offering itself.
  if (db.rollup_state === "incomplete") return true;
  if (db.rollup_state !== "covered") return false;
  return (db.rollup_lag_secs ?? 0) > ROLLUP_FRESH_SECS;
}
