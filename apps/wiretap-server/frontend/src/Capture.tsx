/**
 * The capture daemon: what it is doing, and the settings a UI may change.
 * Everything here is read from, and written to, the daemon's control socket.
 */
import { For, Index, Show, createEffect, createMemo, createSignal, on, onMount } from "solid-js";

import {
  Badge,
  Button,
  Capability,
  Field,
  Form,
  Notice,
  NothingYet,
  Refusal,
  Section,
  Select,
  Table,
  Td,
  Th,
  Tick,
  Tr,
  Warnings,
  ago,
  allows,
  api,
  createAction,
  duration,
  useRefresh,
} from "@wired-square/appliance-ui";
import type { Me, Tone } from "@wired-square/appliance-ui";

import { GROUPS, LEVELS, changes, devicesOf, draftOf, setting } from "./change";
import type { DeviceDraft, Draft, Kind, Field as SettingField } from "./change";
import type { Device, ListedDevice, Reading, Setting, Settings, Status } from "./daemon";

const STATUS = "/api/capture/status";
const SETTINGS = "/api/capture/settings";

export function Capture(props: { me: Me }) {
  return (
    <>
      <DaemonStatus />
      <DaemonSettings editable={allows(props.me.role, "admin")} />
    </>
  );
}

function DaemonStatus() {
  const [status, setStatus] = createSignal<Reading<Status>>();
  const [now, setNow] = createSignal(Date.now());
  const reading = createAction();
  const load = () =>
    reading.run(async () => {
      setStatus(await api<Reading<Status>>(STATUS));
      setNow(Date.now());
    });
  onMount(() => void load());
  useRefresh(load, 5_000);

  return (
    <Section title="Capture" blurb="What the capture daemon is doing now.">
      <Show when={reading.failed()}>{(why) => <Notice tone="bad">{why()}</Notice>}</Show>
      <Show when={status()}>
        {(read) => (
          <Capability of={read()} what="The capture daemon is not answering">
            {(running) => <Running status={running()} now={now()} />}
          </Capability>
        )}
      </Show>
    </Section>
  );
}

const DEVICE_TONE: Record<Device["state"], Tone> = {
  starting: "neutral",
  up: "good",
  waiting: "warn",
  error: "bad",
};

function Running(props: { status: Status; now: number }) {
  const s = () => props.status;
  return (
    <>
      <p>
        wiretap-server {s().version}, up {duration(s().uptime_secs)}
        <Show when={s().daemon_id}>{(id) => <>, as {id()}</>}</Show>.
      </p>
      <Show when={s().applying}>
        <Notice tone="neutral">Applying a change: the daemon is restarting.</Notice>
      </Show>
      <Warnings of={s().warnings} />

      <Section title="Devices" level={3}>
        <Show
          when={s().devices.length > 0}
          fallback={
            <NothingYet
              label="No local devices"
              note="This daemon captures nothing itself; frames arrive only by device push."
            />
          }
        >
          <Table
            head={
              <>
                <Th>Interface</Th>
                <Th>Kind</Th>
                <Th>Mode</Th>
                <Th>State</Th>
                <Th right>Frames</Th>
              </>
            }
          >
            <For each={s().devices}>
              {(d) => (
                <Tr>
                  <Td strong>{d.interface}</Td>
                  <Td>{d.kind === "can" ? "CAN" : "Serial"}</Td>
                  <Td>{d.mode}</Td>
                  <Td>
                    <Badge tone={DEVICE_TONE[d.state]}>{d.state}</Badge>
                  </Td>
                  <Td right numeric>
                    {d.frames.toLocaleString()}
                  </Td>
                </Tr>
              )}
            </For>
          </Table>
        </Show>
      </Section>

      <Section
        title="Gateway"
        level={3}
        blurb={
          s().forward.enabled ? `Forwarding to ${s().forward.gateway}.` : "Forwarding is off."
        }
      >
        <Show when={s().forward.links.length > 0}>
          <Table
            head={
              <>
                <Th>Database</Th>
                <Th>Link</Th>
                <Th>Last acknowledged</Th>
                <Th right>Cached on disk</Th>
                <Th right>Written</Th>
                <Th right>Dropped</Th>
              </>
            }
          >
            <For each={s().forward.links}>
              {(link) => (
                <Tr>
                  <Td strong>{link.database}</Td>
                  <Td>
                    <Badge tone={link.connected ? "good" : "warn"}>
                      {link.connected ? "connected" : "not connected"}
                    </Badge>
                  </Td>
                  <Td muted>
                    {link.last_ack_unix_ms === null ? "never" : ago(link.last_ack_unix_ms, props.now)}
                  </Td>
                  <Td right numeric>
                    {link.cache_pending.toLocaleString()}
                  </Td>
                  <Td right numeric>
                    {link.written.toLocaleString()}
                  </Td>
                  <Td right numeric>
                    {link.dropped.toLocaleString()}
                  </Td>
                </Tr>
              )}
            </For>
          </Table>
        </Show>
      </Section>

      <Section title="Clients" level={3}>
        <Table
          head={
            <>
              <Th>Listener</Th>
              <Th>Address</Th>
              <Th right>Clients</Th>
            </>
          }
        >
          <Listener name="Device push" on={s().ingest.enabled} at={s().ingest.listen} clients={s().ingest.clients} />
          <Listener name="GVRET" on={s().gvret.enabled} at={s().gvret.listen} clients={s().gvret.clients} />
        </Table>
      </Section>
    </>
  );
}

