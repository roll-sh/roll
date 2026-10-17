"use client";

import { Ban, Boxes, Pause, Play, RefreshCw, RotateCcw, Send, Trash2 } from "lucide-react";
import { useState } from "react";
import {
  formatDuration,
  formatRate,
  formatTime,
  rel,
  tasks,
  type FunctionDef,
  type PendingOp,
  type Run,
  type RunDetail,
  type RunStatus,
} from "@/lib/tasks-api";
import { MessageList } from "./messages";
import {
  btn,
  Chip,
  CodeBlock,
  CopyButton,
  Drawer,
  Empty,
  Field,
  ICON,
  iconBtn,
  IdLink,
  InlineError,
  input,
  LoadMore,
  Loading,
  Panel,
  parseJsonInput,
  primary,
  Section,
  Segmented,
  Spinner,
  StateDot,
  useActions,
  useNow,
  useOpenDetail,
  usePoll,
} from "./ui";

const FINAL: RunStatus[] = ["completed", "failed", "canceled"];

const STATUSES: { value: RunStatus | "all"; label: string }[] = [
  { value: "all", label: "All" },
  { value: "queued", label: "Queued" },
  { value: "running", label: "Running" },
  { value: "waiting", label: "Waiting" },
  { value: "completed", label: "Completed" },
  { value: "failed", label: "Failed" },
  { value: "canceled", label: "Canceled" },
];

export function FunctionsTab() {
  const [fnFilter, setFnFilter] = useState("");
  return (
    <div className="space-y-4">
      <div className="grid gap-4 lg:grid-cols-5">
        <Functions onRuns={setFnFilter} />
        <Apps />
      </div>
      <Runs fn={fnFilter} setFn={setFnFilter} />
    </div>
  );
}

function Apps() {
  const { run, busy } = useActions();
  const apps = usePoll(() => tasks.apps(), 10_000);
  const [url, setUrl] = useState("");
  const now = useNow(10_000);

  const act = async (key: string, fn: () => Promise<unknown>) => {
    const out = await run(key, fn);
    if (out !== undefined) apps.reload();
    return out;
  };

  return (
    <Panel className="lg:col-span-2" title="Apps" meta={apps.data ? apps.data.length : undefined}>
      <InlineError error={apps.error} />
      {!apps.data ? (
        <Loading />
      ) : apps.data.length === 0 ? (
        <Empty title="No apps synced">An app is a deployment of the SDK: one URL serving your functions.</Empty>
      ) : (
        <ul>
          {apps.data.map((a) => (
            <li key={a.id} className="flex items-center gap-3 border-b border-line px-5 py-3 last:border-0">
              <Boxes className="size-4 shrink-0 text-subtle" strokeWidth={1.5} />
              <div className="min-w-0 flex-1">
                <p className="truncate font-mono text-[13px]">{a.id}</p>
                <p className="truncate font-mono text-[11px] text-subtle" title={a.url}>
                  {a.url}
                </p>
                <p className="text-[11px] text-subtle">
                  {a.functions.length} function{a.functions.length === 1 ? "" : "s"} · synced {rel(a.synced_at, now)}
                  {a.sdk && ` · ${a.sdk}`}
                </p>
              </div>
              <button title="Resync" disabled={busy !== null} onClick={() => act(`sync-${a.id}`, () => tasks.syncApp(a.url))} className={iconBtn}>
                {busy === `sync-${a.id}` ? <Spinner /> : <RefreshCw {...ICON} />}
              </button>
              <button
                title="Remove app"
                disabled={busy !== null}
                onClick={() => {
                  if (confirm(`Remove ${a.id}? Its runs in progress are canceled.`)) act(`del-${a.id}`, () => tasks.deleteApp(a.id));
                }}
                className={`${iconBtn} hover:text-danger`}
              >
                <Trash2 {...ICON} />
              </button>
            </li>
          ))}
        </ul>
      )}
      <form
        className="mt-auto flex items-center gap-2 border-t border-line px-5 py-3"
        onSubmit={async (e) => {
          e.preventDefault();
          if (url.trim() && (await act("sync", () => tasks.syncApp(url.trim()))) !== undefined) setUrl("");
        }}
      >
        <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://app.example.com/api/roll" className={`${input} min-w-0 flex-1 font-mono text-[12px]`} />
        <button type="submit" disabled={busy !== null || !url.trim()} className={btn}>
          {busy === "sync" ? <span className="shimmer">Syncing…</span> : "Sync"}
        </button>
      </form>
    </Panel>
  );
}

