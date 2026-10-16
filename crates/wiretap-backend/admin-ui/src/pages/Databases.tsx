import { useRef, useState } from "react";
import { DatabaseEntry, api, apiUrl, formatBytes, formatElapsed } from "../api";
import { useConfirm } from "../Confirm";
import Menu from "../Menu";
import {
  MigrationProgressView,
  RollupBadge,
  SchemaBadge,
  canMigrate,
  rollupNeedsWork,
} from "../SchemaBadge";
import { useDatabases } from "../useDatabases";
import { ExportChoice, ExportForm, ImportForm, ImportHandle } from "./DatabaseArchive";

export default function Databases() {
  const { databases, target, error, setError, refresh } = useDatabases();
  const [name, setName] = useState("");
  const [creating, setCreating] = useState(false);
  const [notice, setNotice] = useState("");
  const [dialog, ask] = useConfirm();
  const exportChoice = useRef<ExportChoice>({ window: null, empty: true });
  const importer = useRef<ImportHandle>(null);

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

  const migrate = (dbName: string) =>
    ask({
      title: "Migrate database",
      message:
        `Migrate "${dbName}" to schema v${target} now? Until it finishes it refuses ` +
        `reads, which can take hours on a large archive. Ingest is buffered and ` +
        `stored once it is done.`,
      verb: "Migrate",
      act: async () => {
        setError("");
        await api(`/v1/databases/${dbName}/migrate`, { method: "POST" });
        refresh();
      },
    });

  const exportDb = (dbName: string) =>
    ask({
      title: `Export "${dbName}"`,
      message: <ExportForm db={dbName} choice={exportChoice} />,
      verb: "Export",
      act: async () => {
        const { window: span, empty } = exportChoice.current;
        if (empty) throw new Error("There is nothing in this window to export.");
        const { url } = await api<{ url: string }>(`/v1/databases/${dbName}/export`, {
          method: "POST",
          body: span ?? {},
        });
        // An attachment: the page stays, and the browser saves the stream.
        window.location.assign(apiUrl(url));
      },
    });

  const importInto = (replacing?: string) =>
    ask({
      title: replacing ? `Replace "${replacing}" from an archive` : "Import an archive as a new database",
      message: <ImportForm target={replacing} handle={importer} />,
      verb: replacing ? "Replace" : "Import",
      danger: !!replacing,
      act: async () => {
        setError("");
        setNotice("");
        const done = await importer.current!.submit();
        const frames = done.rows.capture_frame ?? 0;
        setNotice(
          `${done.replaced ? "Replaced" : "Created"} "${done.database}" from "${done.source}": ` +
            `${frames.toLocaleString()} frames and ${(done.rows.events ?? 0).toLocaleString()} events.`,
        );
        if (done.attention) setError(done.attention);
        refresh();
      },
    });

  const remove = (dbName: string) =>
    ask({
      title: "Delete database",
      message:
        `Delete database "${dbName}"? This permanently removes all its captured ` +
        `frames and cannot be undone.`,
      verb: "Delete",
      danger: true,
      act: async () => {
        setError("");
        await api(`/v1/databases/${dbName}`, { method: "DELETE" });
        refresh();
      },
    });

  const unavailable = (d: DatabaseEntry) =>
    d.import_busy_secs !== null
      ? "an import is running"
      : d.schema_state !== "current"
        ? "not at the current schema"
        : undefined;

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
        <button className="btn" onClick={() => importInto()}>
          Import…
        </button>
      </div>
      {error && <p className="error">{error}</p>}
      {notice && <p className="muted">{notice}</p>}
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
            <tr key={d.name}>
              <td className="mono">{d.name}</td>
              <td>{formatBytes(d.size_bytes)}</td>
              <td>
                {d.import_busy_secs !== null ? (
                  <span className="badge migrating" title="Reads are refused, and ingest held, until it is in place">
                    Importing {formatElapsed(d.import_busy_secs)}
                  </span>
                ) : (
                  <SchemaBadge db={d} target={target} />
                )}{" "}
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
                <Menu
                  label={`Actions for database ${d.name}`}
                  items={[
                    { label: "Export…", onSelect: () => exportDb(d.name), unavailable: unavailable(d) },
                    {
                      label: "Import (replace)…",
                      onSelect: () => importInto(d.name),
                      unavailable: unavailable(d),
                    },
                    "separator",
                    { label: "Delete…", onSelect: () => remove(d.name), danger: true },
                  ]}
                />
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
      <p className="muted" style={{ marginBottom: 0 }}>
        Export saves a database, or a window of it, as an archive; Import makes a new
        database from one, or replaces an existing one with it. A replace refuses the
        database's reads while it runs, holds its ingest, and stores that in the new
        one. The gateway's own database, which holds its keys and catalogues, is not
        listed and takes no captures.
      </p>
      {dialog}
    </div>
  );
}
