import { useEffect, useState } from "react";
import { api, type Caller, clearKey, setKey } from "./api";
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
  // undefined while asking; null when a key is needed.
  const [caller, setCaller] = useState<Caller | null>();
  const [tab, setTab] = useState<Tab>("Keys");

  useEffect(() => {
    api<Caller>("/v1/whoami")
      .then(setCaller)
      .catch(() => {
        clearKey();
        setCaller(null);
      });
  }, []);

  if (caller === undefined) return null;
  if (caller === null) return <Login onAuthed={setCaller} />;
  if (caller.role !== "admin") {
    return (
      <div className="login">
        <h1>
          Wire<span>TAP</span> Backend
        </h1>
        <p className="muted">
          {caller.name} can read this gateway but not administer it. Ask an admin of this app.
        </p>
      </div>
    );
  }

  return (
    <>
      <div className="topbar">
        <h1>
          <img className="logo" src="logo.svg" alt="" />
          Wire<span>TAP</span> Backend
        </h1>
        <nav className="tabs">
          {TABS.map((t) => (
            <button key={t} className={t === tab ? "active" : ""} onClick={() => setTab(t)}>
              {t}
            </button>
          ))}
        </nav>
        {caller.via === "console" ? (
          <span className="muted">{caller.name}</span>
        ) : (
          <button
            className="btn"
            onClick={() => {
              clearKey();
              setCaller(null);
            }}
          >
            Sign out
          </button>
        )}
      </div>
      <MigrationBanner key={tab} onOpen={() => setTab("Databases")} />
      {tab === "Keys" && <Keys caller={caller} />}
      {tab === "Databases" && <Databases />}
      {tab === "Ingest" && <Ingest />}
      {tab === "Daemons" && <Daemons />}
      {tab === "Activity" && <Activity />}
      {tab === "Logging" && <Logging />}
      {tab === "Health" && <Health />}
    </>
  );
}

/** Counts every database waiting for an operator's Migrate now, on every tab. */
function MigrationBanner({ onOpen }: { onOpen: () => void }) {
  // Polled while one waits too, so a migration started elsewhere shows.
  const { databases } = useDatabases(
    (d) => d.schema_state === "pending" || d.schema_state === "migrating",
  );
  const waiting = databases.filter(canMigrate).length;
  if (waiting === 0) return null;
  return (
    <div className="card banner">
      {waiting === 1
        ? "1 database is waiting for migration. It refuses reads, and buffers ingest until migrated."
        : `${waiting} databases are waiting for migration. They refuse reads, and buffer ingest until migrated.`}{" "}
      <button className="btn" onClick={onOpen}>
        Fix
      </button>
    </div>
  );
}

function Login({ onAuthed }: { onAuthed: (caller: Caller) => void }) {
  const [key, setKeyInput] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    setBusy(true);
    setError("");
    setKey(key.trim());
    try {
      const caller = await api<Caller>("/v1/whoami");
      if (caller.role !== "admin") throw new Error("that key is not an admin key");
      onAuthed(caller);
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
