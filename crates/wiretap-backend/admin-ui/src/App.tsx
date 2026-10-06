import { useState } from "react";
import { api, clearKey, getKey, setKey } from "./api";
import { canMigrate } from "./SchemaBadge";
import { useDatabases } from "./useDatabases";
import Activity from "./pages/Activity";
import Daemons from "./pages/Daemons";
import Databases from "./pages/Databases";
import Health from "./pages/Health";
import Ingest from "./pages/Ingest";
import Keys from "./pages/Keys";
import Logging from "./pages/Logging";

const TABS = ["Keys", "Databases", "Ingest", "Daemons", "Activity", "Logging", "Health"] as const;
type Tab = (typeof TABS)[number];

export default function App() {
  const [authed, setAuthed] = useState(() => getKey() !== null);
  const [tab, setTab] = useState<Tab>("Keys");

  if (!authed) {
    return <Login onAuthed={() => setAuthed(true)} />;
  }

  return (
    <>
      <div className="topbar">
        <h1>
          <img className="logo" src="/admin/logo.svg" alt="" />
          Wire<span>TAP</span> Backend
        </h1>
        <nav className="tabs">
          {TABS.map((t) => (
            <button key={t} className={t === tab ? "active" : ""} onClick={() => setTab(t)}>
              {t}
            </button>
          ))}
        </nav>
        <button
          className="btn"
          onClick={() => {
            clearKey();
            setAuthed(false);
          }}
        >
          Sign out
        </button>
      </div>
      <MigrationBanner
        key={tab}
        onOpen={(name) => {
          setTab("Databases");
          requestAnimationFrame(() =>
            document.getElementById(`db-${name}`)?.scrollIntoView({ block: "center" }),
          );
        }}
      />
      {tab === "Keys" && <Keys />}
      {tab === "Databases" && <Databases />}
      {tab === "Ingest" && <Ingest />}
      {tab === "Daemons" && <Daemons />}
      {tab === "Activity" && <Activity />}
      {tab === "Logging" && <Logging />}
      {tab === "Health" && <Health />}
    </>
  );
}

/** Names every database waiting for an operator's Migrate now, on every tab. */
function MigrationBanner({ onOpen }: { onOpen: (name: string) => void }) {
  const { databases } = useDatabases();
  const waiting = databases.filter(canMigrate);
  if (waiting.length === 0) return null;
  return (
    <div className="card banner">
      {waiting.length === 1 ? "A database is" : `${waiting.length} databases are`} waiting
      for migration:{" "}
      {waiting.map((d) => (
        <button key={d.name} className="btn mono" onClick={() => onOpen(d.name)}>
          {d.name}
        </button>
      ))}{" "}
      They refuse reads, and buffer ingest, until migrated with Migrate now.
    </div>
  );
}

function Login({ onAuthed }: { onAuthed: () => void }) {
  const [key, setKeyInput] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    setBusy(true);
    setError("");
    setKey(key.trim());
    try {
      await api("/v1/admin/keys"); // cheapest admin-role check
      onAuthed();
    } catch (e) {
      clearKey();
      setError(e instanceof Error ? e.message : "login failed");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="login">
      <h1>
        Wire<span>TAP</span> Backend
      </h1>
      <p className="muted">Paste an admin API key to continue.</p>
      <input
        type="password"
        placeholder="Admin API key"
        value={key}
        autoFocus
        onChange={(e) => setKeyInput(e.target.value)}
        onKeyDown={(e) => e.key === "Enter" && key && submit()}
      />
      {error && <p className="error">{error}</p>}
      <button className="btn primary" disabled={!key || busy} onClick={submit}>
        Sign in
      </button>
    </div>
  );
}
