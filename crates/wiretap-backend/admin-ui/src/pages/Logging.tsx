import { useEffect, useState } from "react";
import { api, LogLevel, LogRecord, LogSnapshot } from "../api";

/** Most severe first. */
const LEVELS: LogLevel[] = ["error", "warn", "info", "debug"];

/** Existing badge colours: red for failure, brand orange for attention, grey
 *  for the ordinary. Not `badge error` — `.error` is the page-level message
 *  class and carries a margin. */
const LEVEL_CLASS: Partial<Record<LogLevel, string>> = {
  error: "revoked",
  warn: "behind",
  info: "pending",
};

const fieldText = (r: LogRecord) => r.fields.map(([k, v]) => `${k}=${v}`).join(" ");

export default function Logging() {
  const [records, setRecords] = useState<LogRecord[]>([]);
  const [level, setLevel] = useState("");
  const [error, setError] = useState("");

  useEffect(() => {
    const q = level ? `?level=${level}` : "";
    const tick = () =>
      api<LogSnapshot>(`/v1/admin/logs${q}`)
        .then((r) => {
          setRecords(r.records.reverse());
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
              {l.toUpperCase()} and above
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
          {records.map((r) => {
            const fields = fieldText(r);
            return (
              <tr key={r.seq}>
                <td className="muted mono">{new Date(r.at).toLocaleTimeString()}</td>
                <td>
                  <span className={`badge ${LEVEL_CLASS[r.level] ?? "debug"}`}>
                    {r.level.toUpperCase()}
                  </span>
                </td>
                <td className="muted mono">{r.target}</td>
                <td
                  className="mono clip"
                  title={fields ? `${r.message}\n${fields}` : r.message}
                  style={{ maxWidth: 620 }}
                >
                  {r.message}
                  {fields && <span className="muted"> {fields}</span>}
                </td>
              </tr>
            );
          })}
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
        The newest 200 records, from the last 1,000 held in memory and lost on restart —
        the container's stdout is the durable record. Routine traffic (health checks, the
        admin bundle, the tabs' own polling) is logged at DEBUG and needs{" "}
        <span className="mono">RUST_LOG=wiretap_backend=debug</span> to appear.
      </p>
    </div>
  );
}
