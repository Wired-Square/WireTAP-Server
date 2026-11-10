/**
 * The gateway's `/v1` API as its screens read it, and the decisions they make
 * from it: which screens a role is offered, what each form sends, and how a
 * database's state is worded.
 */

export const ARCHIVE_TYPE = "application/vnd.wiretap.archive";

/** The start of the gateway's refusal when the console's proof of the password is too old. */
const STALE_PROOF = "this needs the password confirmed";

export type GatewayRole = "read" | "ingest" | "admin";

/** Who `/v1/whoami` says is calling. */
export interface Caller {
  via: "key" | "console";
  name: string;
  role: GatewayRole;
}

export const SCREENS = [
  { id: "databases", label: "Databases", admin: false },
  { id: "keys", label: "Keys", admin: true },
  { id: "ingest", label: "Ingest", admin: true },
  { id: "daemons", label: "Daemons", admin: true },
  { id: "activity", label: "Activity", admin: true },
  { id: "log", label: "Log", admin: true },
  { id: "health", label: "Health", admin: false },
] as const;

export type ScreenId = (typeof SCREENS)[number]["id"];

/** Every screen but the two a read role may see answers it 403. */
export function screensFor(role: GatewayRole) {
  return SCREENS.filter((s) => role === "admin" || !s.admin);
}

/** The gateway's refusal, saying where a stale proof is renewed. */
export function reworded(status: number, message: string): string {
  return status === 403 && message.startsWith(STALE_PROOF)
    ? `${message[0]!.toUpperCase()}${message.slice(1)}. Re-authenticate in the console, then try again.`
    : message;
}

export interface KeySummary {
  id: number;
  name: string;
  role: GatewayRole;
  database_pin: string | null;
  created_at: string;
  last_used_at: string | null;
  disabled: boolean;
}

export function keyCreation(name: string, role: GatewayRole, pin: string) {
  return { name: name.trim(), role, database_pin: pin.trim() || null };
}

export function keyEdit(name: string, pin: string) {
  return { name: name.trim(), database_pin: pin.trim() || null };
}

export type KeyVerb = "rotate" | "disable" | "enable" | "revoke";

/** The request each key action makes. */
export function keyAction(id: number, verb: KeyVerb): { path: string; method: string } {
  return verb === "revoke"
    ? { path: `/v1/admin/keys/${id}`, method: "DELETE" }
    : { path: `/v1/admin/keys/${id}/${verb}`, method: "POST" };
}

export interface SecretSources {
  db_password: { persisted: boolean };
  admin_key: { configured: boolean; persisted: boolean; stale_config: boolean };
}

export interface RotatedSecret {
  secret: string;
  persisted: boolean;
  note: string;
}

export type SchemaState = "current" | "pending" | "migrating" | "rebuilding" | "failed" | "unknown";

export interface DatabaseEntry {
  name: string;
  size_bytes: number;
  schema_state: SchemaState;
  schema_version: number | null;
  busy_secs: number | null;
  schema_error: string | null;
  rollup_state: "covered" | "incomplete" | "empty" | null;
  rollup_lag_secs: number | null;
  rollup_busy_secs: number | null;
  migration: MigrationProgress | null;
  buffered_rows: number | null;
  import_busy_secs: number | null;
}

export interface MigrationProgress {
  phase: string;
  chunks_done: number | null;
  chunks_total: number | null;
  rows_done: number | null;
  current_chunk_start: string | null;
  compressed_left: number | null;
  uncompressed_left: number | null;
  avg_s_compressed: number | null;
  avg_s_uncompressed: number | null;
  last_error: string | null;
  updated_at: string;
}

export interface DatabaseList {
  databases: DatabaseEntry[];
  schema_version: number;
  auto_migrate: boolean;
}

/**
 * Seconds left, from the average chunk time so far, compressed and
 * uncompressed chunks costed apart; until one kind is timed the other stands in.
 */
export function migrationEta(m: MigrationProgress): number | undefined {
  const compressed = m.avg_s_compressed ?? m.avg_s_uncompressed;
  const uncompressed = m.avg_s_uncompressed ?? m.avg_s_compressed;
  if (compressed === null || uncompressed === null) return undefined;
  return Math.round((m.compressed_left ?? 0) * compressed + (m.uncompressed_left ?? 0) * uncompressed);
}

