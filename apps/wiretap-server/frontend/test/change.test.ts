import assert from "node:assert/strict";
import { test } from "node:test";

import { changes, devicesOf, draftOf } from "../src/change.ts";
import type { Setting } from "../src/daemon.ts";

const SETTINGS: Setting[] = [
  {
    key: "devices",
    source: "default",
    pinned: false,
    value: [{ interface: "can0", kind: "can", mode: "active", fd: false, database: null, bus: 0 }],
  },
  { key: "forward.enable", source: "flag", pinned: true, where: "flag --forward-enable", value: true },
  { key: "forward.host", source: "ui", pinned: false, value: "gw" },
  { key: "forward.port", source: "file", pinned: true, where: "/etc/wiretap-server/wiretap-server.toml", value: 9323 },
  { key: "forward.database", source: "default", pinned: false, value: "" },
  { key: "forward.flush_interval", source: "default", pinned: false, value: 0.5 },
  { key: "forward.batch_size", source: "default", pinned: false, value: 500 },
  { key: "forward.api_key", source: "ui", pinned: false, set: true },
  { key: "ingest.token", source: "env", pinned: true, where: "env WIRETAP_INGEST_TOKEN", set: true },
  { key: "logging.level", source: "default", pinned: false, value: "INFO" },
];

const untouched = () => ({
  draft: draftOf(SETTINGS),
  devices: devicesOf(SETTINGS),
});

test("an untouched form sends nothing, not even the transmitting default's devices", () => {
  const { draft, devices } = untouched();
  assert.deepEqual(changes(SETTINGS, draft, [], devices), { body: {} });
});

test("only what changed is sent, typed, and an emptied field goes back to the default", () => {
  const { draft, devices } = untouched();
  Object.assign(draft, {
    "forward.database": " site ",
    "forward.flush_interval": "1.5",
    "forward.host": "",
    "logging.level": "DEBUG",
  });
  assert.deepEqual(changes(SETTINGS, draft, [], devices), {
    body: {
      "forward.database": "site",
      "forward.flush_interval": 1.5,
      "forward.host": null,
      "logging.level": "DEBUG",
    },
  });
});

test("a pinned key is never sent, whatever the form holds", () => {
  const { draft, devices } = untouched();
  Object.assign(draft, { "forward.enable": false, "forward.port": "1", "ingest.token": "t" });
  assert.deepEqual(changes(SETTINGS, draft, [], devices), { body: {} });
});

test("a secret is sent only when typed, and removed only when asked", () => {
  const { draft, devices } = untouched();
  assert.deepEqual(changes(SETTINGS, draft, ["forward.api_key"], devices), {
    body: { "forward.api_key": null },
  });
  draft["forward.api_key"] = "new-key";
  assert.deepEqual(changes(SETTINGS, draft, [], devices), { body: { "forward.api_key": "new-key" } });
});

test("a number that is not one is refused before anything is sent", () => {
  const { draft, devices } = untouched();
  draft["forward.batch_size"] = "2.5";
  assert.deepEqual(changes(SETTINGS, draft, [], devices), {
    refused: "Batch size, frames must be a whole number.",
  });
});

test("a device list is sent whole, with no mode, so every device on it listens only", () => {
  const { draft } = untouched();
  const body = changes(SETTINGS, draft, [], [
    { interface: "can0", fd: false, database: "" },
    { interface: " can1 ", fd: true, database: "bench" },
  ]);
  assert.deepEqual(body, {
    body: {
      devices: [
        { interface: "can0", fd: false, database: null },
        { interface: "can1", fd: true, database: "bench" },
      ],
    },
  });
  assert.deepEqual(changes(SETTINGS, draft, [], [{ interface: "", fd: false, database: "" }]), {
    refused: "Each device needs an interface name.",
  });
});
