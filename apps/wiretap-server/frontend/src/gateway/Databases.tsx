import { For, Show, createEffect, createSignal, on, onCleanup } from "solid-js";
import type { JSX } from "solid-js";

import {
  Badge,
  Badges,
  Button,
  Confirm,
  Field,
  Form,
  Menu,
  MenuItem,
  MenuItemUnavailable,
  Notice,
  NothingYet,
  PickFile,
  Progress,
  Refusal,
  Section,
  Select,
  Table,
  Td,
  Th,
  Tr,
  createAction,
  day,
  duration,
  keyedRows,
  size,
} from "@wired-square/kit";
import type { ConfirmProps } from "@wired-square/kit";

import type { Listed } from "./Gateway";
import { followExport, gateway, uploadArchive } from "./api";
import {
  ROLLUP_FRESH_SECS,
  SCOPES,
  archiveUnavailable,
  canMigrate,
  chosenFor,
  exportWindow,
  formatWindow,
  fromUs,
  importPath,
  localDay,
  migrationEta,
  readManifest,
  rollupNeedsWork,
} from "./wire";
import type { ArchiveEstimate, ArchiveWindow, ArchiveManifest, Chosen, DatabaseEntry, Imported, Scope } from "./wire";

type Question = Omit<ConfirmProps, "onClose">;

const NAME_PATTERN = "[a-z][a-z0-9_]*";

