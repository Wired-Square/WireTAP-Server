/**
 * What `wiretap-server`'s control socket answers, as this UI passes it on: a
 * reading is the daemon's body marked ready, or why the daemon is not
 * answering, the union the kit's `Capability` takes.
 */
export type Reading<T> = (T & { state: "ready" }) | { state: "unsupported"; because: string };

export interface Status {
  version: string;
  daemon_id: string | null;
  uptime_secs: number;
  applying: boolean;
  devices: Device[];
  forward: { enabled: boolean; gateway: string | null; links: Link[] };
  ingest: { enabled: boolean; listen: string | null; clients: number | null };
  gvret: { enabled: boolean; listen: string; clients: number | null };
  warnings: string[];
}

export interface Device {
  interface: string;
  kind: "can" | "serial";
  bus: number;
  mode: "passive" | "active";
  state: "starting" | "up" | "waiting" | "error";
  frames: number;
}

export interface Link {
  database: string;
  connected: boolean;
  last_ack_unix_ms: number | null;
  cache_pending: number;
  enqueued: number;
  written: number;
  dropped: number;
}

export interface Settings {
  settings: Setting[];
  warnings: string[];
}

export interface Setting {
  key: string;
  source: "default" | "ui" | "file" | "env" | "flag";
  pinned: boolean;
  /** Where a pinned value is pinned: the config file's path, `env X` or `flag --x`. */
  where?: string;
  /** Absent for a secret, which says only whether it is `set`. */
  value?: unknown;
  set?: boolean;
}

export interface ListedDevice {
  interface: string;
  kind: "can" | "serial";
  mode: "passive" | "active";
  fd: boolean;
  database: string | null;
  bus: number;
}
