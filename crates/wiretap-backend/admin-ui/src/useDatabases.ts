import { useCallback, useEffect, useState } from "react";
import { api, DatabaseEntry, DatabaseList } from "./api";
import { isBusy } from "./SchemaBadge";

/** Fired by `refresh`, so every view of the list fetches it again at once. */
const CHANGED = "wiretap:databases-changed";

/**
 * The database list, plus the schema version they are all headed for, plus a
 * poll that runs only while something is unsettled, or while `pollWhile`
 * holds for a database.
 *
 * Shared by Databases, Health and the migration banner: they need the same
 * fetch, and a settled deployment should not be polled at all.
 */
export function useDatabases(pollWhile?: (d: DatabaseEntry) => boolean) {
  const [databases, setDatabases] = useState<DatabaseEntry[]>([]);
  const [target, setTarget] = useState(0);
  const [autoMigrate, setAutoMigrate] = useState(false);
  const [error, setError] = useState("");

  const fetchList = useCallback(() => {
    api<DatabaseList>("/v1/databases")
      .then((r) => {
        setDatabases(r.databases);
        setTarget(r.schema_version);
        setAutoMigrate(r.auto_migrate);
      })
      .catch((e) => setError(String(e.message ?? e)));
  }, []);

  useEffect(() => {
    fetchList();
    window.addEventListener(CHANGED, fetchList);
    return () => window.removeEventListener(CHANGED, fetchList);
  }, [fetchList]);
  const refresh = useCallback(() => window.dispatchEvent(new Event(CHANGED)), []);

  // Depends on the boolean, not the array: the response replaces the array
  // identity every time, which would tear down and recreate the interval on
  // each poll and stretch the period to "2s after the response".
  const busy = databases.some((d) => isBusy(d, autoMigrate) || !!pollWhile?.(d));
  useEffect(() => {
    if (!busy) return;
    const t = setInterval(fetchList, 2000);
    return () => clearInterval(t);
  }, [busy, fetchList]);

  return { databases, target, error, setError, refresh };
}
