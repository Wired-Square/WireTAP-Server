/** The Capture overview's one-line state of the devices and of forwarding, each linking to its view. */
import type { Tone } from "@wired-square/kit";

import type { Device, Status } from "./daemon";

export interface Summary {
  tone: Tone;
  badge: string;
  detail: string;
}

const counted = (n: number, one: string, many: string) => `${n.toLocaleString()} ${n === 1 ? one : many}`;

export function devicesSummary(devices: Device[]): Summary {
  if (devices.length === 0) return { tone: "neutral", badge: "none", detail: "frames arrive only by device push" };
  const up = devices.filter((d) => d.state === "up").length;
  const tone = devices.some((d) => d.state === "error") ? "bad" : up === devices.length ? "good" : "warn";
  const frames = devices.reduce((sum, d) => sum + d.frames, 0);
  return { tone, badge: `${up} of ${devices.length} up`, detail: counted(frames, "frame", "frames") };
}

export function forwardingSummary(forward: Status["forward"]): Summary {
  if (!forward.enabled) return { tone: "neutral", badge: "off", detail: "frames are not forwarded" };
  const connected = forward.links.filter((l) => l.connected).length;
  const total = forward.links.length;
  return {
    tone: total > 0 && connected === total ? "good" : "warn",
    badge: `${connected} of ${counted(total, "link", "links")} connected`,
    detail: `to ${forward.gateway ?? "the gateway"}`,
  };
}
