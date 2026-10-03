import { Show, createMemo, createSignal } from "solid-js";

import {
  About,
  AccountSettings,
  AuditLog,
  BackupSettings,
  Certificates,
  Chrome,
  FactoryReset,
  HostSettings,
  Logs,
  NetworkSettings,
  Notice,
  OutboundTrust,
  SshSettings,
  TabPanel,
  Tabs,
  allows,
  useSession,
} from "@wired-square/appliance-ui";
import type { Role } from "@wired-square/appliance-ui";

/** One row per section `DaemonFiles` puts in a bundle, by its name there. */
const BACKUP_SECTIONS = [
  {
    name: "wiretap-server",
    label: "Capture daemon",
    hint: "/etc/wiretap-server, not the disk cache",
  },
];

/** `allows` decides what is offered; the daemon decides what is permitted. */
const TABS = [
  { id: "about", label: "About", needs: "admin" },
  { id: "host", label: "Host", needs: "admin" },
  { id: "network", label: "Network", needs: "admin" },
  { id: "ssh", label: "SSH", needs: "admin" },
  { id: "accounts", label: "Accounts", needs: "admin" },
  { id: "certificates", label: "Certificate", needs: "admin" },
  { id: "trust", label: "Outbound trust", needs: "admin" },
  { id: "backup", label: "Backup", needs: "admin" },
  { id: "audit", label: "Audit log", needs: "admin" },
  { id: "logs", label: "Logs", needs: "admin" },
] as const satisfies readonly { id: string; label: string; needs: Role }[];

type TabId = (typeof TABS)[number]["id"];

/** The fragment's tab, so a link from the HTTPS port section lands on its screen. */
const opened = (): TabId => TABS.find((t) => `#${t.id}` === window.location.hash)?.id ?? "about";

export function App() {
  const { me, refresh } = useSession();
  const [tab, setTab] = createSignal<TabId>(opened());
  const choose = (id: TabId) => {
    setTab(id);
    history.replaceState(null, "", `#${id}`);
  };

  return (
    <Show when={me()}>
      {(who) => {
        const offered = createMemo(() => TABS.filter((t) => allows(who().role, t.needs)));
        const shown = createMemo(() =>
          offered().some((t) => t.id === tab()) ? tab() : offered()[0]?.id,
        );

        return (
          <Chrome
            nav={<Tabs aria-label="Settings" tabs={offered()} selected={shown()} onChange={choose} />}
          >
            <Show when={offered().length === 0}>
              <Notice tone="neutral">
                Nothing here is offered to a {who().role}; an administrator manages this box.
              </Notice>
            </Show>
            <TabPanel id="about" selected={shown()}>
              <About />
              <FactoryReset onReset={refresh} />
            </TabPanel>
            <TabPanel id="host" selected={shown()}>
              <HostSettings />
            </TabPanel>
            <TabPanel id="network" selected={shown()}>
              <NetworkSettings />
            </TabPanel>
            <TabPanel id="ssh" selected={shown()}>
              <SshSettings />
            </TabPanel>
            <TabPanel id="accounts" selected={shown()}>
              <AccountSettings me={who()} />
            </TabPanel>
            <TabPanel id="certificates" selected={shown()}>
              <Certificates />
            </TabPanel>
            <TabPanel id="trust" selected={shown()}>
              <OutboundTrust />
            </TabPanel>
            <TabPanel id="backup" selected={shown()}>
              <BackupSettings sections={BACKUP_SECTIONS} onRestored={refresh} />
            </TabPanel>
            <TabPanel id="audit" selected={shown()}>
              <AuditLog />
            </TabPanel>
            <TabPanel id="logs" selected={shown()}>
              <Logs />
            </TabPanel>
          </Chrome>
        );
      }}
    </Show>
  );
}
