import assert from "node:assert/strict";
import { test } from "node:test";

import type { Device, Link } from "../src/daemon.ts";
import { devicesSummary, forwardingSummary } from "../src/summary.ts";

const device = (state: Device["state"], frames: number): Device => ({
  interface: "can0",
  kind: "can",
  bus: 0,
  mode: "passive",
  state,
  frames,
});

const link = (connected: boolean): Link => ({
  database: "capture",
  connected,
  last_ack_unix_ms: null,
  cache_pending: 0,
  enqueued: 0,
  written: 0,
  dropped: 0,
});

test("devices read good only when every one is up, bad when any has failed, and count their frames", () => {
  assert.deepEqual(devicesSummary([device("up", 1), device("up", 0)]), { tone: "good", badge: "2 of 2 up", detail: "1 frame" });
  assert.equal(devicesSummary([device("up", 5), device("waiting", 0)]).tone, "warn");
  assert.equal(devicesSummary([device("starting", 0), device("error", 0)]).tone, "bad");
  assert.equal(devicesSummary([device("up", 2), device("up", 3)]).detail, "5 frames");
});

test("no local devices is neutral, not a fault: frames may still be pushed", () => {
  assert.deepEqual(devicesSummary([]), { tone: "neutral", badge: "none", detail: "frames arrive only by device push" });
});

test("forwarding reads good only when every link is connected, and off is neutral", () => {
  const on = (links: Link[]) => forwardingSummary({ enabled: true, gateway: "127.0.0.1:9323", links });
  assert.deepEqual(on([link(true)]), { tone: "good", badge: "1 of 1 link connected", detail: "to 127.0.0.1:9323" });
  assert.equal(on([link(true), link(false)]).badge, "1 of 2 links connected");
  assert.equal(on([link(true), link(false)]).tone, "warn");
  assert.equal(on([]).tone, "warn");
  assert.deepEqual(forwardingSummary({ enabled: false, gateway: null, links: [] }), {
    tone: "neutral",
    badge: "off",
    detail: "frames are not forwarded",
  });
});
