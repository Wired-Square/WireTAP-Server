import { useEffect, useState } from "react";
import { api, LogRecord } from "../api";

/** Most severe first, matching the server's own ordering. */
const LEVELS = ["ERROR", "WARN", "INFO", "DEBUG"] as const;

/** Existing badge colours: red for failure, brand orange for attention, grey
 *  for the ordinary. Not `badge error` — `.error` is the page-level message
 *  class and carries a margin. */
const LEVEL_CLASS: Record<string, string> = {
  ERROR: "revoked",
  WARN: "behind",
  INFO: "pending",
};

export default function Logging() {
  const [records, setRecords] = useState<LogRecord[]>([]);
  const [capacity, setCapacity] = useState(0);
  const [level, setLevel] = useState("");
  const [error, setError] = useState("");

  useEffect(() => {
    const q = level ? `?level=${level}` : "";
    const tick = () =>
      api<{ records: LogRecord[]; capacity: number }>(`/v1/admin/logs${q}`)
        .then((r) => {
          setRecords(r.records);
          setCapacity(r.capacity);
          setError("");
        })
        .catch((e) => setError(String(e.message ?? e)));
    tick();
    const t = setInterval(tick, 3000);
    return () => clearInterval(t);
  }, [level]);

  return (
    <div className="card">
      <div className="row">
        <h2 style={{ margin: 0 }}>Server log</h2>
        <div className="spacer" />
        <select value={level} onChange={(e) => setLevel(e.target.value)}>
          <option value="">All levels</option>
          {LEVELS.map((l) => (
            <option key={l} value={l}>
              {l} and above
            </option>
          ))}
        </select>
      </div>
      {error && <p className="error">{error}</p>}
      <table>
        <thead>
          <tr>
            <th>Time</th>
            <th>Level</th>
            <th>Target</th>
            <th>Message</th>
          </tr>
        </thead>
        <tbody>
          {records.map((r) => (
            <tr key={r.seq}>
              <td className="muted mono">{new Date(r.ts).toLocaleTimeString()}</td>
              <td>
                <span className={`badge ${LEVEL_CLASS[r.level] ?? "debug"}`}>{r.level}</span>
              </td>
              <td className="muted mono">{r.target}</td>
              <td
                className="mono clip"
                title={r.fields ? `${r.message}\n${r.fields}` : r.message}
                style={{ maxWidth: 620 }}
              >
                {r.message}
                {r.fields && <span className="muted"> {r.fields}</span>}
              </td>
            </tr>
          ))}
          {records.length === 0 && (
            <tr>
              <td colSpan={4} className="muted">
                Nothing logged at this level yet. (Refreshes every 3 s.)
              </td>
            </tr>
          )}
        </tbody>
      </table>
      <p className="muted">
        The newest {capacity.toLocaleString()} records, held in memory and lost on restart —
        the container's stdout is the durable record. Routine traffic (health checks, the
        admin bundle, the tabs' own polling) is logged at DEBUG and needs{" "}
        <span className="mono">RUST_LOG=wiretap_backend=debug</span> to appear.
      </p>
    </div>
  );
}
