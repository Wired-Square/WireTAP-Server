import { For, Show, createSignal, onMount } from "solid-js";

import {
  Badge,
  Button,
  Confirm,
  Dialog,
  Field,
  Form,
  Menu,
  MenuItem,
  MenuItemUnavailable,
  Notice,
  NothingYet,
  Section,
  Select,
  ShownOnce,
  Table,
  Td,
  Th,
  Tr,
  createAction,
  keyedRows,
  when,
} from "@wired-square/kit";
import type { ConfirmProps } from "@wired-square/kit";

import { gateway } from "./api";
import { keyAction, keyCreation, keyEdit } from "./wire";
import type { GatewayRole, KeySummary, KeyVerb, RotatedSecret, SecretSources } from "./wire";

type Question = Omit<ConfirmProps, "onClose">;

interface Shown {
  label: string;
  value: string;
  note: string;
}

const VERBS: Record<KeyVerb, string> = { rotate: "Rotate", disable: "Disable", enable: "Enable", revoke: "Revoke" };

const PROMPTS: Record<KeyVerb, (name: string) => string> = {
  rotate: (name) => `Rotate key “${name}”? Its current value stops working at once; clients need the new one.`,
  disable: (name) => `Disable key “${name}”? Clients using it lose access until it is enabled again.`,
  enable: (name) => `Enable key “${name}”? It will work again.`,
  revoke: (name) => `Revoke key “${name}”? Clients using it lose access, and this cannot be undone.`,
};

const ROLES: [GatewayRole, string][] = [
  ["read", "read: desktops and queries"],
  ["ingest", "ingest: capture devices"],
  ["admin", "admin: full control"],
];

export function Keys() {
  const [keys, setKeys] = createSignal<KeySummary[]>();
  const rows = keyedRows(keys, "id");
  const reading = createAction();
  const load = () => reading.run(async () => setKeys((await gateway<{ keys: KeySummary[] }>("/v1/admin/keys")).keys));
  onMount(() => void load());

  const [asked, ask] = createSignal<Question>();
  const [shown, show] = createSignal<Shown>();
  const [editing, setEditing] = createSignal<KeySummary>();

  const act = (k: KeySummary, verb: KeyVerb) =>
    ask({
      title: `${VERBS[verb]} key`,
      verb: VERBS[verb],
      tone: verb === "enable" ? "primary" : "danger",
      children: <p>{PROMPTS[verb](k.name)}</p>,
      act: async () => {
        const { path, method } = keyAction(k.id, verb);
        const answer = await gateway<{ key?: string } | null>(path, { method });
        if (answer?.key) show({ label: `Key “${k.name}”`, value: answer.key, note: "Copy it now: it is shown once and never stored." });
        await load();
      },
    });

  return (
    <>
      <Section title="API keys" blurb="Keys for desktops, capture devices and scripts. A key is shown once, when it is made or rotated.">
        <Notice tone="bad">{reading.failed()}</Notice>
        <Show
          when={rows().length > 0}
          fallback={<NothingYet label="No keys yet" note="The admin key is the only access." />}
        >
          <Table
            head={
              <>
                <Th>Name</Th>
                <Th>Role</Th>
                <Th>Database pin</Th>
                <Th>Last used</Th>
                <Th>Status</Th>
                <Th unseen>Actions</Th>
              </>
            }
          >
            <For each={rows()}>
              {(k) => (
                <Tr dim={k.disabled}>
                  <Td strong>{k.name}</Td>
                  <Td>
                    <Badge tone="neutral">{k.role}</Badge>
                  </Td>
                  <Td muted>{k.database_pin ?? "—"}</Td>
                  <Td muted>{k.last_used_at ? when(k.last_used_at) : "never"}</Td>
                  <Td>
                    <Badge tone={k.disabled ? "bad" : "good"}>{k.disabled ? "disabled" : "active"}</Badge>
                  </Td>
                  <Td controls>
                    <Menu aria-label={`Actions for key ${k.name}`}>
                      <MenuItem onSelect={() => setEditing(k)}>Edit…</MenuItem>
                      <Show
                        when={!k.disabled}
                        fallback={<MenuItemUnavailable why="enable it first">Rotate…</MenuItemUnavailable>}
                      >
                        <MenuItem onSelect={() => act(k, "rotate")}>Rotate…</MenuItem>
                      </Show>
                      <MenuItem onSelect={() => act(k, k.disabled ? "enable" : "disable")}>
                        {k.disabled ? "Enable…" : "Disable…"}
                      </MenuItem>
                      <MenuItem danger onSelect={() => act(k, "revoke")}>
                        Revoke…
                      </MenuItem>
                    </Menu>
                  </Td>
                </Tr>
              )}
            </For>
          </Table>
        </Show>

        <Show when={editing()} keyed fallback={<NewKey
              onMade={(s) => {
                show(s);
                void load();
              }}
            />}>
          {(k) => (
            <EditKey
              editing={k}
              onDone={() => {
                setEditing(undefined);
                void load();
              }}
            />
          )}
        </Show>
      </Section>

      <Secrets onRotated={show} />

      <Show when={asked()}>{(q) => <Confirm {...q()} onClose={() => ask(undefined)} />}</Show>
      <Show when={shown()}>
        {(s) => (
          <Dialog title="Shown once" undismissible>
            <ShownOnce label={s().label} value={s().value} note={s().note} onDone={() => show(undefined)} />
          </Dialog>
        )}
      </Show>
    </>
  );
}

