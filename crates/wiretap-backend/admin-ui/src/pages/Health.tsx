import { useEffect, useState } from "react";
import { api, DatabaseEntry } from "../api";
import { SchemaBadge } from "../SchemaBadge";
import { useDatabases } from "../useDatabases";

interface HealthInfo {
  status: string;
  version: string;
  db_ok: boolean;
}

/**
 * In a settled deployment every database is on the same version, so say that in
 * one line. The exceptions — a sweep in progress, a partial failure, an archive
 * restored from a backup — are the whole reason to look, so those expand.
 */
function SchemaSummary({ databases, target }: { databases: DatabaseEntry[]; target: number }) {

  // `unknown` means the gateway has never managed this database — a cluster can
  // hold databases nothing to do with it, and counting those would report an
  // otherwise settled deployment as mixed forever.
  const managed = databases.filter((d) => d.schema_state !== "unknown");
  const odd = managed.filter(
    (d) => !(d.schema_state === "current" && d.schema_version === target),
  );
  if (managed.length === 0) return <span className="muted">no capture databases</span>;
  if (odd.length === 0) {
    return (
      <>
        <span className="badge ok">v{target}</span>{" "}
        <span className="muted">
          all {managed.length} database{managed.length === 1 ? "" : "s"}
        </span>
      </>
    );
  }
  return (
    <>
      <span className={`badge ${odd.some((d) => d.schema_state === "failed") ? "revoked" : "migrating"}`}>
        mixed
      </span>{" "}
      <span className="muted">
        {managed.length - odd.length} of {managed.length} at v{target}
      </span>
      <table style={{ marginTop: "0.4rem" }}>
        <tbody>
          {odd.map((d) => (
            <tr key={d.name}>
              <td className="mono">{d.name}</td>
              <td>
                <SchemaBadge db={d} target={target} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  );
}

export default function Health() {
  const [health, setHealth] = useState<HealthInfo | null>(null);
  const [healthError, setHealthError] = useState("");
  // Names and per-database versions come from the keyed endpoint; /v1/health is
  // unauthenticated and deliberately carries only a consensus word.
  const { databases, target, error } = useDatabases();

  useEffect(() => {
    api<HealthInfo>("/v1/health")
      .then(setHealth)
      .catch((e) => setHealthError(String(e.message ?? e)));
  }, [databases]);

  return (
    <div className="card">
      <h2>Health</h2>
      {(healthError || error) && <p className="error">{healthError || error}</p>}
      {health && (
        <table>
          <tbody>
            <tr>
              <td className="muted">Status</td>
              <td>
                <span className={`badge ${health.status === "ok" ? "ok" : "revoked"}`}>
                  {health.status}
                </span>
              </td>
            </tr>
            <tr>
              <td className="muted">Backend version</td>
              <td className="mono">{health.version}</td>
            </tr>
            <tr>
              <td className="muted">Database reachable</td>
              <td>
                <span className={`badge ${health.db_ok ? "ok" : "revoked"}`}>
                  {health.db_ok ? "yes" : "no"}
                </span>
              </td>
            </tr>
            <tr>
              <td className="muted">Schema</td>
              <td>
                <SchemaSummary databases={databases} target={target} />
              </td>
            </tr>
          </tbody>
        </table>
      )}
    </div>
  );
}
