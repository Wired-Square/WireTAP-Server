import { For, Show, createSignal, onMount } from "solid-js";

import { useRefresh } from "@wired-square/appliance-ui";
import { Badge, Notice, Section, createAction } from "@wired-square/kit";

import type { Listed } from "./Gateway";
import { gateway } from "./api";
import { schemaOdd } from "./wire";
import type { Health as Answer } from "./wire";

export function Health(props: { listed: Listed }) {
  const [health, setHealth] = createSignal<Answer>();
  const reading = createAction();
  const load = () => reading.run(async () => setHealth(await gateway<Answer>("/v1/health")));
  onMount(() => void load());
  useRefresh(load);

  return (
    <Section title="Health" blurb="Whether the gateway reaches its database, and which schema its capture databases are on.">
      <Notice tone="bad">{reading.failed() ?? props.listed.failed()}</Notice>
      <Show when={health()}>
        {(h) => (
          <dl class="app-details">
            <dt>Status</dt>
            <dd>
              <Badge tone={h().status === "ok" ? "good" : "bad"}>{h().status}</Badge>
            </dd>
            <dt>Gateway version</dt>
            <dd>{h().version}</dd>
            <dt>Database reachable</dt>
            <dd>
              <Badge tone={h().db_ok ? "good" : "bad"}>{h().db_ok ? "yes" : "no"}</Badge>
            </dd>
            <dt>Schema</dt>
            <dd>
              <Show when={props.listed.list()}>{(list) => <Schemas {...schemaOdd(list())} target={list().schema_version} />}</Show>
            </dd>
          </dl>
        )}
      </Show>
    </Section>
  );
}

/** One line when every database is on the current schema; the exceptions spelt out when not. */
function Schemas(props: ReturnType<typeof schemaOdd> & { target: number }) {
  return (
    <Show when={props.managed > 0} fallback={<span class="app-muted">no capture databases</span>}>
      <Show
        when={props.odd.length > 0}
        fallback={
          <>
            <Badge tone="good">v{props.target}</Badge>{" "}
            <span class="app-muted">all {props.managed === 1 ? "1 database" : `${props.managed} databases`}</span>
          </>
        }
      >
        <Badge tone={props.odd.some((d) => d.schema_state === "failed") ? "bad" : "warn"}>mixed</Badge>{" "}
        <span class="app-muted">
          {props.managed - props.odd.length} of {props.managed} at v{props.target}
        </span>
        <ul>
          <For each={props.odd}>
            {(d) => (
              <li>
                {d.name}: {d.schema_state}
                {d.schema_version === null ? "" : `, v${d.schema_version}`}
              </li>
            )}
          </For>
        </ul>
      </Show>
    </Show>
  );
}
