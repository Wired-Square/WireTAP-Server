import { DatabaseEntry, ROLLUP_FRESH_SECS, formatElapsed, formatSpan } from "./api";

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
          title="Behind the current schema, so refusing reads and writes until the sweep reaches it"
        >
          v{version} → v{target}
        </span>
      );
    case "migrating":
      return (
        <span className="badge migrating" title="Reads and writes are refused until this finishes">
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

/**
 * True while this database is not settled — migrating, queued behind the sweep,
 * or rebuilding its rollup. Derived as "not settled" rather than listing the
 * busy states, so a page opened while the sweep still has everything `pending`
 * polls instead of sitting still.
 */
export function isBusy(d: DatabaseEntry): boolean {
  return (
    d.rollup_busy_secs !== null ||
    (d.schema_state !== "current" && d.schema_state !== "unknown")
  );
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
