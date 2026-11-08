/**
 * The gateway bundled with this UI: its screens, offered by the role the
 * gateway gives the caller. Every route is gated in the gateway regardless.
 */
import { Show, createSignal, onMount } from "solid-js";
import type { JSX } from "solid-js";

import { useRefresh } from "@wired-square/appliance-ui";
import { Button, Notice, Views, createAction, routed } from "@wired-square/kit";
import type { Routed } from "@wired-square/kit";

import { Activity } from "./Activity";
import { Daemons } from "./Daemons";
import { Databases } from "./Databases";
import { Health } from "./Health";
import { Ingest } from "./Ingest";
import { Keys } from "./Keys";
import { Log } from "./Log";
import { gateway } from "./api";
import { canMigrate, screensFor, unsettled } from "./wire";
import type { Caller, DatabaseList, ScreenId } from "./wire";

/** The database list, shared by the screens that read it, re-read every 2 s while something in it is unsettled. */
export interface Listed {
  list: () => DatabaseList | undefined;
  failed: () => string | undefined;
  load: () => Promise<void>;
}

function createListed(): Listed {
  const [list, setList] = createSignal<DatabaseList>();
  const reading = createAction({ supersede: true });
  const load = () => reading.run(async () => setList(await gateway<DatabaseList>("/v1/databases")));
  onMount(() => void load());
  useRefresh(() => {
    const now = list();
    if (now && unsettled(now)) void load();
  }, 2_000);
  return { list, failed: reading.failed, load };
}

export function Gateway(props: Routed & { caller: Caller }) {
  const listed = createListed();
  const admin = () => props.caller.role === "admin";
  const route = routed(props);
  const panels: Record<ScreenId, () => JSX.Element> = {
    databases: () => <Databases listed={listed} admin={admin()} />,
    keys: () => <Keys />,
    ingest: () => <Ingest />,
    daemons: () => <Daemons />,
    activity: () => <Activity listed={listed} />,
    log: () => <Log />,
    health: () => <Health listed={listed} />,
  };
  const waiting = () => listed.list()?.databases.filter(canMigrate).length ?? 0;

  return (
    <>
      <Show when={waiting() > 0}>
        <Notice tone="warn">
          {waiting() === 1
            ? "1 database is waiting for migration. It refuses reads, and buffers ingest until migrated."
            : `${waiting()} databases are waiting for migration. They refuse reads, and buffer ingest until migrated.`}{" "}
          <Button tone="plain" onClick={() => route.onChange("databases")}>
            Show
          </Button>
        </Notice>
      </Show>
      <Views
        aria-label="Gateway"
        group="gateway"
        views={screensFor(props.caller.role).map((s) => ({ id: s.id, label: s.label, panel: panels[s.id] }))}
        {...route}
      />
    </>
  );
}
