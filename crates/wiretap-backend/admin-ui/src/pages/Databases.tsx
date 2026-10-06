import { useState } from "react";
import { api, formatBytes } from "../api";
import {
  MigrationProgressView,
  RollupBadge,
  SchemaBadge,
  canMigrate,
  rollupNeedsWork,
} from "../SchemaBadge";
import { useDatabases } from "../useDatabases";

export default function Databases() {
  const { databases, target, error, setError, refresh } = useDatabases();
  const [name, setName] = useState("");
  const [creating, setCreating] = useState(false);

  const create = async () => {
    setCreating(true);
    setError("");
    try {
      await api("/v1/databases", { method: "POST", body: { name } });
      setName("");
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setCreating(false);
    }
  };

  const rebuild = async (dbName: string) => {
    setError("");
    try {
      await api(`/v1/databases/${dbName}/rollup/refresh`, { method: "POST" });
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const migrate = async (dbName: string) => {
    if (
      !confirm(
        `Migrate "${dbName}" to schema v${target} now? Until it finishes it refuses ` +
          `reads, which can take hours on a large archive. Ingest is buffered and ` +
          `stored once it is done.`,
      )
    )
      return;
    setError("");
    try {
      await api(`/v1/databases/${dbName}/migrate`, { method: "POST" });
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const remove = async (dbName: string) => {
    if (
      !confirm(
        `Delete database "${dbName}"? This permanently removes all its captured ` +
          `frames and cannot be undone.`,
      )
    )
      return;
    setError("");
    try {
      await api(`/v1/databases/${dbName}`, { method: "DELETE" });
      refresh();
    } catch (e) {
      // e.g. 409 when a device is actively ingesting into it
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div className="card">
      <div className="row" style={{ marginBottom: "0.75rem" }}>
        <h2 style={{ margin: 0 }}>Capture databases</h2>
        <div className="spacer" />
        <input
          placeholder="new_capture_name"
          value={name}
          onChange={(e) => setName(e.target.value.toLowerCase())}
          pattern="[a-z][a-z0-9_]*"
        />
        <button className="btn primary" disabled={!name || creating} onClick={create}>
          Create
        </button>
      </div>
      {error && <p className="error">{error}</p>}
      <table>
        <thead>
          <tr>
            <th>Name</th>
            <th>Size</th>
            <th>Schema</th>
            <th>Rollup</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {databases.map((d) => (
            <tr key={d.name} id={`db-${d.name}`}>
              <td className="mono">{d.name}</td>
              <td>{formatBytes(d.size_bytes)}</td>
              <td>
                <SchemaBadge db={d} target={target} />{" "}
                {canMigrate(d) && (
                  <button className="btn" onClick={() => migrate(d.name)}>
                    Migrate now
                  </button>
                )}
                <MigrationProgressView db={d} />
              </td>
              <td>
                <RollupBadge db={d} />{" "}
                {rollupNeedsWork(d) && (
                  <button className="btn" onClick={() => rebuild(d.name)}>
                    Rebuild
                  </button>
                )}
              </td>
              <td style={{ textAlign: "right" }}>
                <button className="btn danger" onClick={() => remove(d.name)}>
                  Delete
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="muted" style={{ marginBottom: 0 }}>
        Ingest devices can also auto-create a database by naming one in their
        HELLO message (when auto-create is enabled). A database behind the current
        schema waits for Migrate now, or is migrated on start with
        WIRETAP_AUTO_MIGRATE=true. It refuses reads until the migration is done; ingest
        is buffered meanwhile and stored when it finishes. Rebuilding a rollup does not block anything — it takes
        minutes on a large archive, and until it finishes the buckets it has not
        reached are recomputed on every query. A rollup a few hours behind is the
        maintenance policy working normally. "incomplete" is not: the stored
        summaries do not reach this archive's first frame, so rollup queries are
        omitting the gap rather than recomputing it. The gateway repairs that on
        start; the button is for a rollup that has merely fallen behind.
      </p>
    </div>
  );
}