function Listener(props: { name: string; on: boolean; at: string | null; clients: number | null }) {
  return (
    <Tr dim={!props.on}>
      <Td strong>{props.name}</Td>
      <Td>{props.on ? props.at : "off"}</Td>
      <Td right numeric>
        {props.on ? (props.clients ?? 0) : "—"}
      </Td>
    </Tr>
  );
}

function DaemonSettings(props: { editable: boolean }) {
  const [view, setView] = createSignal<Reading<Settings>>();
  const reading = createAction();
  const load = () => reading.run(async () => setView(await api<Reading<Settings>>(SETTINGS)));
  onMount(() => void load());

  return (
    <Section
      title="Settings"
      blurb="Saving restarts the capture daemon on the change. A setting the config file, a flag or the environment sets is fixed there, and says where."
      actions={
        <Button tone="plain" onClick={() => void load()}>
          Read again
        </Button>
      }
    >
      <Show when={reading.failed()}>{(why) => <Notice tone="bad">{why()}</Notice>}</Show>
      <Show when={view()}>
        {(read) => (
          <Capability of={read()} what="The capture daemon is not answering">
            {(ready) => (
              <SettingsForm
                settings={ready().settings}
                editable={props.editable}
                onSaved={setView}
              />
            )}
          </Capability>
        )}
      </Show>
    </Section>
  );
}

interface FormState {
  settings: Setting[];
  editable: boolean;
  draft: Draft;
  setDraft: (key: string, value: string | boolean) => void;
  removing: string[];
  setRemoving: (key: string, on: boolean) => void;
  devices: DeviceDraft[];
  setDevices: (devices: DeviceDraft[]) => void;
}

function SettingsForm(props: {
  settings: Setting[];
  editable: boolean;
  onSaved: (next: Reading<Settings>) => void;
}) {
  const [draft, setDraft] = createSignal<Draft>({});
  const [removing, setRemoving] = createSignal<string[]>([]);
  const [devices, setDevices] = createSignal<DeviceDraft[]>([]);
  createEffect(
    on(
      () => props.settings,
      (settings) => {
        setDraft(draftOf(settings));
        setRemoving([]);
        setDevices(devicesOf(settings));
      },
    ),
  );
  const change = createMemo(() => changes(props.settings, draft(), removing(), devices()));
  const unchanged = () => {
    const c = change();
    return "body" in c && Object.keys(c.body).length === 0;
  };

  const state: FormState = {
    get settings() {
      return props.settings;
    },
    get editable() {
      return props.editable;
    },
    get draft() {
      return draft();
    },
    setDraft: (key, value) => setDraft((d) => ({ ...d, [key]: value })),
    get removing() {
      return removing();
    },
    setRemoving: (key, on) =>
      setRemoving((r) => (on ? [...r, key] : r.filter((k) => k !== key))),
    get devices() {
      return devices();
    },
    setDevices,
  };

  const save = async () => {
    const c = change();
    if ("refused" in c) throw new Refusal(c.refused);
    props.onSaved(await api<Reading<Settings>>(SETTINGS, { method: "PUT", body: c.body }));
  };

  return (
    <Show when={props.editable} fallback={<Fields state={state} />}>
      <Form
        act={save}
        verb="Save and restart"
        done="Saved. The capture daemon restarted on the change."
        disabled={unchanged()}
      >
        <Fields state={state} />
      </Form>
    </Show>
  );
}

function Fields(props: { state: FormState }) {
  return (
    <>
      <Devices state={props.state} />
      <For each={GROUPS}>
        {(group) => (
          <Section title={group.title} blurb={group.blurb} level={3}>
            <For each={group.fields}>
              {(field) => (
                <Show when={setting(props.state.settings, field.key)}>
                  {(now) => <Input field={field} now={now()} state={props.state} />}
                </Show>
              )}
            </For>
          </Section>
        )}
      </For>
    </>
  );
}

/** Where a pinned setting is fixed, or what a free one is set by. */
function provenance(now: Setting): string {
  if (now.pinned) return `Fixed ${now.source === "file" ? "in" : "by"} ${now.where}.`;
  return now.source === "ui" ? "Set here." : "The default.";
}

const BACK_TO_DEFAULT: Partial<Record<Kind, string>> = {
  level: "Choose the default to go back to it.",
  text: "Empty it to go back to the default.",
  whole: "Empty it to go back to the default.",
  number: "Empty it to go back to the default.",
};