export function Databases(props: { listed: Listed; admin: boolean }) {
  const rows = keyedRows(() => props.listed.list()?.databases, "name");
  const target = () => props.listed.list()?.schema_version ?? 0;
  const [asked, ask] = createSignal<Question>();
  const acting = createAction();
  const [name, setName] = createSignal("");

  const rebuild = (db: string) =>
    acting.run(async () => {
      await gateway(`/v1/databases/${db}/rollup/refresh`, { method: "POST" });
      await props.listed.load();
    });

  const migrate = (db: string) =>
    ask({
      title: "Migrate database",
      verb: "Migrate",
      tone: "primary",
      children: (
        <p>
          Migrate “{db}” to schema v{target()} now? Until it finishes it refuses reads, which can take hours on a
          large archive. Ingest is buffered and stored once it is done.
        </p>
      ),
      act: async () => {
        await gateway(`/v1/databases/${db}/migrate`, { method: "POST" });
        await props.listed.load();
      },
    });

  const remove = (db: string) =>
    ask({
      title: "Delete database",
      verb: "Delete",
      children: <p>Delete “{db}”? This permanently removes all its captured frames and cannot be undone.</p>,
      act: async () => {
        await gateway(`/v1/databases/${db}`, { method: "DELETE" });
        await props.listed.load();
      },
    });

  const exportOf = (db: string) => {
    const [choice, setChoice] = createSignal<ExportChoice>({ window: null, empty: true });
    ask({
      title: `Export “${db}”`,
      verb: "Export",
      tone: "primary",
      get disabled() {
        return choice().empty;
      },
      // A getter, so the form is made under the dialog's owner rather than in this handler.
      get children() {
        return <ExportForm db={db} onChoice={setChoice} />;
      },
      act: async () => {
        const { url } = await gateway<{ url: string }>(`/v1/databases/${db}/export`, {
          method: "POST",
          body: choice().window ?? {},
        });
        followExport(url);
      },
    });
  };

  const importInto = (replacing?: string) => {
    let submit: () => Promise<Imported> = () => Promise.reject(new Refusal("Choose an archive."));
    ask({
      title: replacing ? `Replace “${replacing}” from an archive` : "Import an archive as a new database",
      verb: replacing ? "Replace" : "Import",
      tone: replacing ? "danger" : "primary",
      get children() {
        return <ImportForm replacing={replacing} handle={(s) => (submit = s)} />;
      },
      act: async () => {
        const done = await submit();
        const frames = (done.rows.capture_frame ?? 0).toLocaleString();
        const events = (done.rows.events ?? 0).toLocaleString();
        acting.succeed(
          `${done.replaced ? "Replaced" : "Created"} “${done.database}” from “${done.source}”: ${frames} frames and ${events} events.` +
            (done.attention ? ` ${done.attention}` : ""),
        );
        await props.listed.load();
      },
    });
  };

  return (
    <Section
      title="Capture databases"
      blurb="Where the gateway archives frames. Its own database, which holds its keys and catalogues, is not listed and takes no captures."
      actions={
        <Show when={props.admin}>
          <Button onClick={() => importInto()}>Import an archive…</Button>
        </Show>
      }
    >
      <Notice tone="bad">{props.listed.failed() ?? acting.failed()}</Notice>
      <Notice tone="good">{acting.succeeded()}</Notice>
      <Show
        when={rows().length > 0}
        fallback={<NothingYet label="No capture databases" note="Create one, or let a device name one in its HELLO." />}
      >
        <Table
          head={
            <>
              <Th>Name</Th>
              <Th right>Size</Th>
              <Th>Schema</Th>
              <Th>Rollup</Th>
              <Show when={props.admin}>
                <Th unseen>Actions</Th>
              </Show>
            </>
          }
        >
          <For each={rows()}>
            {(d) => (
              <Tr>
                <Td strong>{d.name}</Td>
                <Td right numeric>
                  {size(d.size_bytes)}
                </Td>
                <Td>
                  <Badges>
                    <SchemaBadge db={d} target={target()} />
                    <Show when={props.admin && canMigrate(d)}>
                      <Button tone="plain" onClick={() => migrate(d.name)}>
                        Migrate now
                      </Button>
                    </Show>
                  </Badges>
                  <Migration db={d} />
                </Td>
                <Td>
                  <Badges>
                    <RollupBadge db={d} />
                    <Show when={props.admin && rollupNeedsWork(d)}>
                      <Button tone="plain" pending={acting.pending()} onClick={() => void rebuild(d.name)}>
                        Rebuild
                      </Button>
                    </Show>
                  </Badges>
                </Td>
                <Show when={props.admin}>
                  <Td controls>
                    <Menu aria-label={`Actions for database ${d.name}`}>
                      <Show
                        when={!archiveUnavailable(d)}
                        fallback={
                          <>
                            <MenuItemUnavailable why={archiveUnavailable(d)}>Export…</MenuItemUnavailable>
                            <MenuItemUnavailable why={archiveUnavailable(d)}>Replace from an archive…</MenuItemUnavailable>
                          </>
                        }
                      >
                        <MenuItem onSelect={() => exportOf(d.name)}>Export…</MenuItem>
                        <MenuItem onSelect={() => importInto(d.name)}>Replace from an archive…</MenuItem>
                      </Show>
                      <MenuItem danger onSelect={() => remove(d.name)}>
                        Delete…
                      </MenuItem>
                    </Menu>
                  </Td>
                </Show>
              </Tr>
            )}
          </For>
        </Table>
      </Show>

      <p class="app-muted">
        A database behind the current schema waits for Migrate now, or is migrated on start with
        WIRETAP_AUTO_MIGRATE=true. It refuses reads until the migration is done; ingest is buffered meanwhile. A
        rollup a few hours behind is the maintenance policy working normally; “incomplete” means the stored summaries
        do not reach the first frame, which the gateway repairs on start. Rebuilding blocks nothing.
      </p>

      <Show when={props.admin}>
        <Section title="New database" level={3}>
          <Form
            verb="Create"
            disabled={name() === ""}
            done="Created."
            act={async () => {
              await gateway("/v1/databases", { method: "POST", body: { name: name() } });
              setName("");
              await props.listed.load();
            }}
          >
            <Field
              label="Name"
              hint="Lower case letters, digits and underscores, starting with a letter."
              pattern={NAME_PATTERN}
              value={name()}
              onValue={(v) => setName(v.toLowerCase())}
            />
          </Form>
        </Section>
      </Show>

      <Show when={asked()}>{(q) => <Confirm {...q()} onClose={() => ask(undefined)} />}</Show>
    </Section>
  );
}

