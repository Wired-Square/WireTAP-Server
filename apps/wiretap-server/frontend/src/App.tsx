/**
 * Signed in: the Capture screen, the gateway's where one is bundled, then the
 * chassis's screens. Tabs are offered by role so nobody is handed one that
 * would 403; every route is gated in the daemon regardless.
 */
import { Show, createMemo, createResource, createSignal } from "solid-js";

import {
  About,
  AccountSettings,
  ApiError,
  AuditLog,
  Chrome,
  Logs,
  allows,
  api,
  useSession,
} from "@wired-square/appliance-ui";
import type { Role } from "@wired-square/appliance-types";
import { Loaded, TabPanel, Tabs } from "@wired-square/kit";

import { Capture } from "./Capture";
import { Gateway } from "./gateway/Gateway";
import type { Caller } from "./gateway/wire";

const TABS = [
  { id: "capture", label: "Capture", needs: "viewer" },
  { id: "gateway", label: "Gateway", needs: "viewer" },
  { id: "users", label: "Users", needs: "viewer" },
  { id: "audit", label: "Audit log", needs: "admin" },
  { id: "logs", label: "Logs", needs: "admin" },
  { id: "about", label: "About", needs: "admin" },
] as const satisfies readonly { id: string; label: string; needs: Role }[];

type TabId = (typeof TABS)[number]["id"];

const [openedTab, openedView] = window.location.hash.slice(1).split("/");
const opened = (): TabId => TABS.find((t) => t.id === openedTab)?.id ?? "capture";

/** The gateway bundled with this UI, if there is one: it 404s where there is none. */
async function bundledGateway(): Promise<Caller | undefined> {
  try {
    return await api<Caller>("/api/gateway/v1/whoami");
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return undefined;
    throw e;
  }
}

export function App() {
  const { me } = useSession();
  const [tab, setTab] = createSignal<TabId>(opened());
  const [view, setView] = createSignal<string | undefined>(openedView);
  const [gateway, { refetch }] = createResource(bundledGateway);
  const hasGateway = () => gateway.state === "errored" || (gateway.state === "ready" && gateway() !== undefined);
  const choose = (id: TabId, to?: string) => {
    setTab(id);
    setView(to);
    history.replaceState(null, "", to ? `#${id}/${to}` : `#${id}`);
  };

  return (
    <Show when={me()}>
      {(who) => {
        // Behind a console the console keeps the accounts.
        const account = () => {
          const me = who();
          return "enabled" in me ? me : undefined;
        };
        const offered = createMemo(() =>
          TABS.filter(
            (t) =>
              allows(who().role, t.needs) && (t.id !== "users" || account()) && (t.id !== "gateway" || hasGateway()),
          ),
        );
        const shown = createMemo(() => (offered().some((t) => t.id === tab()) ? tab() : "capture"));

        return (
          <Chrome
            nav={<Tabs aria-label="Sections" tabs={offered()} selected={shown()} onChange={choose} />}
          >
            <TabPanel id="capture" selected={shown()}>
              <Capture me={who()} />
            </TabPanel>
            <TabPanel id="gateway" selected={shown()}>
              <Loaded of={gateway} refetch={refetch} reading="the gateway">
                {(caller) => (
                  <Show when={caller()}>
                    {(c) => <Gateway caller={c()} view={view()} onView={(to) => choose("gateway", to)} />}
                  </Show>
                )}
              </Loaded>
            </TabPanel>
            <Show when={account()}>
              {(user) => (
                <TabPanel id="users" selected={shown()}>
                  <AccountSettings me={user()} view={view()} onView={(to) => choose("users", to)} />
                </TabPanel>
              )}
            </Show>
            <TabPanel id="audit" selected={shown()}>
              <AuditLog />
            </TabPanel>
            <TabPanel id="logs" selected={shown()}>
              <Logs />
            </TabPanel>
            <TabPanel id="about" selected={shown()}>
              <About />
            </TabPanel>
          </Chrome>
        );
      }}
    </Show>
  );
}