/**
 * How far the rollup may trail before rebuilding it is worth it: the policy's
 * 3 h `start_offset` plus its scheduling and bucket slack peaks just under 3.5 h.
 */
export const ROLLUP_FRESH_SECS = 4 * 60 * 60;

export function canMigrate(db: DatabaseEntry): boolean {
  return db.schema_state === "pending" || db.schema_state === "failed";
}

export function rollupNeedsWork(db: DatabaseEntry): boolean {
  if (db.rollup_busy_secs !== null) return false;
  if (db.rollup_state === "incomplete") return true;
  return db.rollup_state === "covered" && (db.rollup_lag_secs ?? 0) > ROLLUP_FRESH_SECS;
}

/** Whether the list is changing by itself, or waiting on somebody, so worth reading again soon. */
export function unsettled(list: DatabaseList): boolean {
  return list.databases.some(
    (d) =>
      d.rollup_busy_secs !== null ||
      d.import_busy_secs !== null ||
      d.schema_state === "migrating" ||
      d.schema_state === "pending" ||
      (list.auto_migrate && d.schema_state === "failed"),
  );
}

/** Why a database offers no export or replace now. */
export function archiveUnavailable(d: DatabaseEntry): string | undefined {
  if (d.import_busy_secs !== null) return "an import is running";
  if (d.schema_state !== "current") return "not at the current schema";
  return undefined;
}

/** Databases the gateway manages that are not current, for Health's one line. */
export function schemaOdd(list: DatabaseList) {
  const managed = list.databases.filter((d) => d.schema_state !== "unknown");
  const odd = managed.filter((d) => !(d.schema_state === "current" && d.schema_version === list.schema_version));
  return { managed: managed.length, odd };
}

/** `[from_us, to_us)`, in microseconds since the epoch. */
export interface ArchiveWindow {
  from_us: number;
  to_us: number;
}

export interface ArchiveEstimate {
  min_ts_us: number | null;
  max_ts_us: number | null;
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
  attention: string | null;
}

export type Scope = "whole" | "24h" | "7d" | "30d" | "day" | "month" | "custom";

export const SCOPES: [Scope, string][] = [
  ["whole", "The whole database"],
  ["24h", "The last 24 hours"],
  ["7d", "The last 7 days"],
  ["30d", "The last 30 days"],
  ["day", "A calendar day"],
  ["month", "A calendar month"],
  ["custom", "A custom range"],
];

const HOUR_US = 3_600_000_000;
const LAST: Partial<Record<Scope, number>> = { "24h": 24, "7d": 24 * 7, "30d": 24 * 30 };

