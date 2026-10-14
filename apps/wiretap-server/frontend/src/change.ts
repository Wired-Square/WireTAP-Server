/**
 * The settings form's fields, and the change it sends: only what was changed,
 * never a pinned key, a secret only when one was typed, and an emptied field
 * as `null`, which hands the setting back to the default.
 */
import type { ListedDevice, Setting } from "./daemon";

export type Kind = "flag" | "text" | "whole" | "number" | "level" | "secret";

export interface Field {
  key: string;
  label: string;
  kind: Kind;
  hint?: string;
}

export interface Group {
  title: string;
  blurb: string;
  fields: Field[];
}

export const LEVELS = ["DEBUG", "INFO", "WARNING", "ERROR"];

export const GROUPS: Group[] = [
  {
    title: "Gateway",
    blurb: "Where captured frames are forwarded and archived.",
    fields: [
      { key: "forward.enable", label: "Forward to a gateway", kind: "flag" },
      { key: "forward.host", label: "Gateway host", kind: "text" },
      { key: "forward.port", label: "Gateway port", kind: "whole" },
      {
        key: "forward.database",
        label: "Database",
        kind: "text",
        hint: "Empty is the gateway's default database.",
      },
      { key: "forward.api_key", label: "Gateway key", kind: "secret" },
    ],
  },
  {
    title: "Batching",
    blurb: "How frames are grouped on the way to the gateway, and how much is kept on disk while it is away.",
    fields: [
      { key: "forward.batch_size", label: "Batch size, frames", kind: "whole" },
      { key: "forward.flush_interval", label: "Flush interval, seconds", kind: "number" },
      { key: "forward.queue_size", label: "Queue size, frames", kind: "whole" },
      { key: "forward.cache_max_mb", label: "Disk cache limit, MB", kind: "whole" },
      { key: "forward.queue_flush_pct", label: "Queue flush threshold, %", kind: "whole" },
    ],
  },
  {
    title: "Device push",
    blurb: "Frames that devices on the network push to this daemon.",
    fields: [
      { key: "ingest.enable", label: "Accept device push", kind: "flag" },
      { key: "ingest.port", label: "Device push port", kind: "whole" },
      { key: "ingest.token", label: "Device push token", kind: "secret" },
    ],
  },
  {
    title: "Logging",
    blurb: "How much the daemon writes to its log.",
    fields: [{ key: "logging.level", label: "Log level", kind: "level" }],
  },
];

export type Draft = Record<string, string | boolean>;

export interface DeviceDraft {
  interface: string;
  fd: boolean;
  database: string;
}

export type Change = { body: Record<string, unknown> } | { refused: string };

export function setting(settings: Setting[], key: string): Setting | undefined {
  return settings.find((s) => s.key === key);
}

export function draftOf(settings: Setting[]): Draft {
  const draft: Draft = {};
  for (const field of GROUPS.flatMap((g) => g.fields)) {
    const value = setting(settings, field.key)?.value;
    draft[field.key] =
      field.kind === "flag" ? value === true : field.kind === "secret" ? "" : String(value ?? "");
  }
  return draft;
}

export function devicesOf(settings: Setting[]): DeviceDraft[] {
  const listed = (setting(settings, "devices")?.value ?? []) as ListedDevice[];
  return listed.map((d) => ({ interface: d.interface, fd: d.fd, database: d.database ?? "" }));
}

/** What `PUT /api/capture/settings` is sent for `draft`, `removing` and `devices`. */
export function changes(
  settings: Setting[],
  draft: Draft,
  removing: readonly string[],
  devices: DeviceDraft[],
): Change {
  const body: Record<string, unknown> = {};
  for (const field of GROUPS.flatMap((g) => g.fields)) {
    const now = setting(settings, field.key);
    if (!now || now.pinned) continue;
    const typed = draft[field.key];
    if (field.kind === "flag") {
      if (typed !== (now.value === true)) body[field.key] = typed;
      continue;
    }
    const text = String(typed ?? "").trim();
    if (field.kind === "secret") {
      if (removing.includes(field.key)) body[field.key] = null;
      else if (text !== "") body[field.key] = text;
      continue;
    }
    if (text === String(now.value ?? "")) continue;
    if (text === "") {
      body[field.key] = null;
      continue;
    }
    if (field.kind === "text" || field.kind === "level") {
      body[field.key] = text;
      continue;
    }
    const number = Number(text);
    if (!Number.isFinite(number) || (field.kind === "whole" && !Number.isInteger(number))) {
      return { refused: `${field.label} must be a ${field.kind === "whole" ? "whole " : ""}number.` };
    }
    body[field.key] = number;
  }

  const listed = setting(settings, "devices");
  if (listed && !listed.pinned) {
    const next = devices.map((d) => ({
      interface: d.interface.trim(),
      fd: d.fd,
      database: d.database.trim(),
    }));
    if (JSON.stringify(next) !== JSON.stringify(devicesOf(settings))) {
      const names = next.map((d) => d.interface);
      if (names.includes("")) return { refused: "Each device needs an interface name." };
      if (new Set(names).size < names.length) return { refused: "An interface is listed twice." };
      body.devices = next.map((d) => ({ ...d, database: d.database || null }));
    }
  }
  return { body };
}