function SchemaBadge(props: { db: DatabaseEntry; target: number }) {
  const d = () => props.db;
  const eta = () => (d().migration?.phase === "backfill" ? migrationEta(d().migration!) : undefined);
  return (
    <Show
      when={d().import_busy_secs === null}
      fallback={
        <Badge tone="warn" title="Reads are refused, and ingest held, until it is in place">
          Importing {duration(d().import_busy_secs)}
        </Badge>
      }
    >
      {(() => {
        switch (d().schema_state) {
          case "current":
            return <Badge tone="good">v{d().schema_version}</Badge>;
          case "pending":
            return (
              <Badge tone="warn" title="Refusing reads, and buffering ingest, until it is migrated">
                v{d().schema_version} → v{props.target}
              </Badge>
            );
          case "migrating":
            return (
              <>
                <Badge tone="warn" title="Reads are refused, and ingest buffered, until this finishes">
                  Migrating {duration(d().busy_secs)}
                </Badge>
                <Show when={eta() !== undefined}>
                  <Badge tone="neutral">about {duration(eta())} left</Badge>
                </Show>
              </>
            );
          case "failed":
            return (
              <Badge tone="bad" title={d().schema_error ?? undefined}>
                Failed
              </Badge>
            );
          default:
            return <span class="app-muted">—</span>;
        }
      })()}
    </Show>
  );
}

const PHASES: Record<string, string> = {
  started: "starting",
  backfill: "converting chunks",
  check: "validating",
  rollup: "rebuilding the rollup",
  drain: "draining buffered ingest",
  done: "finishing",
  failed: "stopped",
};

/** How far a migration has got, or where a failed one stopped. */
function Migration(props: { db: DatabaseEntry }) {
  const running = () => props.db.schema_state === "migrating";
  const buffered = () =>
    props.db.buffered_rows !== null ? `${props.db.buffered_rows.toLocaleString()} frames buffered` : undefined;
  const shown = () => {
    const m = props.db.migration;
    return m && (running() || m.phase !== "done") ? m : undefined;
  };
  return (
    <Show when={shown()} fallback={<Show when={buffered()}>{(b) => <p class="app-muted">{b()}</p>}</Show>}>
      {(m) => {
        const error = () => props.db.schema_error ?? m().last_error;
        const phase = () => PHASES[m().phase] ?? m().phase;
        const facts = () =>
          [
            m().rows_done !== null && `${m().rows_done!.toLocaleString()} rows converted`,
            m().current_chunk_start && `${error() ? "stopped at" : "on"} ${day(m().current_chunk_start)}`,
            buffered(),
          ]
            .filter(Boolean)
            .join(" · ");
        return (
          <>
            <Show
              when={running()}
              fallback={<p class="app-muted">{error() ? "Stopped" : `Last reported: ${phase()}`}</p>}
            >
              <Progress
                label={phase()}
                value={m().chunks_total ? (m().chunks_done ?? 0) : undefined}
                max={m().chunks_total || 1}
              />
            </Show>
            <Show when={facts()}>
              <p class="app-muted">{facts()}</p>
            </Show>
            <Show when={!running() && error()}>
              <Notice tone="bad">{error()}</Notice>
            </Show>
          </>
        );
      }}
    </Show>
  );
}

function RollupBadge(props: { db: DatabaseEntry }) {
  const d = () => props.db;
  return (
    <Show
      when={d().rollup_busy_secs === null}
      fallback={
        <Badge tone="warn" title="Materialising the hourly rollup">
          Rebuilding {duration(d().rollup_busy_secs)}
        </Badge>
      }
    >
      {(() => {
        switch (d().rollup_state) {
          case null:
            return <span class="app-muted">—</span>;
          case "empty":
            return <span class="app-muted">no frames</span>;
          case "incomplete":
            return (
              <Badge tone="warn" title="Stored summaries do not reach the first frame, so rollup queries omit the gap">
                incomplete
              </Badge>
            );
          default: {
            const lag = d().rollup_lag_secs;
            return lag === null || lag <= ROLLUP_FRESH_SECS ? (
              <Badge tone="good">up to date</Badge>
            ) : (
              <Badge tone="warn" title="Buckets newer than the last stored one are recomputed on every query">
                {duration(lag)} behind
              </Badge>
            );
          }
        }
      })()}
    </Show>
  );
}