/** Compact chips for a function's flow-control settings. */
function FunctionChips({ f }: { f: FunctionDef }) {
  const chips: [string, string?][] = [];
  for (const t of f.triggers) {
    if (t.event) chips.push([`on ${t.event}`, t.if ? `if ${t.if}` : undefined]);
    if (t.cron) chips.push([`cron ${t.cron}${t.tz ? ` ${t.tz}` : ""}`]);
  }
  if (f.triggers.length === 0) chips.push(["invoke only"]);
  if (f.retries != null) chips.push([`${f.retries} retries`]);
  for (const c of f.concurrency ?? []) chips.push([`${c.limit} at once${c.key ? " per key" : ""}`, c.key]);
  if (f.rate_limit) chips.push([`rate ${formatRate(f.rate_limit)}`, f.rate_limit.key ?? "events over it are dropped"]);
  if (f.throttle) chips.push([`throttle ${formatRate(f.throttle)}`, f.throttle.key ?? "runs over it are delayed"]);
  if (f.debounce) chips.push([`debounce ${formatDuration(f.debounce.period_ms)}`, f.debounce.key]);
  if (f.batch) chips.push([`batch ${f.batch.max_size} / ${formatDuration(f.batch.timeout_ms)}`, f.batch.key]);
  if (f.idempotency) chips.push(["idempotent", f.idempotency]);
  if (f.singleton) chips.push([`singleton ${f.singleton.mode}`, f.singleton.key]);
  for (const c of f.cancel_on ?? []) chips.push([`cancel on ${c.event}`, c.match ?? c.if]);
  if (f.priority) chips.push(["priority", f.priority]);
  if (f.timeouts?.start_ms) chips.push([`start ≤ ${formatDuration(f.timeouts.start_ms)}`]);
  if (f.timeouts?.finish_ms) chips.push([`finish ≤ ${formatDuration(f.timeouts.finish_ms)}`]);
  return (
    <div className="mt-1 flex flex-wrap gap-1">
      {chips.map(([label, title], i) => (
        <Chip key={i} title={title}>
          {label}
        </Chip>
      ))}
    </div>
  );
}

