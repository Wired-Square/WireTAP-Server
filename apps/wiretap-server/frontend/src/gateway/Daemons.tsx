import { For, Show, createSignal, onMount } from "solid-js";

import { ApiError, useRefresh } from "@wired-square/appliance-ui";
import {
  Badge,
  Confirm,
  Dialog,
  Menu,
  MenuItem,
  Notice,
  NothingYet,
  PickFile,
  Refusal,
  Section,
  Table,
  Td,
  Th,
  Tr,
  createAction,
} from "@wired-square/kit";
import type { ConfirmProps } from "@wired-square/kit";

import { gateway } from "./api";
import { activeLabel, assignment, short, unassignPath } from "./wire";
import type { Assignment, Daemon, DaemonDevice, StoredCatalog } from "./wire";

const stamp = (us: number) => new Date(us / 1000).toLocaleString();

/** A 409 says the assignment moved under this page, which a re-read shows. */
async function assigning(ask: () => Promise<unknown>): Promise<void> {
  try {
    await ask();
  } catch (e) {
    if (e instanceof ApiError && e.status === 409) {
      throw new Refusal("The assignment changed meanwhile. Look again, then try again.");
    }
    throw e;
  }
}

export function Daemons() {
  const [daemons, setDaemons] = createSignal<Daemon[]>([]);
  const reading = createAction();
  const acting = createAction();
  const load = () =>
    reading.run(async () => setDaemons((await gateway<{ daemons: Daemon[] }>("/v1/admin/daemons")).daemons));
  onMount(() => void load());
  useRefresh(load, 5_000);

  const [asked, ask] = createSignal<Omit<ConfirmProps, "onClose">>();
  const [viewing, setViewing] = createSignal<StoredCatalog>();

  const assign = (daemon: string, device: DaemonDevice, file: File) =>
    acting.run(async () => {
      const body = assignment(daemon, device, await file.arrayBuffer());
      if (!body) throw new Refusal(`${file.name} is not UTF-8 text.`);
      await assigning(() => gateway("/v1/admin/assignments", { method: "PUT", body }));
      acting.succeed(`Assigned ${file.name} to ${device.interface} on ${daemon}.`);
      await load();
    });

  const clear = (daemon: string, device: DaemonDevice & { assignment: Assignment }) =>
    ask({
      title: "Clear catalogue",
      verb: "Clear",
      children: <p>Clear the catalogue assigned to {device.interface} on {daemon}?</p>,
      act: async () => {
        await assigning(() => gateway(unassignPath(daemon, device), { method: "DELETE" }));
        await load();
      },
    });

  const view = (sha: string) =>
    acting.run(async () => setViewing(await gateway<StoredCatalog>(`/v1/admin/catalogs/${sha}`)));

  return (
    <Section
      title="Capture daemons"
      blurb="Daemons that have named themselves to this gateway, and the catalogue each device frames with."
    >
      <Notice tone="bad">{reading.failed() ?? acting.failed()}</Notice>
      <Notice tone="good">{acting.succeeded()}</Notice>
      <Show when={daemons().length > 0} fallback={<NothingYet label="No capture daemon has named itself yet" />}>
        <For each={daemons()}>
          {(d) => (
            <Section title={d.daemon_id} level={3}>
              <Table
                head={
                  <>
                    <Th>Interface</Th>
                    <Th right>Bus</Th>
                    <Th>Database</Th>
                    <Th>Last seen</Th>
                    <Th>Assigned</Th>
                    <Th>Active</Th>
                    <Th unseen>Actions</Th>
                  </>
                }
              >
                <For each={d.devices}>
                  {(dev) => (
                    <Tr>
                      <Td strong>{dev.interface}</Td>
                      <Td right numeric>
                        {dev.bus ?? "—"}
                      </Td>
                      <Td>{dev.database ?? "—"}</Td>
                      <Td muted>{dev.last_seen_us === null ? "never" : stamp(dev.last_seen_us)}</Td>
                      <Td>
                        <Show when={dev.assignment} fallback={<span class="app-muted">—</span>}>
                          {(a) => (
                            <span title={`by ${a().assigned_by ?? "unknown"}, ${stamp(a().assigned_at_us)}`}>
                              {a().name ?? "unnamed"} <span class="app-muted">{short(a().blob_sha)}</span>
                            </span>
                          )}
                        </Show>
                      </Td>
                      <Td>
                        <Badge tone={activeLabel(dev).tone}>{activeLabel(dev).text}</Badge>
                      </Td>
                      <Td controls>
                        <PickFile
                          label="Assign…"
                          accept=".toml"
                          tone="plain"
                          onFile={(file) => void assign(d.daemon_id, dev, file)}
                          onProblem={(why) => acting.run(() => Promise.reject(new Refusal(why)))}
                        />
                        <Show when={dev.assignment}>
                          {(a) => (
                            <Menu aria-label={`Catalogue on ${dev.interface}`}>
                              <MenuItem onSelect={() => void view(a().blob_sha)}>View</MenuItem>
                              <MenuItem danger onSelect={() => clear(d.daemon_id, { ...dev, assignment: a() })}>
                                Clear…
                              </MenuItem>
                            </Menu>
                          )}
                        </Show>
                      </Td>
                    </Tr>
                  )}
                </For>
              </Table>
            </Section>
          )}
        </For>
      </Show>

      <Show when={asked()}>{(q) => <Confirm {...q()} onClose={() => ask(undefined)} />}</Show>
      <Show when={viewing()}>
        {(c) => (
          <Dialog title={`Catalogue ${short(c().blob_sha)}`} wide closeButton onClose={() => setViewing(undefined)}>
            <pre>{c().content}</pre>
          </Dialog>
        )}
      </Show>
    </Section>
  );
}
