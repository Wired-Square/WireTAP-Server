import { type RefObject, useEffect, useImperativeHandle, useState } from "react";
import {
  ArchiveEstimate,
  ArchiveManifest,
  ArchiveWindow,
  Imported,
  api,
  formatBytes,
  readManifest,
  upload,
} from "../api";

const HOUR_US = 3_600_000_000;

/** What the export dialog has chosen: the whole database, a window, or nothing exportable. */
export type ExportChoice = { window: ArchiveWindow | null; empty: boolean };

type Scope = "whole" | "24h" | "7d" | "30d" | "day" | "month" | "custom";

const SCOPES: [Scope, string][] = [
  ["whole", "The whole database"],
  ["24h", "The last 24 hours"],
  ["7d", "The last 7 days"],
  ["30d", "The last 30 days"],
  ["day", "A calendar day"],
  ["month", "A calendar month"],
  ["custom", "A custom range"],
];

const LAST: Partial<Record<Scope, number>> = { "24h": 24, "7d": 24 * 7, "30d": 24 * 30 };

const pad = (n: number) => String(n).padStart(2, "0");
const localDay = (d: Date) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
const localMinute = (d: Date) => `${localDay(d)}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
const toUs = (d: Date) => d.getTime() * 1000;
const fromUs = (us: number) => new Date(Math.floor(us / 1000));

export function formatWindow(w: ArchiveWindow): string {
  return `${fromUs(w.from_us).toLocaleString()} to ${fromUs(w.to_us).toLocaleString()}`;
}

/** `[from, to)` in local time for a scope, before it is clamped to the database's bounds. */
function windowFor(scope: Scope, last: number, day: string, month: string, from: string, to: string) {
  const hours = LAST[scope];
  if (hours) return { from_us: last - hours * HOUR_US, to_us: last };
  if (scope === "day" && day) {
    const start = new Date(`${day}T00:00`);
    const end = new Date(start);
    end.setDate(end.getDate() + 1);
    return { from_us: toUs(start), to_us: toUs(end) };
  }
  if (scope === "month" && month) {
    const start = new Date(`${month}-01T00:00`);
    const end = new Date(start);
    end.setMonth(end.getMonth() + 1);
    return { from_us: toUs(start), to_us: toUs(end) };
  }
  if (scope === "custom" && from && to) {
    return { from_us: toUs(new Date(from)), to_us: toUs(new Date(to)) };
  }
  return null;
}

/**
 * Whole or a window, guided by the database's own bounds, with an estimate of
 * what the export would hold. `choice` is what Export acts on.
 */
export function ExportForm({ db, choice }: { db: string; choice: RefObject<ExportChoice> }) {
  const [bounds, setBounds] = useState<ArchiveEstimate | null>(null);
  const [estimate, setEstimate] = useState<ArchiveEstimate | null>(null);
  const [unestimated, setUnestimated] = useState("");
  const [error, setError] = useState("");
  const [scope, setScope] = useState<Scope>("whole");
  const [day, setDay] = useState("");
  const [month, setMonth] = useState("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");

  useEffect(() => {
    api<ArchiveEstimate>(`/v1/databases/${db}/export/estimate`)
      .then((b) => {
        setBounds(b);
        if (b.max_ts_us === null || b.min_ts_us === null) return;
        const last = fromUs(b.max_ts_us);
        setDay(localDay(last));
        setMonth(localDay(last).slice(0, 7));
        setFrom(localMinute(fromUs(Math.max(b.min_ts_us, b.max_ts_us - 24 * HOUR_US))));
        setTo(localMinute(new Date(last.getTime() + 60_000)));
      })
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, [db]);

  // Exclusive, so the newest row is inside a window that ends at the bounds.
  const end = bounds?.max_ts_us != null ? bounds.max_ts_us + 1 : null;
  const asked = end === null || scope === "whole" ? null : windowFor(scope, end, day, month, from, to);
  const window =
    asked && bounds?.min_ts_us != null && end !== null
      ? { from_us: Math.max(asked.from_us, bounds.min_ts_us), to_us: Math.min(asked.to_us, end) }
      : null;
  const windowEmpty = window !== null && window.from_us >= window.to_us;
  const key = window ? `${window.from_us}-${window.to_us}` : scope;

  useEffect(() => {
    if (!bounds) return;
    if (scope === "whole") {
      setEstimate(bounds);
      return;
    }
    setEstimate(null);
    setUnestimated("");
    if (!window || windowEmpty) return;
    // An answer for a window since changed must not land on the new one.
    let current = true;
    const t = setTimeout(() => {
      api<ArchiveEstimate>(
        `/v1/databases/${db}/export/estimate?from_us=${window.from_us}&to_us=${window.to_us}`,
      )
        .then((e) => current && setEstimate(e))
        .catch((e) => current && setUnestimated(e instanceof Error ? e.message : String(e)));
    }, 300);
    return () => {
      current = false;
      clearTimeout(t);
    };
    // The key stands for the window, whose object is new on every render.
  }, [db, bounds, key]);

  // Unknown while estimating: the gateway refuses an empty window itself.
  const empty = (scope !== "whole" && (!window || windowEmpty)) || estimate?.empty === true;
  choice.current = { window: scope === "whole" ? null : window, empty };

  if (error) return <p className="error">{error}</p>;
  if (!bounds) return <p className="muted">Reading the database's bounds…</p>;
  if (bounds.min_ts_us === null || bounds.max_ts_us === null) {
    return <p className="muted">"{db}" holds nothing to export.</p>;
  }
  const first = fromUs(bounds.min_ts_us);
  const last = fromUs(bounds.max_ts_us);
  return (
    <div className="archive-form">
      <p className="muted">
        "{db}" holds {first.toLocaleString()} to {last.toLocaleString()}. Times are local.
      </p>
      <label>
        Export{" "}
        <select value={scope} onChange={(e) => setScope(e.target.value as Scope)}>
          {SCOPES.map(([value, label]) => (
            <option key={value} value={value}>
              {label}
            </option>
          ))}
        </select>
      </label>
      {scope === "day" && (
        <input type="date" value={day} min={localDay(first)} max={localDay(last)} onChange={(e) => setDay(e.target.value)} />
      )}
      {scope === "month" && (
        <input
          type="month"
          value={month}
          min={localDay(first).slice(0, 7)}
          max={localDay(last).slice(0, 7)}
          onChange={(e) => setMonth(e.target.value)}
        />
      )}
      {scope === "custom" && (
        <div className="row">
          <input type="datetime-local" value={from} onChange={(e) => setFrom(e.target.value)} aria-label="From" />
          <span className="muted">to</span>
          <input type="datetime-local" value={to} onChange={(e) => setTo(e.target.value)} aria-label="To" />
        </div>
      )}
      {window && !windowEmpty && <p className="muted">{formatWindow(window)}, within the database's bounds.</p>}
      <p>
        {empty
          ? "Nothing in this window to export."
          : estimate
            ? `About ${estimate.frames.toLocaleString()} frames and ${estimate.events.toLocaleString()} ` +
              `events: an archive of roughly ${formatBytes(estimate.approx_bytes)}.`
            : unestimated
              ? `No estimate: ${unestimated}`
              : "Estimating…"}
      </p>
    </div>
  );
}

export interface ImportHandle {
  /** Upload the chosen file, resolving once it is in place; throws with the refusal. */
  submit: () => Promise<Imported>;
}

/**
 * The file, read for its manifest before anything is sent, and the upload's
 * progress. For a new database, its name too.
 */
export function ImportForm({
  target,
  handle,
}: {
  /** The database to replace; absent for a new one. */
  target?: string;
  handle: RefObject<ImportHandle | null>;
}) {
  const [name, setName] = useState("");
  const [file, setFile] = useState<File | null>(null);
  const [manifest, setManifest] = useState<ArchiveManifest | null>(null);
  const [unreadable, setUnreadable] = useState("");
  const [sent, setSent] = useState<[number, number] | null>(null);

  const choose = async (f: File | null) => {
    setFile(f);
    setManifest(null);
    setUnreadable("");
    if (!f) return;
    try {
      setManifest(await readManifest(f));
    } catch (e) {
      setUnreadable(e instanceof Error ? e.message : String(e));
    }
  };

  useImperativeHandle(handle, () => ({
    submit: async () => {
      const into = target ?? name;
      if (!into) throw new Error("Name the new database.");
      if (!file || !manifest) throw new Error(unreadable || "Choose an archive.");
      const query = target ? "?replace=true" : "";
      setSent([0, file.size]);
      try {
        return await upload<Imported>(`/v1/databases/${into}/import${query}`, file, (n, total) =>
          setSent([n, total]),
        );
      } finally {
        setSent(null);
      }
    },
  }));

  const rows = (table: string) => manifest?.tables.find((t) => t.name === table)?.rows ?? 0;
  return (
    <div className="archive-form">
      {target ? (
        <p>
          Replace everything in "{target}" with the archive. This cannot be undone. Ingest for it is held
          meanwhile and stored in the new one.
        </p>
      ) : (
        <input
          placeholder="new_capture_name"
          value={name}
          onChange={(e) => setName(e.target.value.toLowerCase())}
          pattern="[a-z][a-z0-9_]*"
          aria-label="New database name"
        />
      )}
      <input type="file" accept=".wtar" onChange={(e) => choose(e.target.files?.[0] ?? null)} aria-label="Archive" />
      {unreadable && <p className="error">{unreadable}</p>}
      {manifest && (
        <p className="muted">
          "{manifest.database}", exported {new Date(manifest.created_at).toLocaleString()}:{" "}
          {rows("capture_frame").toLocaleString()} frames and {rows("events").toLocaleString()} events,{" "}
          {manifest.window ? `from ${formatWindow(manifest.window)}` : "the whole database"}.
        </p>
      )}
      {target && manifest?.window && (
        <p className="error">
          This archive holds only {formatWindow(manifest.window)}. After the replace, "{target}" holds only that
          window: everything else in it now is deleted.
        </p>
      )}
      {sent && (
        <div className="upload">
          <div className="progress">
            <div style={{ width: `${sent[1] ? (100 * sent[0]) / sent[1] : 0}%` }} />
          </div>
          <span className="muted">
            {sent[0] < sent[1]
              ? `Uploading: ${formatBytes(sent[0])} of ${formatBytes(sent[1])}`
              : "Uploaded. Checking it and putting it in place…"}
          </span>
        </div>
      )}
    </div>
  );
}
