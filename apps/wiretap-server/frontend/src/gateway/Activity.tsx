import { For, Show, createEffect, createSignal, on } from "solid-js";

import { useRefresh } from "@wired-square/appliance-ui";
import { Button, Notice, NothingYet, Section, Select, Table, Td, Th, Tr, createAction, num } from "@wired-square/kit";

import type { Listed } from "./Gateway";
import { gateway } from "./api";
import { activitySignal } from "./wire";
import type { Activity as Row } from "./wire";

export function Activity(props: { listed: Listed }) {
  const names = () => props.listed.list()?.databases.map((d) => d.name) ?? [];
  const [chosen, choose] = createSignal<string>();
  const db = () => chosen() ?? names()[0];
  const [now, setNow] = createSignal<{ queries: Row[]; sessions: Row[] }>();
  const reading = createAction({ supersede: true });
  const acting = createAction();
  const load = () => {
    const name = db();
    if (name) void reading.run(async () => setNow(await gateway(`/v1/db/${name}/activity`)));
  };
  createEffect(on(db, load));
  useRefresh(load, 3_000);

  const signal = (pid: number, terminate: boolean) =>
    acting.run(async () => {
      const { path, method } = activitySignal(db()!, pid, terminate);
      await gateway(path, { method });
      load();
    });

  const table = (rows: Row[], running: boolean) => (
    <Show when={rows.length > 0} fallback={<NothingYet label={running ? "No running queries" : "No idle sessions"} />}>
      <Table
        head={
          <>
            <Th right>PID</Th>
            <Th>Client</Th>
            <Th>Application</Th>
            <Th>{running ? "Query" : "State"}</Th>
            <Th right>Duration</Th>
            <Th unseen>Actions</Th>
          </>
        }
      >
        <For each={rows}>
          {(r) => (
            <Tr>
              <Td right numeric strong>
                {r.pid}
              </Td>
              <Td>{r.client_addr ?? "local"}</Td>
              <Td>{r.application_name || "—"}</Td>
              <Td>{(running ? r.query : r.state) ?? ""}</Td>
              <Td right numeric>
                {num(r.duration_secs, " s", 1)}
              </Td>
              <Td controls>
                <Show when={running}>
                  <Button tone="plain" onClick={() => void signal(r.pid, false)}>
                    Cancel
                  </Button>
                </Show>
                <Button tone="danger" onClick={() => void signal(r.pid, true)}>
                  Terminate
                </Button>
              </Td>
            </Tr>
          )}
        </For>
      </Table>
    </Show>
  );

  return (
    <Section title="Database activity" blurb="Queries and sessions on one capture database, read every 3 seconds.">
      <Select label="Database" value={db() ?? ""} onValue={choose}>
        <For each={names()}>{(name) => <option value={name}>{name}</option>}</For>
      </Select>
      <Notice tone="bad">{reading.failed() ?? acting.failed()}</Notice>
      <Show when={now()}>
        {(a) => (
          <>
            <Section title="Running queries" level={3}>
              {table(a().queries, true)}
            </Section>
            <Section title="Idle sessions" level={3}>
              {table(a().sessions, false)}
            </Section>
          </>
        )}
      </Show>
    </Section>
  );
}
