import { useCallback, useEffect, useState } from "react";
import {
  api,
  ApiError,
  CatalogFinding,
  Daemon,
  DaemonDevice,
  DaemonList,
  StoredCatalog,
} from "../api";

const short = (sha: string) => sha.slice(0, 8);

/** What the daemon frames with, measured against what is assigned. */
function activeLabel(d: DaemonDevice): { text: string; badge: string } {
  const { assignment, active } = d;
  if (assignment) {
    if (active?.source === "assigned" && active.blob_sha === assignment.blob_sha) {
      return { text: "applied", badge: "ok" };
    }
    if (active?.refused?.blob_sha === assignment.blob_sha) {
      return { text: `refused: ${active.refused.reason}`, badge: "revoked" };
    }
    return { text: "pending", badge: "pending" };
  }
  if (active?.source === "local" && active.blob_sha) {
    return { text: `local ${short(active.blob_sha)}`, badge: "pending" };
  }
  return { text: "none", badge: "pending" };
}

type Panel =
  | { kind: "assign"; daemon: string; device: DaemonDevice }
  | { kind: "view"; daemon: string; catalogue: StoredCatalog };

/** A refused PUT or DELETE, as the page shows it. */
interface Refusal {
  message: string;
  findings: CatalogFinding[];
}

function refusal(e: unknown): Refusal {
  if (!(e instanceof ApiError)) {
    return { message: e instanceof Error ? e.message : String(e), findings: [] };
  }
  if (e.status === 409) {
    const current = (e.body as { current: string | null }).current;
    const now = current ? short(current) : "nothing";
    return { message: `Changed meanwhile: ${now} is assigned now. Try again.`, findings: [] };
  }
  const findings = (e.body as { findings?: CatalogFinding[] } | null)?.findings ?? [];
  return { message: e.message, findings };
}

export default function Daemons() {
  const [daemons, setDaemons] = useState<Daemon[]>([]);
  const [error, setError] = useState<Refusal | null>(null);
  const [panel, setPanel] = useState<Panel | null>(null);

  const refresh = useCallback(
    () =>
      api<DaemonList>("/v1/admin/daemons")
        .then((r) => setDaemons(r.daemons))
        .catch((e) => setError(refusal(e))),
    [],
  );

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 5000);
    return () => clearInterval(t);
  }, [refresh]);

  const run = async (action: () => Promise<unknown>) => {
    setError(null);
    try {
      await action();
      setPanel(null);
    } catch (e) {
      setError(refusal(e));
    }
    refresh();
  };

  const assign = (daemon: string, device: DaemonDevice, file: File) =>
    run(async () => {
      // Not `file.text()`: that drops a byte-order mark, and the SHA-1 is
      // over the bytes as they are.
      const bytes = await file.arrayBuffer();
      const content = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes);
      await api("/v1/admin/assignments", {
        method: "PUT",
        body: {
          daemon_id: daemon,
          interface: device.interface,
          content,
          provenance: {},
          expected: device.assignment?.blob_sha ?? "",
        },
      });
    });

  const clear = (daemon: string, device: DaemonDevice) => {
    if (!device.assignment) return;
    if (!confirm(`Clear the catalogue assigned to ${device.interface} on ${daemon}?`)) return;
    const q = new URLSearchParams({
      daemon_id: daemon,
      interface: device.interface,
      expected: device.assignment.blob_sha,
    });
    run(() => api(`/v1/admin/assignments?${q}`, { method: "DELETE" }));
  };

  const view = async (daemon: string, sha: string) => {
    setError(null);
    try {
      const catalogue = await api<StoredCatalog>(`/v1/admin/catalogs/${sha}`);
      setPanel({ kind: "view", daemon, catalogue });
    } catch (e) {
      setError(refusal(e));
    }
  };

  return (
    <>
      {error && (
        <div className="card">
          <p className="error">{error.message}</p>
          {error.findings.length > 0 && (
            <ul className="findings">
              {error.findings.map((f, i) => (
                <li key={i}>
                  <span className="mono">{f.field}</span>: {f.message}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
      {daemons.map((d) => (
        <div className="card" key={d.daemon_id}>
          <h2 className="mono">{d.daemon_id}</h2>
          <table>
            <thead>
              <tr>
                <th>Interface</th>
                <th>Bus</th>
                <th>Database</th>
                <th>Last seen</th>
                <th>Assigned</th>
                <th>Active</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {d.devices.map((dev) => {
                const active = activeLabel(dev);
                return (
                  <tr key={dev.interface}>
                    <td className="mono">{dev.interface}</td>
                    <td>{dev.bus ?? "—"}</td>
                    <td className="mono">{dev.database ?? "—"}</td>
                    <td className="muted">
                      {dev.last_seen_us === null
                        ? "never"
                        : new Date(dev.last_seen_us / 1000).toLocaleString()}
                    </td>
                    <td>
                      {dev.assignment ? (
                        <span
                          title={`by ${dev.assignment.assigned_by ?? "unknown"}, ${new Date(
                            dev.assignment.assigned_at_us / 1000,
                          ).toLocaleString()}`}
                        >
                          {dev.assignment.name ?? "unnamed"}{" "}
                          <span className="mono muted">{short(dev.assignment.blob_sha)}</span>
                        </span>
                      ) : (
                        <span className="muted">—</span>
                      )}
                    </td>
                    <td>
                      <span className={`badge ${active.badge}`}>{active.text}</span>
                    </td>
                    <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                      <button
                        className="btn"
                        onClick={() => setPanel({ kind: "assign", daemon: d.daemon_id, device: dev })}
                      >
                        Assign…
                      </button>
                      {dev.assignment && (
                        <>
                          {" "}
                          <button
                            className="btn"
                            onClick={() => view(d.daemon_id, dev.assignment!.blob_sha)}
                          >
                            View
                          </button>{" "}
                          <button className="btn danger" onClick={() => clear(d.daemon_id, dev)}>
                            Clear
                          </button>
                        </>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>

          {panel?.daemon === d.daemon_id && panel.kind === "assign" && (
            <div className="card" style={{ marginTop: "0.75rem", marginBottom: 0 }}>
              <div className="row">
                <span>
                  Assign a catalogue to <span className="mono">{panel.device.interface}</span>
                </span>
                <input
                  type="file"
                  accept=".toml"
                  onChange={(e) => {
                    const file = e.target.files?.[0];
                    if (file) assign(d.daemon_id, panel.device, file);
                  }}
                />
                <div className="spacer" />
                <button className="btn" onClick={() => setPanel(null)}>
                  Cancel
                </button>
              </div>
            </div>
          )}

          {panel?.daemon === d.daemon_id && panel.kind === "view" && (
            <div className="card" style={{ marginTop: "0.75rem", marginBottom: 0 }}>
              <div className="row" style={{ marginBottom: "0.5rem" }}>
                <span className="mono">{panel.catalogue.blob_sha}</span>
                <div className="spacer" />
                <button className="btn" onClick={() => setPanel(null)}>
                  Close
                </button>
              </div>
              <pre className="catalogue">{panel.catalogue.content}</pre>
            </div>
          )}
        </div>
      ))}
      {daemons.length === 0 && !error && (
        <div className="card muted">
          No capture daemon has named itself yet. (Refreshes every 5 s.)
        </div>
      )}
    </>
  );
}
