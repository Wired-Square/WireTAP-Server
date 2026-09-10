import { useCallback, useEffect, useState } from "react";
import { api, DatabaseEntry, DatabaseList } from "./api";
import { isBusy } from "./SchemaBadge";

/**
 * The database list, plus the schema version they are all headed for, plus a
 * poll that runs only while something is unsettled.
 *
 * Shared by Databases and Health: both need the same fetch and the same polling
 * rule, and a settled deployment should not be polled at all.
 */
export function useDatabases() {
  const [databases, setDatabases] = useState<DatabaseEntry[]>([]);
  const [target, setTarget] = useState(0);
  const [error, setError] = useState("");

  const refresh = useCallback(() => {
    api<DatabaseList>("/v1/databases")
      .then((r) => {
        setDatabases(r.databases);
        setTarget(r.schema_version);
      })
      .catch((e) => setError(String(e.message ?? e)));
  }, []);

  useEffect(refresh, [refresh]);

  // Depends on the boolean, not the array: the response replaces the array
  // identity every time, which would tear down and recreate the interval on
  // each poll and stretch the period to "2s after the response".
  const busy = databases.some(isBusy);
  useEffect(() => {
    if (!busy) return;
    const t = setInterval(refresh, 2000);
    return () => clearInterval(t);
  }, [busy, refresh]);

  return { databases, target, error, setError, refresh };
}
