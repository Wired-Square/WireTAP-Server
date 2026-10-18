// Thin fetch wrapper. The admin key lives in sessionStorage for the life of
// the tab; every call sends it as a Bearer token. Behind a console there is none.

const KEY_STORAGE = "wiretap-admin-key";

export function setKey(key: string) {
  sessionStorage.setItem(KEY_STORAGE, key);
}

export function clearKey() {
  sessionStorage.removeItem(KEY_STORAGE);
}

/** The start of the console's refusal when its user last proved their password too long ago. */
const STALE_PROOF = "this needs the password confirmed";

/** The gateway's base path: everything before the SPA's own `/admin/`. */
const API_BASE = (() => {
  const path = location.pathname;
  const admin = path.lastIndexOf("/admin/");
  return admin < 0 ? "" : path.slice(0, admin);
})();

/** A path on the gateway, as the browser reaches it. */
export function apiUrl(path: string): string {
  return API_BASE + path;
}

function authorisation(): Record<string, string> {
  const key = sessionStorage.getItem(KEY_STORAGE);
  return key ? { Authorization: `Bearer ${key}` } : {};
}

function refusal(status: number, text: string): ApiError {
  let message = `${status}`;
  let body: unknown = null;
  try {
    body = JSON.parse(text);
    message = (body as { error?: string }).error ?? message;
  } catch {
    /* non-JSON error body */
  }
  if (status === 403 && message.startsWith(STALE_PROOF)) {
    message += ". Re-authenticate in the console, then try again.";
  }
  return new ApiError(status, message, body);
}

export async function api<T>(
  path: string,
  options: { method?: string; body?: unknown } = {},
): Promise<T> {
  const headers = authorisation();
  if (options.body !== undefined) headers["Content-Type"] = "application/json";
  const resp = await fetch(apiUrl(path), {
    method: options.method ?? "GET",
    headers,
    body: options.body !== undefined ? JSON.stringify(options.body) : undefined,
  });
  if (!resp.ok) throw refusal(resp.status, await resp.text());
  if (resp.status === 204) return undefined as T;
  return resp.json() as Promise<T>;
}

/** POST a file, reporting bytes sent: fetch cannot report an upload's progress. */
export function upload<T>(
  path: string,
  file: Blob,
  onProgress: (sent: number, total: number) => void,
): Promise<T> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open("POST", apiUrl(path));
    for (const [name, value] of Object.entries(authorisation())) xhr.setRequestHeader(name, value);
    xhr.setRequestHeader("Content-Type", ARCHIVE_TYPE);
    xhr.upload.onprogress = (e) => onProgress(e.loaded, e.total);
    xhr.onload = () => {
      if (xhr.status >= 300) return reject(refusal(xhr.status, xhr.responseText));
      try {
        resolve(JSON.parse(xhr.responseText) as T);
      } catch {
        reject(new Error(`The gateway answered ${xhr.status} without saying what it did.`));
      }
    };
    xhr.onerror = () => reject(new Error("The upload failed: the connection to the gateway was lost."));
    xhr.send(file);
  });
}

const ARCHIVE_TYPE = "application/vnd.wiretap.archive";

/** `[from_us, to_us)`, in microseconds since the epoch. */
export interface ArchiveWindow {
  from_us: number;
  to_us: number;
}

/** A database's bounds, and roughly what an export of a window would hold. */
export interface ArchiveEstimate {
  min_ts_us: number | null;
  max_ts_us: number | null;
  /** To the hour, from the rollup. */
  frames: number;
  events: number;
  approx_bytes: number;
  empty: boolean;
}

export interface ArchiveManifest {
  schema_version: number;
  gateway_version: string;
  created_at: string;
  database: string;
  window: ArchiveWindow | null;
  tables: { name: string; columns: string[]; rows: number }[];
}

export interface Imported {
  database: string;
  replaced: boolean;
  source: string;
  window: ArchiveWindow | null;
  rows: Record<string, number>;
  elapsed_ms: number;
  /** Done, but not yet serving, and why. */
  attention: string | null;
}

/**
 * An archive's manifest, read in the browser: the magic and format version,
 * then the first section of the gzip stream (docs/archive.md).
 */
