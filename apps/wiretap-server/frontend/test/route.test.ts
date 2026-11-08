import assert from "node:assert/strict";
import { test } from "node:test";

import { followHash, pushRoute, routeOf } from "../src/route.ts";
import type { Route } from "../src/route.ts";

function page(hash: string) {
  const pushed: string[] = [];
  const location = { hash };
  const at = Object.assign(new EventTarget(), {
    location,
    history: {
      pushState: (_: unknown, __: string, url: string) => {
        pushed.push(url);
        location.hash = url;
      },
    },
  });
  return { at, pushed };
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
  const { at, pushed } = page("#capture");
  pushRoute(at, { tab: "capture", view: undefined });
  pushRoute(at, { tab: "capture", view: "devices" });
  pushRoute(at, { tab: "capture", view: "devices" });
  pushRoute(at, { tab: "about", view: undefined });
  assert.deepEqual(pushed, ["#capture/devices", "#about"]);
});
