import assert from "node:assert/strict";
import { test } from "node:test";

import { followHash, pushRoute, replaceRoute, routeOf } from "../src/route.ts";
import type { Route } from "../src/route.ts";

/** A page whose `entries` are its history, the current one last. */
function page(hash: string) {
  const entries = [hash];
  const location = { hash };
  const at = Object.assign(new EventTarget(), {
    location,
    history: {
      pushState: (_: unknown, __: string, url: string) => {
        entries.push(url);
        location.hash = url;
      },
      replaceState: (_: unknown, __: string, url: string) => {
        entries[entries.length - 1] = url;
        location.hash = url;
      },
    },
  });
  return { at, entries };
}

test("the hash names the tab and, after a slash, the view", () => {
  assert.deepEqual(routeOf("#capture/devices"), { tab: "capture", view: "devices" });
  assert.deepEqual(routeOf("#about"), { tab: "about", view: undefined });
  assert.deepEqual(routeOf(""), { tab: "", view: undefined });
});

test("a hash change from any source moves the route, until the page stops following", () => {
  const { at } = page("#capture");
  const seen: Route[] = [];
  const stop = followHash(at, (r) => seen.push(r));
  at.location.hash = "#gateway/keys";
  at.dispatchEvent(new Event("hashchange"));
  assert.deepEqual(seen, [{ tab: "gateway", view: "keys" }]);
  stop();
  at.location.hash = "#about";
  at.dispatchEvent(new Event("hashchange"));
  assert.equal(seen.length, 1);
});

test("choosing a tab or view pushes a history entry, but not one for where the page already is", () => {
  const { at, entries } = page("#capture");
  pushRoute(at, { tab: "capture", view: undefined });
  pushRoute(at, { tab: "capture", view: "devices" });
  pushRoute(at, { tab: "capture", view: "devices" });
  pushRoute(at, { tab: "about", view: undefined });
  assert.deepEqual(entries, ["#capture", "#capture/devices", "#about"]);
});

test("a hash naming other than what is shown is rewritten in place, adding no entry", () => {
  const { at, entries } = page("#zzz");
  assert.equal(replaceRoute(at, { tab: "capture", view: "overview" }), true);
  assert.equal(replaceRoute(at, { tab: "capture", view: "overview" }), false);
  assert.deepEqual(entries, ["#capture/overview"]);
});
