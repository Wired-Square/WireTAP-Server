import { For, Show, createEffect, createSignal, on } from "solid-js";

import { useRefresh } from "@wired-square/appliance-ui";
import type { LogSnapshot } from "@wired-square/appliance-types";
import { Notice, Section, Select, createAction, time } from "@wired-square/kit";

import { gateway } from "./api";

const LEVELS = ["error", "warn", "info", "debug"] as const;

/**
 * The gateway's in-memory log, polled: it serves a snapshot and no feed, so
 * appliance-ui's `Logs`, which follows `/api/logs/events`, cannot show it.
 */
export function Log() {
  const [level, setLevel] = createSignal("");
  const [snapshot, setSnapshot] = createSignal<LogSnapshot>();
  const reading = createAction({ supersede: true });
  const load = () =>
    reading.run(async () => setSnapshot(await gateway<LogSnapshot>(`/v1/admin/logs${level() ? `?level=${level()}` : ""}`)));
  createEffect(on(level, () => void load()));
  useRefresh(load, 3_000);
  const newestFirst = () => snapshot()?.records.slice().reverse() ?? [];

  return (
    <Section
      title="Gateway log"
      blurb="The newest 200 records, from the last 1,000 the gateway holds in memory; a restart empties it. Routine traffic is logged at DEBUG, which RUST_LOG=wiretap_backend=debug shows."
    >
      <Select label="Show" value={level()} onValue={setLevel}>
        <option value="">everything</option>
        <For each={LEVELS}>{(l) => <option value={l}>{l} and above</option>}</For>
      </Select>
      <Notice tone="bad">{reading.failed()}</Notice>
      <div class="app-log">
        <Show when={newestFirst().length > 0} fallback={<p class="app-log__empty">Nothing logged at this level yet.</p>}>
          <For each={newestFirst()}>
            {(r) => (
              <div class="app-log__line" title={`${r.target} · ${r.at}`}>
                <span class="app-log__when numeric">{time(r.at)}</span>
                <span class={`app-log__level app-log__level--${r.level}`}>{r.level.toUpperCase()}</span>
                <span class="app-log__text">
                  {r.message}
                  <For each={r.fields}>{([k, v]) => <span class="app-log__field"> {`${k}=${v}`}</span>}</For>
                </span>
              </div>
            )}
          </For>
        </Show>
      </div>
    </Section>
  );
}
