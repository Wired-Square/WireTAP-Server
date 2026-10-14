/**
 * Signed in: the Capture screen, then the chassis's screens. Tabs are offered
 * by role so nobody is handed one that would 403; every route is gated in the
 * daemon regardless.
 */
import { Show, createMemo, createSignal } from "solid-js";

import {
  About,
  AccountSettings,
  AuditLog,
  Button,
  Chrome,
  Logs,
  PasswordSettings,
  TabPanel,
  Tabs,
  allows,
  useSession,
} from "@wired-square/appliance-ui";
import type { Role } from "@wired-square/appliance-ui";

import { Capture } from "./Capture";

const TABS = [
  { id: "capture", label: "Capture", needs: "viewer" },
  { id: "accounts", label: "Accounts", needs: "admin" },
  { id: "audit", label: "Audit log", needs: "admin" },
  { id: "logs", label: "Logs", needs: "admin" },
  { id: "about", label: "About", needs: "admin" },
] as const satisfies readonly { id: string; label: string; needs: Role }[];

type TabId = (typeof TABS)[number]["id"];

const opened = (): TabId => TABS.find((t) => `#${t.id}` === window.location.hash)?.id ?? "capture";

export function App() {
  const { me } = useSession();
  const [tab, setTab] = createSignal<TabId>(opened());
  const choose = (id: TabId) => {
    setTab(id);
    history.replaceState(null, "", `#${id}`);
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
          TABS.filter((t) => allows(who().role, t.needs) && (t.id !== "accounts" || account())),
        );
        const shown = createMemo(() => (offered().some((t) => t.id === tab()) ? tab() : "capture"));

        return (
          <Chrome
            nav={<Tabs aria-label="Sections" tabs={offered()} selected={shown()} onChange={choose} />}
            changePassword={(close) => (
              <PasswordSettings
                actions={
                  <Button tone="plain" onClick={close}>
                    Close
                  </Button>
                }
              />
            )}
          >
            <TabPanel id="capture" selected={shown()}>
              <Capture me={who()} />
            </TabPanel>
            <Show when={account()}>
              {(user) => (
                <TabPanel id="accounts" selected={shown()}>
                  <AccountSettings me={user()} />
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