function Functions({ onRuns }: { onRuns: (id: string) => void }) {
  const { run, busy } = useActions();
  const open = useOpenDetail();
  const fns = usePoll(() => tasks.functions(), 10_000);
  const [invoking, setInvoking] = useState<string | null>(null);
  const [data, setData] = useState("{}");

  const act = async <T,>(key: string, fn: () => Promise<T>) => {
    const out = await run(key, fn);
    if (out !== undefined) fns.reload();
    return out;
  };

  return (
    <Panel className="lg:col-span-3" title="Functions" meta={fns.data ? fns.data.length : undefined}>
      <InlineError error={fns.error} />
      {!fns.data ? (
        <Loading />
      ) : fns.data.length === 0 ? (
        <Empty title="No functions yet">Sync an app to register its functions; events and crons then start runs.</Empty>
      ) : (
        <ul>
          {fns.data.map((f) => (
            <li key={f.id} className="border-b border-line last:border-0">
              <div className="flex items-start gap-3 px-5 py-3">
                <span className="pt-1.5">
                  <StateDot state={f.paused ? "paused" : "pending"} title={f.paused ? "paused" : "active"} />
                </span>
                <div className="min-w-0 flex-1">
                  <p className="flex items-center gap-2 text-[13px]">
                    <span className="font-mono">{f.id}</span>
                    {f.name && f.name !== f.id && <span className="truncate text-muted">{f.name}</span>}
                    {f.paused && <Chip>paused</Chip>}
                  </p>
                  <FunctionChips f={f} />
                </div>
                <div className="flex items-center gap-0.5">
                  <button title="Invoke" onClick={() => setInvoking(invoking === f.id ? null : f.id)} className={iconBtn}>
                    <Send {...ICON} />
                  </button>
                  <button
                    title={f.paused ? "Resume" : "Pause"}
                    disabled={busy !== null}
                    onClick={() => act(`pause-${f.id}`, () => (f.paused ? tasks.resumeFunction(f.id) : tasks.pauseFunction(f.id)))}
                    className={iconBtn}
                  >
                    {busy === `pause-${f.id}` ? <Spinner /> : f.paused ? <Play {...ICON} /> : <Pause {...ICON} />}
                  </button>
                  <button onClick={() => onRuns(f.id)} className="rounded-md px-1.5 py-1 text-[11px] text-subtle transition hover:bg-panel-2 hover:text-fg">
                    runs
                  </button>
                </div>
              </div>
              {invoking === f.id && (
                <form
                  className="rise flex items-start gap-2 border-t border-line bg-panel-2/40 px-5 py-3"
                  onSubmit={async (e) => {
                    e.preventDefault();
                    const out = await act(`invoke-${f.id}`, async () => tasks.invoke(f.id, parseJsonInput(data) ?? {}));
                    if (out) {
                      setInvoking(null);
                      open({ kind: "run", id: out.run_id });
                    }
                  }}
                >
                  <textarea
                    autoFocus
                    value={data}
                    onChange={(e) => setData(e.target.value)}
                    rows={3}
                    spellCheck={false}
                    className={`${input} block min-w-0 flex-1 resize-y font-mono text-[12px]`}
                  />
                  <button type="submit" disabled={busy !== null} className={primary}>
                    {busy === `invoke-${f.id}` ? <span className="shimmer">Invoking…</span> : "Invoke"}
                  </button>
                </form>
              )}
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

function Runs({ fn, setFn }: { fn: string; setFn: (f: string) => void }) {
  const open = useOpenDetail();
  const [status, setStatus] = useState<RunStatus | "all">("all");
  const [limit, setLimit] = useState(50);
  const fns = usePoll(() => tasks.functions(), 30_000);
  const runs = usePoll(() => tasks.runs({ function: fn || undefined, status, limit }), 3000, `${fn}|${status}|${limit}`);
  const now = useNow(1000);
  const items = runs.data?.items ?? [];

  return (
    <Panel title="Runs" meta={runs.data ? `${items.length}${runs.data.cursor ? "+" : ""}` : undefined}>
      <div className="flex flex-wrap items-center gap-2 border-b border-line px-5 py-3">
        <select value={fn} onChange={(e) => setFn(e.target.value)} className={`${input} text-[12px]`}>
          <option value="">All functions</option>
          {fns.data?.map((f) => (
            <option key={f.id} value={f.id}>
              {f.id}
            </option>
          ))}
        </select>
        <Segmented value={status} options={STATUSES} onChange={setStatus} />
      </div>
      <InlineError error={runs.error} />
      {!runs.data ? (
        <Loading />
      ) : items.length === 0 ? (
        <Empty title="No runs">Send an event or invoke a function to start one.</Empty>
      ) : (
        <ul>
          {items.map((r) => (
            <li
              key={r.id}
              onClick={() => open({ kind: "run", id: r.id })}
              className="flex cursor-pointer items-center gap-3 border-b border-line px-5 py-2.5 transition last:border-0 hover:bg-panel-2/60"
            >
              <StateDot state={r.status} />
              <div className="min-w-0 flex-1">
                <p className="truncate text-[13px]">
                  <span className="font-mono">{r.function_id}</span>
                  <span className="ml-2 text-[11px] text-subtle">{r.events[0]?.name}</span>
                </p>
                <p className="truncate font-mono text-[11px] text-subtle">{r.id}</p>
              </div>
              <RunStatusLabel r={r} now={now} />
              <span className="w-16 text-right font-mono text-[11px] text-subtle tabular-nums">{runDuration(r, now)}</span>
              <span className="w-20 text-right text-[12px] text-subtle" title={formatTime(r.created_at)}>
                {rel(r.created_at, now)}
              </span>
            </li>
          ))}
        </ul>
      )}
      <LoadMore cursor={runs.data?.cursor} onMore={() => setLimit((l) => l + 50)} loading={false} />
    </Panel>
  );
}

function runDuration(r: Run, now: number) {
  if (!r.started_at) return "—";
  return formatDuration((r.ended_at ?? now) - r.started_at);
}

function RunStatusLabel({ r, now }: { r: Run; now: number }) {
  const wait = r.pending?.[0];
  const text = r.status === "waiting" && wait ? pendingText(wait, now) : r.status;
  const cls =
    r.status === "failed" ? "text-danger" : r.status === "running" ? "shimmer" : r.status === "waiting" || r.status === "queued" ? "text-accent" : "text-muted";
  return <span className={`hidden w-44 truncate text-right text-[12px] sm:block ${cls}`}>{text}</span>;
}

function pendingText(p: PendingOp, now: number) {
  switch (p.op) {
    case "sleep":
      return `sleeping until ${p.until ? formatTime(p.until) : "—"} (${rel(p.until, now)})`;
    case "wait_for_event":
      return `waiting for ${p.target ?? "an event"}${p.until ? `, gives up ${rel(p.until, now)}` : ""}`;
    case "invoke":
      return `waiting on ${p.target ?? "a function"}${p.until ? `, times out ${rel(p.until, now)}` : ""}`;
    default:
      return `${p.op}${p.until ? ` until ${formatTime(p.until)}` : ""}`;
  }
}

export function RunDrawer({ id, onClose }: { id: string; onClose: () => void }) {
  const { run, busy } = useActions();
  const open = useOpenDetail();
  const r = usePoll(() => tasks.run(id), 2500, id);
  const d = r.data;
  const final = d ? FINAL.includes(d.status) : false;

  return (
    <Drawer
      onClose={onClose}
      title={
        <span className="flex items-center gap-2">
          {d && <StateDot state={d.status} />}
          <span className="font-mono text-[13px]">{id}</span>
          <CopyButton text={id} />
        </span>
      }
      actions={
        d && (
          <div className="flex items-center gap-1.5">
            {!final && (
              <button
                disabled={busy !== null}
                onClick={async () => {
                  if (confirm("Cancel this run?") && (await run("run-cancel", () => tasks.cancelRun(id))) !== undefined) r.reload();
                }}
                className={btn}
              >
                <Ban {...ICON} /> Cancel
              </button>
            )}
            <button
              disabled={busy !== null}
              onClick={async () => {
                const next = await run("rerun", () => tasks.rerun(id));
                if (next) open({ kind: "run", id: next.id });
              }}
              className={btn}
            >
              <RotateCcw {...ICON} /> Rerun
            </button>
          </div>
        )
      }
    >
      <InlineError error={r.error} />
      {!d ? <Loading /> : <RunBody d={d} />}
    </Drawer>
  );
}

function RunBody({ d }: { d: RunDetail }) {
  const now = useNow(1000);
  const steps = [...d.steps].sort((a, b) => a.at - b.at);
  return (
    <>
      <Section title="Run">
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
          <Field label="Status">
            <span className={d.status === "failed" ? "text-danger" : d.status === "running" ? "shimmer" : d.status === "waiting" || d.status === "queued" ? "text-accent" : ""}>
              {d.status}
            </span>
          </Field>
          <Field label="Function">{d.function_id}</Field>
          <Field label="App">{d.app_id}</Field>
          <Field label="Invocations">{d.invocations}</Field>
          <Field label="Created">
            <span title={formatTime(d.created_at)}>{rel(d.created_at, now)}</span>
          </Field>
          <Field label="Started">
            <span title={formatTime(d.started_at)}>{d.started_at ? rel(d.started_at, now) : "—"}</span>
          </Field>
          <Field label="Duration">{runDuration(d, now)}</Field>
          {d.parent && (
            <Field label="Parent">
              <IdLink kind="run" id={d.parent[0]} />
            </Field>
          )}
        </div>
      </Section>

      {d.pending && d.pending.length > 0 && (
        <Section title="Blocked on">
          <ul className="space-y-1.5">
            {d.pending.map((p) => (
              <li key={p.step_id} className="flex items-center gap-2 text-[12px]">
                <StateDot state="waiting" />
                <span className="font-mono text-[11px] text-subtle">{p.step_id}</span>
                <span className="text-accent">{pendingText(p, now)}</span>
                {p.child_run && <IdLink kind="run" id={p.child_run} />}
              </li>
            ))}
          </ul>
        </Section>
      )}

      <Section title={`Steps · ${steps.length}`}>
        {steps.length === 0 ? (
          <p className="text-[12px] text-subtle">
            {d.status === "running" ? <span className="shimmer">Running the first step…</span> : "No memoised steps."}
          </p>
        ) : (
          <ol className="relative ml-1 border-l border-line">
            {steps.map((s) => (
              <StepItem key={s.id} s={s} now={now} />
            ))}
            {d.status === "running" && (
              <li className="relative pl-4">
                <span className="absolute top-1.5 -left-[4.5px] size-2 animate-pulse rounded-full bg-accent motion-reduce:animate-none" />
                <span className="shimmer text-[12px]">running…</span>
              </li>
            )}
          </ol>
        )}
      </Section>

      {d.output !== undefined && (
        <Section title="Output">
          <CodeBlock value={d.output} />
        </Section>
      )}
      {d.error !== undefined && d.error !== null && (
        <Section title="Error">
          <div className="text-danger">
            <CodeBlock value={d.error} />
          </div>
        </Section>
      )}

      <Section title={`Triggered by · ${d.events.length}`}>
        <ul className="space-y-2">
          {d.events.map((e, i) => (
            <li key={e.id ?? i}>
              <p className="mb-1 flex items-center gap-2 text-[12px]">
                <span className="font-mono">{e.name}</span>
                {e.id && <IdLink kind="event" id={e.id} />}
              </p>
              <CodeBlock value={e.data} max="max-h-32" />
            </li>
          ))}
        </ul>
      </Section>

      {d.messages.length > 0 && (
        <div className="border-t border-line">
          <p className="px-5 pt-4 pb-1 text-[12px] text-subtle">Invocation messages · {d.messages.length}</p>
          <MessageList items={d.messages} />
        </div>
      )}
    </>
  );
}

function StepItem({ s, now }: { s: RunDetail["steps"][number]; now: number }) {
  const [open, setOpen] = useState(false);
  const failed = s.error !== undefined && s.error !== null;
  return (
    <li className="relative pb-3 pl-4 last:pb-0">
      <span className={`absolute top-1.5 -left-[4.5px] size-2 rounded-full ${failed ? "bg-danger/80" : "bg-muted"}`} />
      <button onClick={() => setOpen((o) => !o)} className="flex w-full flex-wrap items-baseline gap-x-2 text-left text-[12px]">
        <span className="font-medium">{s.name ?? s.id}</span>
        <Chip>{s.op}</Chip>
        {s.attempts > 1 && <span className="font-mono text-[11px] text-subtle">{s.attempts} attempts</span>}
        <span className="ml-auto text-[11px] text-subtle" title={formatTime(s.at)}>
          {rel(s.at, now)}
        </span>
      </button>
      {open && (
        <div className="rise mt-1.5">
          {failed ? (
            <div className="text-danger">
              <CodeBlock value={s.error} max="max-h-40" />
            </div>
          ) : (
            <CodeBlock value={s.data ?? null} max="max-h-40" />
          )}
        </div>
      )}
    </li>
  );
}
