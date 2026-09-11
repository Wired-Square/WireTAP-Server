// Thin fetch wrapper. The admin key lives in sessionStorage for the life of
// the tab; every call sends it as a Bearer token.

const KEY_STORAGE = "wiretap-admin-key";

export function getKey(): string | null {
  return sessionStorage.getItem(KEY_STORAGE);
}

export function setKey(key: string) {
  sessionStorage.setItem(KEY_STORAGE, key);
}

export function clearKey() {
  sessionStorage.removeItem(KEY_STORAGE);
}

export async function api<T>(
  path: string,
  options: { method?: string; body?: unknown } = {},
): Promise<T> {
  const headers: Record<string, string> = {};
  const key = getKey();
  if (key) headers["Authorization"] = `Bearer ${key}`;
  if (options.body !== undefined) headers["Content-Type"] = "application/json";
  const resp = await fetch(path, {
    method: options.method ?? "GET",
    headers,
    body: options.body !== undefined ? JSON.stringify(options.body) : undefined,
  });
  if (!resp.ok) {
    let message = `${resp.status}`;
    try {
      message = (await resp.json()).error ?? message;
    } catch {
      /* non-JSON error body */
    }
    throw new Error(message);
  }
  return resp.json() as Promise<T>;
}

export interface KeySummary {
  id: number;
  name: string;
  role: "read" | "ingest" | "admin";
  database_pin: string | null;
  created_at: string;
  last_used_at: string | null;
  revoked: boolean;
}

export type SchemaState =
  | "current"
  | "pending"
  | "migrating"
  | "rebuilding"
  | "failed"
  | "unknown";

export interface DatabaseEntry {
  name: string;
  size_bytes: number;
  schema_state: SchemaState;
  /** null while migrating — the version is in flight. */
  schema_version: number | null;
  /** Seconds spent migrating, else null. */
  busy_secs: number | null;
  schema_error: string | null;
  /**
   * `covered` — stored summaries reach back to the first frame; `incomplete` —
   * they do not, so rollup queries **omit** the uncovered span rather than
   * recomputing it; `empty` — no frames. Null when the database is not in a
   * state to ask.
   */
  rollup_state: "covered" | "incomplete" | "empty" | null;
  /**
   * Seconds between the newest frame and the last materialised bucket. Null when
   * there is nothing to compare — no summaries, or no frames.
   */
  rollup_lag_secs: number | null;
  /** Seconds the rollup rebuild has been running, else null. */
  rollup_busy_secs: number | null;
}

/**
 * How far the rollup may legitimately trail before it is worth rebuilding.
 *
 * Set by what the policy can repair, not by taste. `init_schema.sql` gives it a
 * `start_offset` of 3 hours, so it only ever looks at the last three hours: a
 * rollup further behind than that can never catch up on its own.
 *
 * Four terms make up the healthy lag, and the last two are easy to miss:
 * `end_offset` holds back the last hour; `schedule_interval` adds up to 30
 * minutes before the job runs; TimescaleDB snaps the refresh window down to a
 * whole bucket, costing up to another hour; and we measure to the last
 * materialised bucket, an hour below the watermark itself. A healthy live
 * archive therefore peaks just under 3.5 h behind — so 4 h, not less.
 */
export const ROLLUP_FRESH_SECS = 4 * 60 * 60;

/** A coarse span: `3 days`, `4 hours`, `12 minutes`. */
export function formatSpan(secs: number): string {
  const units: [number, string][] = [
    [86400, "day"],
    [3600, "hour"],
    [60, "minute"],
  ];
  for (const [size, label] of units) {
    if (secs >= size) {
      const n = Math.floor(secs / size);
      return `${n} ${label}${n === 1 ? "" : "s"}`;
    }
  }
  return `${secs} seconds`;
}

export interface DatabaseList {
  databases: DatabaseEntry[];
  schema_version: number;
}

/** Seconds as `2m14s` — long enough to matter, short enough to read. */
export function formatElapsed(secs: number): string {
  return secs < 60 ? `${secs}s` : `${Math.floor(secs / 60)}m${secs % 60}s`;
}

export interface IngestSession {
  peer: string;
  key_name: string;
  database: string;
  frames: number;
  batches: number;
  connected_at: string;
}

export interface LogRecord {
  seq: number;
  ts: string;
  level: string;
  target: string;
  message: string;
  /** `database=x elapsed_ms=n` — structured fields, empty when there are none. */
  fields: string;
}

export interface Activity {
  pid: number;
  username: string | null;
  application_name: string | null;
  client_addr: string | null;
  state: string | null;
  query: string | null;
  duration_secs: number | null;
  is_cancellable: boolean;
}

export function formatBytes(n: number): string {
  if (n >= 1e12) return `${(n / 1e12).toFixed(1)} TB`;
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)} MB`;
  return `${(n / 1e3).toFixed(0)} kB`;
}
