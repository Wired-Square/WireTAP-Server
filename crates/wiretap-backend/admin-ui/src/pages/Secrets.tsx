import { useEffect, useState } from "react";
import { api, type Caller, type RotatedSecret, type SecretSources, setKey } from "../api";
import { useConfirm } from "../Confirm";
import SecretReveal from "../SecretReveal";

type Secret = "db-password" | "admin-key";

const NAMES: Record<Secret, string> = { "db-password": "Database password", "admin-key": "Admin key" };

function confirmation(secret: Secret, persisted: boolean): string {
  if (secret === "db-password") {
    return persisted
      ? "Rotate the database password? The gateway changes it and saves it. Open connections stay up."
      : "Rotate the database password? It comes from the deployment's env, so the new one is not saved. " +
          "Copy it into POSTGRES_PASSWORD, for the backend and the timescaledb services, before the gateway " +
          "next restarts: until then a restarted gateway cannot reach the database.";
  }
  return persisted
    ? "Rotate the admin key? The current one stops working at once."
    : "Rotate the admin key? The current one stops working at once and stays dead, even if the gateway " +
        "restarts with it. The new one is kept across restarts; copy it into WIRETAP_ADMIN_KEY too, so the env matches.";
}

export default function Secrets({ caller }: { caller: Caller }) {
  const [sources, setSources] = useState<SecretSources | null>(null);
  const [rotated, setRotated] = useState<RotatedSecret & { name: string }>();
  const [error, setError] = useState("");
  const [dialog, ask] = useConfirm();

  const refresh = () =>
    api<SecretSources>("/v1/admin/secrets")
      .then(setSources)
      .catch((e) => setError(String(e.message ?? e)));
  useEffect(() => {
    refresh();
  }, []);

  const rotate = (secret: Secret, persisted: boolean) =>
    ask({
      title: `Rotate the ${NAMES[secret].toLowerCase()}`,
      message: confirmation(secret, persisted),
      verb: "Rotate",
      danger: true,
      act: async () => {
        setError("");
        const r = await api<RotatedSecret>(`/v1/admin/secrets/${secret}/rotate`, { method: "POST" });
        // This tab signed in with the key it just replaced.
        if (secret === "admin-key" && caller.via === "key" && caller.name === "bootstrap-admin") {
          setKey(r.secret);
        }
        setRotated({ ...r, name: NAMES[secret] });
        refresh();
      },
    });

  if (!sources) return error ? <p className="error">{error}</p> : null;
  const rows: [Secret, boolean][] = [
    ["db-password", sources.db_password.persisted],
    ["admin-key", sources.admin_key.persisted],
  ];
  return (
    <div className="card">
      <h2>Secrets</h2>
      {error && <p className="error">{error}</p>}
      <table>
        <tbody>
          {rows.map(([secret, persisted]) => (
            <tr key={secret}>
              <td>{NAMES[secret]}</td>
              <td className="muted">
                {secret === "admin-key" && !sources.admin_key.configured
                  ? "none configured: set WIRETAP_ADMIN_KEY or WIRETAP_ADMIN_KEY_FILE to have one"
                  : persisted
                  ? "saved in a file the gateway owns"
                  : "from the deployment's env: a new one must be copied there by hand"}
                {secret === "admin-key" && sources.admin_key.stale_config && (
                  <div className="error">
                    {persisted ? "The admin key file" : "The deployment's env"} still holds a rotated admin key.
                  </div>
                )}
              </td>
              <td style={{ textAlign: "right" }}>
                {(secret !== "admin-key" || sources.admin_key.configured) && (
                  <button className="btn" onClick={() => rotate(secret, persisted)}>
                    Rotate…
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {rotated && (
        <SecretReveal value={rotated.secret} note={`New ${rotated.name.toLowerCase()}, shown once. ${rotated.note}`} />
      )}
      {dialog}
    </div>
  );
}