export async function readManifest(file: Blob): Promise<ArchiveManifest> {
  const head = new Uint8Array(await file.slice(0, 5).arrayBuffer());
  if (new TextDecoder().decode(head.slice(0, 4)) !== "WTAR") {
    throw new Error("This is not a WireTAP archive.");
  }
  if (head[4] !== 1) throw new Error(`Archive format v${head[4]} is newer than this page reads.`);
  const reader = file
    .slice(5)
    .stream()
    .pipeThrough(new DecompressionStream("gzip"))
    .getReader();
  let read = new Uint8Array(0);
  try {
    for (;;) {
      if (read.length >= 4) {
        const len = new DataView(read.buffer).getUint32(0);
        if (read.length >= 4 + len) {
          return JSON.parse(new TextDecoder().decode(read.slice(4, 4 + len))) as ArchiveManifest;
        }
      }
      const { value, done } = await reader.read();
      if (done) throw new Error("The archive ends before its manifest.");
      const more = new Uint8Array(read.length + value.length);
      more.set(read);
      more.set(value, read.length);
      read = more;
    }
  } finally {
    reader.cancel().catch(() => {});
  }
}

/** A refusal, with its status and JSON body for the callers that read them. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly body: unknown,
  ) {
    super(message);
  }
}

/** Who `/v1/whoami` says is calling: a key, or a user an ApplianceOS console vouches for. */
export interface Caller {
  via: "key" | "console";
  name: string;
  role: KeySummary["role"];
}

export interface KeySummary {
  id: number;
  name: string;
  role: "read" | "ingest" | "admin";
  database_pin: string | null;
  created_at: string;
  last_used_at: string | null;
  disabled: boolean;
}

/** Whether each secret is saved in a file the gateway owns, or comes from env. */
export interface SecretSources {
  db_password: { persisted: boolean };
  admin_key: { configured: boolean; persisted: boolean; stale_config: boolean };
}

/** A rotated secret, shown once, and what to do with it. */
export interface RotatedSecret {
  secret: string;
  persisted: boolean;
  note: string;
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
  /** What the migration to the current schema last reported; null when settled. */
  migration: MigrationProgress | null;
  /** Frames this gateway has buffered while the database is behind, else null. */
  buffered_rows: number | null;
  /** Seconds an archive import into it has been running, else null. */
  import_busy_secs: number | null;
}

/** One migration's progress row, as `wiretap_migration_progress` keeps it. */
export interface MigrationProgress {
  phase: string;
  chunks_done: number | null;
  chunks_total: number | null;
  rows_done: number | null;
  /** The chunk being worked on, or the one a failed run stopped at. */
  current_chunk_start: string | null;
  compressed_left: number | null;
  uncompressed_left: number | null;
  avg_s_compressed: number | null;
  avg_s_uncompressed: number | null;
  last_error: string | null;
  updated_at: string;
}

/**
 * Seconds left, from the average chunk time so far. Compressed and uncompressed
 * chunks are costed apart, as a compressed one is decompressed and compressed
 * again; until one kind has been timed, the other's average stands in.
 */
export function migrationEta(m: MigrationProgress): number | null {
  const compressed = m.avg_s_compressed ?? m.avg_s_uncompressed;
  const uncompressed = m.avg_s_uncompressed ?? m.avg_s_compressed;
  if (compressed === null || uncompressed === null) return null;
  return Math.round(
    (m.compressed_left ?? 0) * compressed + (m.uncompressed_left ?? 0) * uncompressed,
  );
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
  /** Whether a database behind is migrated without being asked. */
  auto_migrate: boolean;
}

/** Seconds as `2h 33m 20s`, without leading zero units. */
export function formatElapsed(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (h > 0) return `${h}h ${m}m ${s}s`;
  return m > 0 ? `${m}m ${s}s` : `${s}s`;
}

export interface IngestSession {
  peer: string;
  key_name: string;
  database: string;
  protocol_version: number;
  frames: number;
  batches: number;
  connected_at: string;
}

export interface DaemonList {
  daemons: Daemon[];
}

export interface Daemon {
  daemon_id: string;
  devices: DaemonDevice[];
}

export interface DaemonDevice {
  interface: string;
  /** null for an interface assigned but never named in a HELLO. */
  bus: number | null;
  database: string | null;
  last_seen_us: number | null;
  assignment: Assignment | null;
  /** null until the daemon sends a CATALOG_STATUS. */
  active: ActiveCatalog | null;
}

export interface Assignment {
  blob_sha: string;
  name: string | null;
  assigned_at_us: number;
  assigned_by: string | null;
}

export interface ActiveCatalog {
  source: "assigned" | "local" | "none";
  blob_sha: string | null;
  name: string | null;
  since_us: number;
  refused: { blob_sha: string; reason: string } | null;
}

export interface CatalogFinding {
  field: string;
  message: string;
}

export interface StoredCatalog {
  blob_sha: string;
  content: string;
  created_at_us: number;
}

export type LogLevel = "trace" | "debug" | "info" | "warn" | "error";

export interface LogRecord {
  seq: number;
  at: string;
  level: LogLevel;
  target: string;
  message: string;
  fields: [string, string][];
}

/** Oldest first. */
export interface LogSnapshot {
  started_at: string;
  records: LogRecord[];
  log_file: string | null;
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
