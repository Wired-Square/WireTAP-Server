import { useCallback, useEffect, useState } from "react";
import { api, type Caller, KeySummary } from "../api";
import Menu from "../Menu";
import SecretReveal from "../SecretReveal";
import Secrets from "./Secrets";

function NameAndPin(props: {
  name: string;
  pin: string;
  setName: (name: string) => void;
  setPin: (pin: string) => void;
}) {
  return (
    <>
      <input placeholder="Name (e.g. shed-esp32)" value={props.name} onChange={(e) => props.setName(e.target.value)} />
      <input
        placeholder="Database pin (optional)"
        value={props.pin}
        onChange={(e) => props.setPin(e.target.value)}
        title="Restrict this key to one capture database"
      />
    </>
  );
}

type Verb = "rotate" | "disable" | "enable" | "revoke";

export default function Keys({ caller }: { caller: Caller }) {
  const [keys, setKeys] = useState<KeySummary[]>([]);
  const [error, setError] = useState("");
  const [showCreate, setShowCreate] = useState(false);
  const [editing, setEditing] = useState<KeySummary | null>(null);
  const [name, setName] = useState("");
  const [role, setRole] = useState("read");
  const [pin, setPin] = useState("");
  const [revealed, setRevealed] = useState<{ name: string; key: string } | null>(null);

  const refresh = useCallback(() => {
    api<{ keys: KeySummary[] }>("/v1/admin/keys")
      .then((r) => setKeys(r.keys))
      .catch((e) => setError(String(e.message ?? e)));
  }, []);

  useEffect(refresh, [refresh]);

  const run = async (call: () => Promise<void>) => {
    setError("");
    try {
      await call();
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const openForm = (key: KeySummary | null) => {
    setEditing(key);
    setShowCreate(key === null && !showCreate);
    setName(key?.name ?? "");
    setPin(key?.database_pin ?? "");
    setRevealed(null);
  };

  const create = () =>
    run(async () => {
      const r = await api<{ id: number; key: string }>("/v1/admin/keys", {
        method: "POST",
        body: { name, role, database_pin: pin || null },
      });
      setRevealed({ name, key: r.key });
      setName("");
      setPin("");
    });

  const save = (k: KeySummary) =>
    run(async () => {
      await api(`/v1/admin/keys/${k.id}`, { method: "PATCH", body: { name, database_pin: pin || null } });
      setEditing(null);
    });

  const act = (k: KeySummary, verb: Verb) => {
    const prompts: Record<Verb, string> = {
      rotate: `Rotate key "${k.name}"? Its current value stops working at once; clients need the new one.`,
      disable: `Disable key "${k.name}"? Clients using it lose access until it is enabled again.`,
      enable: `Enable key "${k.name}"? It will work again.`,
      revoke: `Revoke key "${k.name}"? Clients using it lose access, and this cannot be undone.`,
    };
    if (!confirm(prompts[verb])) return;
    run(async () => {
      if (verb === "revoke") {
        await api(`/v1/admin/keys/${k.id}`, { method: "DELETE" });
        return;
      }
      const r = await api<{ key?: string }>(`/v1/admin/keys/${k.id}/${verb}`, { method: "POST" });
      if (r.key) setRevealed({ name: k.name, key: r.key });
    });
  };

  return (
    <>
      <div className="card">
        <div className="row" style={{ marginBottom: "0.75rem" }}>
          <h2 style={{ margin: 0 }}>API keys</h2>
          <div className="spacer" />
          <button className="btn primary" onClick={() => openForm(null)}>
            {showCreate ? "Close" : "New key"}
          </button>
        </div>

        {showCreate && (
          <div className="card">
            <div className="row">
              <NameAndPin name={name} pin={pin} setName={setName} setPin={setPin} />
              <select value={role} onChange={(e) => setRole(e.target.value)}>
                <option value="read">read — desktop / queries</option>
                <option value="ingest">ingest — capture devices</option>
                <option value="admin">admin — full control</option>
              </select>
              <button className="btn primary" disabled={!name} onClick={create}>
                Create
              </button>
            </div>
          </div>
        )}

        {editing && (
          <div className="card">
            <div className="row">
              <NameAndPin name={name} pin={pin} setName={setName} setPin={setPin} />
              <span className={`badge ${editing.role}`} title="To change a role, make a new key">
                {editing.role}
              </span>
              <button className="btn primary" disabled={!name} onClick={() => save(editing)}>
                Save
              </button>
              <button className="btn" onClick={() => setEditing(null)}>
                Cancel
              </button>
            </div>
          </div>
        )}

        {revealed && (
          <SecretReveal
            value={revealed.key}
            note={`Key "${revealed.name}": copy it now. It is shown once and never stored.`}
          />
        )}

        {error && <p className="error">{error}</p>}
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Role</th>
              <th>Database pin</th>
              <th>Last used</th>
              <th>Status</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {keys.map((k) => (
              <tr key={k.id}>
                <td>{k.name}</td>
                <td>
                  <span className={`badge ${k.role}`}>{k.role}</span>
                </td>
                <td className="mono">{k.database_pin ?? "—"}</td>
                <td className="muted">
                  {k.last_used_at ? new Date(k.last_used_at).toLocaleString() : "never"}
                </td>
                <td>
                  <span className={`badge ${k.disabled ? "revoked" : "ok"}`}>
                    {k.disabled ? "disabled" : "active"}
                  </span>
                </td>
                <td style={{ textAlign: "right" }}>
                  <Menu
                    label={`Actions for key ${k.name}`}
                    items={[
                      { label: "Edit…", onSelect: () => openForm(k) },
                      ...(k.disabled ? [] : [{ label: "Rotate…", onSelect: () => act(k, "rotate") }]),
                      k.disabled
                        ? { label: "Enable", onSelect: () => act(k, "enable") }
                        : { label: "Disable", onSelect: () => act(k, "disable") },
                      "separator",
                      { label: "Revoke…", onSelect: () => act(k, "revoke"), danger: true },
                    ]}
                  />
                </td>
              </tr>
            ))}
            {keys.length === 0 && (
              <tr>
                <td colSpan={6} className="muted">
                  No keys yet — the admin key is the only access.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      <Secrets caller={caller} />
    </>
  );
}
