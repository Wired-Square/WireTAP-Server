import assert from "node:assert/strict";
import { test } from "node:test";
import { gzipSync } from "node:zlib";

import {
  activeLabel,
  activitySignal,
  assignment,
  chosenFor,
  exportWindow,
  importPath,
  keyAction,
  keyCreation,
  keyEdit,
  readManifest,
  reworded,
  rollupNeedsWork,
  schemaOdd,
  screensFor,
  unassignPath,
  unsettled,
} from "../src/gateway/wire.ts";
import type { ArchiveEstimate, DaemonDevice, DatabaseEntry, DatabaseList } from "../src/gateway/wire.ts";

test("a read role is offered only the screens the gateway lets it read", () => {
  assert.deepEqual(
    screensFor("read").map((s) => s.id),
    ["databases", "health"],
  );
  assert.deepEqual(
    screensFor("admin").map((s) => s.id),
    ["databases", "keys", "ingest", "daemons", "activity", "log", "health"],
  );
});

test("a stale proof says where to renew it, as a sentence, and any other refusal is left as it was", () => {
  const stale = "this needs the password confirmed in the last 5 minutes";
  assert.equal(
    reworded(403, stale),
    "This needs the password confirmed in the last 5 minutes. Re-authenticate in the console, then try again.",
  );
  assert.equal(reworded(403, "admin role required"), "admin role required");
  assert.equal(reworded(400, stale), stale);
});

test("a key is sent trimmed, and an empty pin as no pin", () => {
  assert.deepEqual(keyCreation(" shed ", "ingest", "  "), { name: "shed", role: "ingest", database_pin: null });
  assert.deepEqual(keyCreation("desk", "read", " bench "), { name: "desk", role: "read", database_pin: "bench" });
  assert.deepEqual(keyEdit(" desk ", ""), { name: "desk", database_pin: null });
});

test("revoking deletes the key; the other actions post to it", () => {
  assert.deepEqual(keyAction(7, "revoke"), { path: "/v1/admin/keys/7", method: "DELETE" });
  assert.deepEqual(keyAction(7, "rotate"), { path: "/v1/admin/keys/7/rotate", method: "POST" });
  assert.deepEqual(keyAction(7, "enable"), { path: "/v1/admin/keys/7/enable", method: "POST" });
});

test("terminating deletes the backend; cancelling posts to its query", () => {
  assert.deepEqual(activitySignal("site", 42, true), { path: "/v1/db/site/activity/42", method: "DELETE" });
  assert.deepEqual(activitySignal("site", 42, false), { path: "/v1/db/site/activity/42/cancel", method: "POST" });
});

test("an import replaces only when asked", () => {
  assert.equal(importPath("site", false), "/v1/databases/site/import");
  assert.equal(importPath("site", true), "/v1/databases/site/import?replace=true");
});

const device = (extra: Partial<DaemonDevice> = {}): DaemonDevice => ({
  interface: "can0",
  bus: 0,
  database: null,
  last_seen_us: null,
  assignment: null,
  active: null,
  ...extra,
});

const assigned = { blob_sha: "abcdef0123456789", name: "car", assigned_at_us: 0, assigned_by: "ops" };

test("an assignment keeps a byte-order mark, expects what was assigned, and refuses bytes that are not UTF-8", () => {
  const bytes = new Uint8Array([0xef, 0xbb, 0xbf, 0x61]).buffer;
  assert.deepEqual(assignment("d1", device({ assignment: assigned }), bytes), {
    daemon_id: "d1",
    interface: "can0",
    content: "﻿a",
    provenance: {},
    expected: "abcdef0123456789",
  });
  assert.equal(assignment("d1", device(), new Uint8Array([0xff]).buffer), undefined);
  assert.equal(assignment("d1", device(), new Uint8Array([0x61]).buffer)?.expected, "");
  assert.equal(
    unassignPath("d one", { ...device(), assignment: assigned }),
    "/v1/admin/assignments?daemon_id=d+one&interface=can0&expected=abcdef0123456789",
  );
});

test("what a device frames with is measured against what is assigned", () => {
  const active = { source: "assigned" as const, blob_sha: assigned.blob_sha, name: null, since_us: 0, refused: null };
  assert.deepEqual(activeLabel(device({ assignment: assigned, active })), { text: "applied", tone: "good" });
  assert.deepEqual(
    activeLabel(
      device({
        assignment: assigned,
        active: { ...active, source: "none", refused: { blob_sha: assigned.blob_sha, reason: "bad id" } },
      }),
    ),
    { text: "refused: bad id", tone: "bad" },
  );
  assert.deepEqual(activeLabel(device({ assignment: assigned })), { text: "pending", tone: "neutral" });
  assert.deepEqual(activeLabel(device({ active: { ...active, source: "local" } })), {
    text: "local abcdef01",
    tone: "neutral",
  });
});