const pad = (n: number) => String(n).padStart(2, "0");
export const localDay = (d: Date) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
export const localMinute = (d: Date) => `${localDay(d)}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
const toUs = (d: Date) => d.getTime() * 1000;
export const fromUs = (us: number) => new Date(Math.floor(us / 1000));

export function formatWindow(w: ArchiveWindow): string {
  return `${fromUs(w.from_us).toLocaleString()} to ${fromUs(w.to_us).toLocaleString()}`;
}

export interface Chosen {
  scope: Scope;
  day: string;
  month: string;
  from: string;
  to: string;
}

/**
 * The window an export of `bounds` sends for what was chosen, in local time,
 * clamped to the database's bounds; `null` for the whole database, `undefined`
 * while the choice is incomplete or the database holds nothing.
 */
export function exportWindow(bounds: ArchiveEstimate, c: Chosen): ArchiveWindow | null | undefined {
  if (c.scope === "whole") return null;
  if (bounds.min_ts_us === null || bounds.max_ts_us === null) return undefined;
  const end = bounds.max_ts_us + 1;
  const asked = askedWindow(c, end);
  if (!asked) return undefined;
  return { from_us: Math.max(asked.from_us, bounds.min_ts_us), to_us: Math.min(asked.to_us, end) };
}

function askedWindow(c: Chosen, end: number): ArchiveWindow | undefined {
  const hours = LAST[c.scope];
  if (hours) return { from_us: end - hours * HOUR_US, to_us: end };
  const span = (start: Date, step: (d: Date) => void) => {
    const stop = new Date(start);
    step(stop);
    return { from_us: toUs(start), to_us: toUs(stop) };
  };
  if (c.scope === "day" && c.day) return span(new Date(`${c.day}T00:00`), (d) => d.setDate(d.getDate() + 1));
  if (c.scope === "month" && c.month) {
    return span(new Date(`${c.month}-01T00:00`), (d) => d.setMonth(d.getMonth() + 1));
  }
  if (c.scope === "custom" && c.from && c.to) return { from_us: toUs(new Date(c.from)), to_us: toUs(new Date(c.to)) };
  return undefined;
}

/** The choice an export dialog opens on, from the database's bounds. */
export function chosenFor(bounds: ArchiveEstimate): Chosen {
  const chosen: Chosen = { scope: "whole", day: "", month: "", from: "", to: "" };
  if (bounds.min_ts_us === null || bounds.max_ts_us === null) return chosen;
  const last = fromUs(bounds.max_ts_us);
  return {
    scope: "whole",
    day: localDay(last),
    month: localDay(last).slice(0, 7),
    from: localMinute(fromUs(Math.max(bounds.min_ts_us, bounds.max_ts_us - 24 * HOUR_US))),
    to: localMinute(new Date(last.getTime() + 60_000)),
  };
}

export function importPath(into: string, replacing: boolean): string {
  return `/v1/databases/${into}/import${replacing ? "?replace=true" : ""}`;
}

/** An archive's manifest, read in the browser: the magic and format, then the gzip stream's first section (docs/archive.md). */
export async function readManifest(file: Blob): Promise<ArchiveManifest> {
  const head = new Uint8Array(await file.slice(0, 5).arrayBuffer());
  if (new TextDecoder().decode(head.slice(0, 4)) !== "WTAR") throw new Error("This is not a WireTAP archive.");
  if (head[4] !== 1) throw new Error(`Archive format v${head[4]} is newer than this page reads.`);
  const reader = file.slice(5).stream().pipeThrough(new DecompressionStream("gzip")).getReader();
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

export interface IngestSession {
  peer: string;
  key_name: string;
  database: string;
  protocol_version: number;
  frames: number;
  batches: number;
  connected_at: string;
}

export interface Daemon {
  daemon_id: string;
  devices: DaemonDevice[];
}

export interface DaemonDevice {
  interface: string;
  bus: number | null;
  database: string | null;
  last_seen_us: number | null;
  assignment: Assignment | null;
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

export interface StoredCatalog {
  blob_sha: string;
  content: string;
  created_at_us: number;
}

export const short = (sha: string) => sha.slice(0, 8);

/** What a device frames with, measured against what is assigned to it. */
export function activeLabel(d: DaemonDevice): { text: string; tone: "good" | "bad" | "neutral" } {
  const { assignment, active } = d;
  if (assignment) {
    if (active?.source === "assigned" && active.blob_sha === assignment.blob_sha) return { text: "applied", tone: "good" };
    if (active?.refused?.blob_sha === assignment.blob_sha) {
      return { text: `refused: ${active.refused.reason}`, tone: "bad" };
    }
    return { text: "pending", tone: "neutral" };
  }
  if (active?.source === "local" && active.blob_sha) return { text: `local ${short(active.blob_sha)}`, tone: "neutral" };
  return { text: "none", tone: "neutral" };
}

/**
 * The assignment `PUT /v1/admin/assignments` takes. `bytes`, not `File.text()`,
 * which drops a byte-order mark: the gateway's SHA-1 is over the bytes as they are.
 */
export function assignment(daemon: string, device: DaemonDevice, bytes: ArrayBuffer) {
  let content: string;
  try {
    content = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes);
  } catch {
    return undefined;
  }
  return {
    daemon_id: daemon,
    interface: device.interface,
    content,
    provenance: {},
    expected: device.assignment?.blob_sha ?? "",
  };
}

export function unassignPath(daemon: string, device: DaemonDevice & { assignment: Assignment }): string {
  const q = new URLSearchParams({ daemon_id: daemon, interface: device.interface, expected: device.assignment.blob_sha });
  return `/v1/admin/assignments?${q}`;
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

/** Cancelling a backend's query, or terminating the backend. */
export function activitySignal(db: string, pid: number, terminate: boolean): { path: string; method: string } {
  return terminate
    ? { path: `/v1/db/${db}/activity/${pid}`, method: "DELETE" }
    : { path: `/v1/db/${db}/activity/${pid}/cancel`, method: "POST" };
}

export interface Health {
  status: string;
  version: string;
  db_ok: boolean;
}