function NewKey(props: { onMade: (shown: Shown) => void }) {
  const [name, setName] = createSignal("");
  const [role, setRole] = createSignal<GatewayRole>("read");
  const [pin, setPin] = createSignal("");
  return (
    <Section title="New key" level={3}>
      <Form
        verb="Create"
        disabled={name().trim() === ""}
        act={async () => {
          const made = await gateway<{ key: string }>("/v1/admin/keys", {
            method: "POST",
            body: keyCreation(name(), role(), pin()),
          });
          props.onMade({ label: `Key “${name().trim()}”`, value: made.key, note: "Copy it now: it is shown once and never stored." });
          setName("");
          setPin("");
        }}
      >
        <Field label="Name" placeholder="shed-esp32" value={name()} onValue={setName} />
        <Select label="Role" value={role()} onValue={(r) => setRole(r as GatewayRole)}>
          <For each={ROLES}>{([value, label]) => <option value={value}>{label}</option>}</For>
        </Select>
        <Field label="Database pin" hint="Optional: restricts the key to one capture database." value={pin()} onValue={setPin} />
      </Form>
    </Section>
  );
}

function EditKey(props: { editing: KeySummary; onDone: () => void }) {
  const [name, setName] = createSignal(props.editing.name);
  const [pin, setPin] = createSignal(props.editing.database_pin ?? "");
  return (
    <Section title={`Edit “${props.editing.name}”`} blurb="To change a key's role, make a new key." level={3}>
      <Form
        disabled={name().trim() === ""}
        actions={
          <Button tone="plain" onClick={() => props.onDone()}>
            Cancel
          </Button>
        }
        act={async () => {
          await gateway(`/v1/admin/keys/${props.editing.id}`, { method: "PATCH", body: keyEdit(name(), pin()) });
          props.onDone();
        }}
      >
        <Field label="Name" value={name()} onValue={setName} />
        <Field label="Database pin" hint="Empty lets it reach every database." value={pin()} onValue={setPin} />
      </Form>
    </Section>
  );
}

type Secret = "db-password" | "admin-key";

const NAMES: Record<Secret, string> = { "db-password": "Database password", "admin-key": "Admin key" };

function confirmation(secret: Secret, persisted: boolean): string {
  if (secret === "db-password") {
    return persisted
      ? "Rotate the database password? The gateway changes it and saves it. Open connections stay up."
      : "Rotate the database password? It comes from the deployment's env, so the new one is not saved. Copy it into POSTGRES_PASSWORD before the gateway next restarts: until then a restarted gateway cannot reach the database.";
  }
  return persisted
    ? "Rotate the admin key? The current one stops working at once."
    : "Rotate the admin key? The current one stops working at once and stays dead, even if the gateway restarts with it. The new one is kept across restarts; copy it into WIRETAP_ADMIN_KEY too, so the env matches.";
}

function Secrets(props: { onRotated: (shown: Shown) => void }) {
  const [sources, setSources] = createSignal<SecretSources>();
  const reading = createAction();
  const load = () => reading.run(async () => setSources(await gateway<SecretSources>("/v1/admin/secrets")));
  onMount(() => void load());
  const [asked, ask] = createSignal<Question>();

  const rotate = (secret: Secret, persisted: boolean) =>
    ask({
      title: `Rotate the ${NAMES[secret].toLowerCase()}`,
      verb: "Rotate",
      children: <p>{confirmation(secret, persisted)}</p>,
      act: async () => {
        const r = await gateway<RotatedSecret>(`/v1/admin/secrets/${secret}/rotate`, { method: "POST" });
        props.onRotated({ label: `New ${NAMES[secret].toLowerCase()}`, value: r.secret, note: r.note });
        await load();
      },
    });

  return (
    <Section title="Secrets" blurb="The gateway's own database password and admin key.">
      <Notice tone="bad">{reading.failed()}</Notice>
      <Show when={sources()}>
        {(s) => {
          const rows: [Secret, boolean][] = [
            ["db-password", s().db_password.persisted],
            ["admin-key", s().admin_key.persisted],
          ];
          const where = (secret: Secret, persisted: boolean) =>
            secret === "admin-key" && !s().admin_key.configured
              ? "None configured: set WIRETAP_ADMIN_KEY or WIRETAP_ADMIN_KEY_FILE to have one."
              : persisted
                ? "Saved in a file the gateway owns."
                : "From the deployment's env: a new one must be copied there by hand.";
          return (
            <Table
              head={
                <>
                  <Th>Secret</Th>
                  <Th>Where it is kept</Th>
                  <Th unseen>Actions</Th>
                </>
              }
            >
              <For each={rows}>
                {([secret, persisted]) => (
                  <Tr>
                    <Td strong>{NAMES[secret]}</Td>
                    <Td muted>
                      {where(secret, persisted)}
                      <Show when={secret === "admin-key" && s().admin_key.stale_config}>
                        {" "}
                        <Badge tone="bad">
                          {persisted ? "the admin key file" : "the deployment's env"} still holds a rotated key
                        </Badge>
                      </Show>
                    </Td>
                    <Td controls>
                      <Show when={secret !== "admin-key" || s().admin_key.configured}>
                        <Button onClick={() => rotate(secret, persisted)}>Rotate…</Button>
                      </Show>
                    </Td>
                  </Tr>
                )}
              </For>
            </Table>
          );
        }}
      </Show>
      <Show when={asked()}>{(q) => <Confirm {...q()} onClose={() => ask(undefined)} />}</Show>
    </Section>
  );
}