const HOUR_US = 3_600_000_000;
const bounds: ArchiveEstimate = {
  min_ts_us: 1_000 * HOUR_US,
  max_ts_us: 1_100 * HOUR_US,
  frames: 1,
  events: 0,
  approx_bytes: 1,
  empty: false,
};

test("an export window is clamped to the database's bounds, and ends past its newest row", () => {
  const chosen = chosenFor(bounds);
  assert.equal(exportWindow(bounds, chosen), null);
  assert.deepEqual(exportWindow(bounds, { ...chosen, scope: "24h" }), {
    from_us: 1_076 * HOUR_US + 1,
    to_us: 1_100 * HOUR_US + 1,
  });
  assert.deepEqual(exportWindow(bounds, { ...chosen, scope: "30d" }), {
    from_us: 1_000 * HOUR_US,
    to_us: 1_100 * HOUR_US + 1,
  });
  assert.equal(exportWindow(bounds, { ...chosen, scope: "custom", from: "" }), undefined);
  assert.equal(exportWindow({ ...bounds, min_ts_us: null, max_ts_us: null }, { ...chosen, scope: "24h" }), undefined);
});

const db = (extra: Partial<DatabaseEntry> = {}): DatabaseEntry => ({
  name: "site",
  size_bytes: 0,
  schema_state: "current",
  schema_version: 4,
  busy_secs: null,
  schema_error: null,
  rollup_state: "covered",
  rollup_lag_secs: 0,
  rollup_busy_secs: null,
  migration: null,
  buffered_rows: null,
  import_busy_secs: null,
  ...extra,
});

const list = (databases: DatabaseEntry[], auto_migrate = false): DatabaseList => ({
  databases,
  schema_version: 4,
  auto_migrate,
});

test("a settled list is not re-read; one migrating, importing, or waiting on somebody is", () => {
  assert.equal(unsettled(list([db()])), false);
  assert.equal(unsettled(list([db({ schema_state: "migrating" })])), true);
  assert.equal(unsettled(list([db({ import_busy_secs: 3 })])), true);
  assert.equal(unsettled(list([db({ schema_state: "pending" })])), true);
  assert.equal(unsettled(list([db({ schema_state: "failed" })])), false);
  assert.equal(unsettled(list([db({ schema_state: "failed" })], true)), true);
});

test("a rollup is worth rebuilding when incomplete or more than 4 h behind, and never mid-rebuild", () => {
  assert.equal(rollupNeedsWork(db({ rollup_lag_secs: 4 * 3600 })), false);
  assert.equal(rollupNeedsWork(db({ rollup_lag_secs: 4 * 3600 + 1 })), true);
  assert.equal(rollupNeedsWork(db({ rollup_state: "incomplete" })), true);
  assert.equal(rollupNeedsWork(db({ rollup_state: "incomplete", rollup_busy_secs: 1 })), false);
  assert.equal(rollupNeedsWork(db({ rollup_state: "empty" })), false);
});

test("health counts only the databases the gateway manages", () => {
  const { managed, odd } = schemaOdd(
    list([db(), db({ name: "old", schema_version: 3, schema_state: "pending" }), db({ name: "x", schema_state: "unknown" })]),
  );
  assert.equal(managed, 2);
  assert.deepEqual(
    odd.map((d) => d.name),
    ["old"],
  );
});

const archive = (manifest: object, version = 1) => {
  const json = new TextEncoder().encode(JSON.stringify(manifest));
  const section = new Uint8Array(4 + json.length);
  new DataView(section.buffer).setUint32(0, json.length);
  section.set(json, 4);
  return new Blob([new TextEncoder().encode("WTAR"), new Uint8Array([version]), gzipSync(section)]);
};

test("an archive's manifest is read before anything is sent, and anything else is refused", async () => {
  assert.deepEqual(await readManifest(archive({ database: "site" })), { database: "site" });
  await assert.rejects(readManifest(new Blob(["nope!"])), /not a WireTAP archive/);
  await assert.rejects(readManifest(archive({}, 2)), /format v2 is newer/);
});