interface ExportChoice {
  window: ArchiveWindow | null;
  empty: boolean;
}

/** Whole or a window, guided by the database's own bounds, with an estimate of what it would hold. */
function ExportForm(props: { db: string; onChoice: (choice: ExportChoice) => void }) {
  const [bounds, setBounds] = createSignal<ArchiveEstimate>();
  const [chosen, setChosen] = createSignal<Chosen>({ scope: "whole", day: "", month: "", from: "", to: "" });
  const [estimate, setEstimate] = createSignal<ArchiveEstimate>();
  const reading = createAction();
  const estimating = createAction({ supersede: true });
  void reading.run(
    () => gateway<ArchiveEstimate>(`/v1/databases/${props.db}/export/estimate`),
    (b) => {
      setBounds(b);
      setChosen(chosenFor(b));
    },
  );

  const window = () => {
    const b = bounds();
    return b && exportWindow(b, chosen());
  };
  const windowEmpty = () => {
    const w = window();
    return !!w && w.from_us >= w.to_us;
  };
  const set = (change: Partial<Chosen>) => setChosen((c) => ({ ...c, ...change }));

  createEffect(
    on(
      () => JSON.stringify([bounds(), window()]),
      () => {
        const b = bounds();
        const w = window();
        setEstimate(undefined);
        if (!b) return;
        if (w === null) return setEstimate(b);
        if (!w || windowEmpty()) return;
        const timer = setTimeout(
          () =>
            void estimating.run(
              () => gateway<ArchiveEstimate>(`/v1/databases/${props.db}/export/estimate?from_us=${w.from_us}&to_us=${w.to_us}`),
              setEstimate,
            ),
          300,
        );
        onCleanup(() => clearTimeout(timer));
      },
    ),
  );

  const empty = () => (chosen().scope !== "whole" && (!window() || windowEmpty())) || estimate()?.empty === true;
  createEffect(() => props.onChoice({ window: window() ?? null, empty: !bounds() || empty() }));

  return (
    <>
      <Notice tone="bad">{reading.failed()}</Notice>
      <Show when={bounds()} fallback={<Show when={!reading.failed()}><p class="app-muted">Reading the database's bounds…</p></Show>}>
        {(b) => (
          <Show
            when={b().min_ts_us !== null && b().max_ts_us !== null}
            fallback={<p>“{props.db}” holds nothing to export.</p>}
          >
            <p class="app-muted">
              “{props.db}” holds {fromUs(b().min_ts_us!).toLocaleString()} to {fromUs(b().max_ts_us!).toLocaleString()}.
              Times are local.
            </p>
            <Select label="Export" value={chosen().scope} onValue={(scope) => set({ scope: scope as Scope })}>
              <For each={SCOPES}>{([value, label]) => <option value={value}>{label}</option>}</For>
            </Select>
            {scopeFields(chosen(), b(), set)}
            <Show when={window() && !windowEmpty()}>
              <p class="app-muted">{formatWindow(window()!)}, within the database's bounds.</p>
            </Show>
            <p>
              {empty()
                ? "Nothing in this window to export."
                : estimate()
                  ? `About ${estimate()!.frames.toLocaleString()} frames and ${estimate()!.events.toLocaleString()} events: an archive of roughly ${size(estimate()!.approx_bytes)}.`
                  : estimating.failed()
                    ? `No estimate: ${estimating.failed()}`
                    : "Estimating…"}
            </p>
          </Show>
        )}
      </Show>
    </>
  );
}

function scopeFields(c: Chosen, b: ArchiveEstimate, set: (change: Partial<Chosen>) => void): JSX.Element {
  const first = localDay(fromUs(b.min_ts_us!));
  const last = localDay(fromUs(b.max_ts_us!));
  switch (c.scope) {
    case "day":
      return <Field label="Day" type="date" min={first} max={last} value={c.day} onValue={(day) => set({ day })} />;
    case "month":
      return (
        <Field
          label="Month"
          type="month"
          min={first.slice(0, 7)}
          max={last.slice(0, 7)}
          value={c.month}
          onValue={(month) => set({ month })}
        />
      );
    case "custom":
      return (
        <>
          <Field label="From" type="datetime-local" value={c.from} onValue={(from) => set({ from })} />
          <Field label="To" type="datetime-local" value={c.to} onValue={(to) => set({ to })} />
        </>
      );
    default:
      return undefined;
  }
}

/** The file, read for its manifest before anything is sent, and the upload's progress. For a new database, its name too. */
function ImportForm(props: { replacing?: string | undefined; handle: (submit: () => Promise<Imported>) => void }) {
  const [name, setName] = createSignal("");
  const [file, setFile] = createSignal<File>();
  const [manifest, setManifest] = createSignal<ArchiveManifest>();
  const [unreadable, setUnreadable] = createSignal<string>();
  const [sent, setSent] = createSignal<number>();

  const choose = async (f: File) => {
    setFile(f);
    setManifest(undefined);
    setUnreadable(undefined);
    try {
      setManifest(await readManifest(f));
    } catch (e) {
      setUnreadable(e instanceof Error ? e.message : String(e));
    }
  };

  props.handle(async () => {
    const into = props.replacing ?? name();
    const f = file();
    if (!into) throw new Refusal("Name the new database.");
    if (!f || !manifest()) throw new Refusal(unreadable() ?? "Choose an archive.");
    setSent(0);
    try {
      return await uploadArchive<Imported>(importPath(into, !!props.replacing), f, setSent);
    } finally {
      setSent(undefined);
    }
  });

  const rows = (table: string) => (manifest()?.tables.find((t) => t.name === table)?.rows ?? 0).toLocaleString();
  return (
    <>
      <Show
        when={props.replacing}
        fallback={
          <Field
            label="New database"
            pattern={NAME_PATTERN}
            value={name()}
            onValue={(v) => setName(v.toLowerCase())}
          />
        }
      >
        {(target) => (
          <p>
            Replace everything in “{target()}” with the archive. This cannot be undone. Ingest for it is held meanwhile
            and stored in the new one.
          </p>
        )}
      </Show>
      <div class="app-actions">
        <PickFile label="Choose archive" accept=".wtar" onFile={(f) => void choose(f)} onProblem={setUnreadable} />
        <Show when={file()}>{(f) => <span class="app-muted">{f().name}</span>}</Show>
      </div>
      <Notice tone="bad">{unreadable()}</Notice>
      <Show when={manifest()}>
        {(m) => (
          <p class="app-muted">
            “{m().database}”, exported {new Date(m().created_at).toLocaleString()}: {rows("capture_frame")} frames and{" "}
            {rows("events")} events, {m().window ? `from ${formatWindow(m().window!)}` : "the whole database"}.
          </p>
        )}
      </Show>
      <Show when={props.replacing ? manifest()?.window : undefined}>
        {(w) => (
          <Notice tone="warn">
            This archive holds only {formatWindow(w())}. After the replace, “{props.replacing}” holds only that window:
            everything else in it now is deleted.
          </Notice>
        )}
      </Show>
      <Show when={sent() !== undefined && file()}>
        {(f) => (
          <Progress
            label={sent()! < f().size ? `Uploading ${size(sent())} of ${size(f().size)}` : "Uploaded. Checking it and putting it in place…"}
            value={sent()! < f().size ? sent() : undefined}
            max={f().size}
          />
        )}
      </Show>
    </>
  );
}
