import { For, Show, createSignal, onMount } from "solid-js";

import { useRefresh } from "@wired-square/appliance-ui";
import { Notice, NothingYet, Section, Table, Td, Th, Tr, createAction, time } from "@wired-square/kit";

import { gateway } from "./api";
import type { IngestSession } from "./wire";

export function Ingest() {
  const [sessions, setSessions] = createSignal<IngestSession[]>([]);
  const reading = createAction();
  const load = () =>
    reading.run(async () =>
      setSessions((await gateway<{ sessions: IngestSession[] }>("/v1/admin/ingest-sessions")).sessions),
    );
  onMount(() => void load());
  useRefresh(load, 2_000);

  return (
    <Section title="Connected ingest devices" blurb="Devices and daemons pushing frames to this gateway now, this box's own capture daemon among them.">
      <Notice tone="bad">{reading.failed()}</Notice>
      <Show when={sessions().length > 0} fallback={<NothingYet label="No ingest devices connected" />}>
        <Table
          head={
            <>
              <Th>Peer</Th>
              <Th>Key</Th>
              <Th>Database</Th>
              <Th>Protocol</Th>
              <Th right>Frames</Th>
              <Th right>Batches</Th>
              <Th>Connected</Th>
            </>
          }
        >
          <For each={sessions()}>
            {(s) => (
              <Tr>
                <Td strong>{s.peer}</Td>
                <Td>{s.key_name}</Td>
                <Td>{s.database}</Td>
                <Td>v{s.protocol_version}</Td>
                <Td right numeric>
                  {s.frames.toLocaleString()}
                </Td>
                <Td right numeric>
                  {s.batches.toLocaleString()}
                </Td>
                <Td muted>{time(s.connected_at)}</Td>
              </Tr>
            )}
          </For>
        </Table>
      </Show>
    </Section>
  );
}