function Input(props: { field: SettingField; now: Setting; state: FormState }) {
  const locked = () => props.now.pinned || !props.state.editable;
  const typed = () => props.state.draft[props.field.key];
  const hint = () =>
    [props.field.hint, provenance(props.now), props.now.source === "ui" && BACK_TO_DEFAULT[props.field.kind]]
      .filter(Boolean)
      .join(" ");
  const set = (value: string | boolean) => props.state.setDraft(props.field.key, value);

  switch (props.field.kind) {
    case "flag":
      return (
        <Tick
          label={props.field.label}
          hint={props.now.pinned ? provenance(props.now) : undefined}
          checked={typed() === true}
          disabled={locked()}
          onChange={set}
        />
      );
    case "level":
      return (
        <Select
          label={props.field.label}
          hint={hint()}
          value={String(typed() ?? "")}
          disabled={locked()}
          onChange={(e) => set(e.currentTarget.value)}
        >
          <option value="">the default</option>
          <For each={LEVELS}>{(level) => <option value={level}>{level}</option>}</For>
        </Select>
      );
    case "secret":
      return (
        <>
          <Field
            label={props.field.label}
            type="password"
            autocomplete="new-password"
            placeholder={props.now.set ? "set; type a new one to replace it" : "not set"}
            hint={props.now.pinned ? provenance(props.now) : "Never shown. Leave empty to keep it."}
            value={String(typed() ?? "")}
            disabled={locked()}
            onInput={(e) => set(e.currentTarget.value)}
          />
          <Show when={!locked() && props.now.source === "ui" && props.now.set}>
            <Tick
              label={`Remove the ${props.field.label.toLowerCase()} set here`}
              checked={props.state.removing.includes(props.field.key)}
              onChange={(on) => props.state.setRemoving(props.field.key, on)}
            />
          </Show>
        </>
      );
    default:
      return (
        <Field
          label={props.field.label}
          hint={hint()}
          inputmode={props.field.kind === "text" ? undefined : "decimal"}
          value={String(typed() ?? "")}
          disabled={locked()}
          onInput={(e) => set(e.currentTarget.value)}
        />
      );
  }
}

function Devices(props: { state: FormState }) {
  const listed = () => setting(props.state.settings, "devices");
  const edit = (i: number, change: Partial<DeviceDraft>) =>
    props.state.setDevices(props.state.devices.map((d, j) => (j === i ? { ...d, ...change } : d)));

  return (
    <Section
      title="Devices"
      level={3}
      blurb="The CAN interfaces this daemon captures. A device listed here only ever listens: nothing here can make one transmit, and saving a list makes every device on it passive."
    >
      <Show
        when={!listed()?.pinned && props.state.editable}
        fallback={<FixedDevices now={listed()} />}
      >
        <Index each={props.state.devices}>
          {(device, i) => (
            <div class="capture-device">
              <Field
                label="Interface"
                value={device().interface}
                placeholder="can0"
                onInput={(e) => edit(i, { interface: e.currentTarget.value })}
              />
              <Field
                label="Database"
                hint="Empty is the gateway's database."
                value={device().database}
                onInput={(e) => edit(i, { database: e.currentTarget.value })}
              />
              <Tick label="CAN FD" checked={device().fd} onChange={(fd) => edit(i, { fd })} />
              <Button
                tone="plain"
                onClick={() => props.state.setDevices(props.state.devices.filter((_, j) => j !== i))}
              >
                Remove
              </Button>
            </div>
          )}
        </Index>
        <Show when={props.state.devices.length === 0}>
          <p>No devices: this daemon will capture nothing locally.</p>
        </Show>
        <div class="app-actions">
          <Button
            onClick={() =>
              props.state.setDevices([...props.state.devices, { interface: "", fd: false, database: "" }])
            }
          >
            Add a CAN interface
          </Button>
        </div>
      </Show>
    </Section>
  );
}

function FixedDevices(props: { now: Setting | undefined }) {
  const devices = () => (props.now?.value ?? []) as ListedDevice[];
  return (
    <>
      <Show when={props.now?.pinned}>
        <p>{provenance(props.now!)}</p>
      </Show>
      <Show when={devices().length > 0} fallback={<p>No devices: this daemon captures nothing locally.</p>}>
        <Table
          head={
            <>
              <Th>Interface</Th>
              <Th>Kind</Th>
              <Th>Mode</Th>
              <Th>CAN FD</Th>
              <Th>Database</Th>
            </>
          }
        >
          <For each={devices()}>
            {(d) => (
              <Tr>
                <Td strong>{d.interface}</Td>
                <Td>{d.kind === "can" ? "CAN" : "Serial"}</Td>
                <Td>{d.mode}</Td>
                <Td>{d.fd ? "yes" : "no"}</Td>
                <Td muted>{d.database ?? "the gateway's"}</Td>
              </Tr>
            )}
          </For>
        </Table>
      </Show>
    </>
  );
}
