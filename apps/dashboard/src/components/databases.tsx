"use client";

import { DotField, HorizonMark } from "@roll/brand";
import { Check, Copy, Eye, EyeOff, GitBranch, Moon, Play, Plus, Power, Trash2 } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  CU_STEPS,
  formatBytes,
  formatUsd,
  maskUri,
  type Branch,
  type Endpoint,
  type Health,
  type Project,
  type QueryResult,
  type Usage,
} from "@/lib/roll-api";
import { AppShell } from "./app-shell";
import { ago, btn, input, Offline, primary, useCopy } from "./ui";
import { useMarkState } from "./mark-state";

const POLL_MS = 2500;

const SUSPEND_OPTIONS = [
  { label: "1 min", value: 60 },
  { label: "5 min", value: 300 },
  { label: "15 min", value: 900 },
  { label: "1 hour", value: 3600 },
  { label: "Never", value: 0 },
];

function StateDot({ state }: { state: Endpoint["state"] | undefined }) {
  const cls =
    state === "running" ? "bg-ok" : state === "starting" ? "bg-accent animate-pulse motion-reduce:animate-none" : "border border-line-2 bg-transparent";
  return <span className={`inline-block size-2 shrink-0 rounded-full ${cls}`} />;
}

export function Databases() {
  const { play, setState } = useMarkState();
  const [health, setHealth] = useState<Health | null>(null);
  const [offline, setOffline] = useState(false);
  const [projects, setProjects] = useState<Project[] | null>(null);
  const [projectId, setProjectId] = useState<string | null>(null);
  const [branchId, setBranchId] = useState<string | null>(null);
  const [usage, setUsage] = useState<Usage | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);

  const project = projects?.find((p) => p.id === projectId) ?? projects?.[0] ?? null;
  const branch = project?.branches.find((b) => b.id === branchId) ?? project?.branches[0] ?? null;

  const refresh = useCallback(async () => {
    try {
      const [h, ps] = await Promise.all([api.health(), api.projects()]);
      setHealth(h);
      setProjects(ps);
      setOffline(false);
    } catch {
      setOffline(true);
    }
  }, []);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      await refresh();
      if (alive) timer = setTimeout(loop, POLL_MS);
    };
    loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [refresh]);

  const pid = project?.id;
  useEffect(() => {
    if (!pid) return;
    let alive = true;
    const load = () => api.usage(pid, 14).then((u) => alive && setUsage(u), () => {});
    load();
    const t = setInterval(load, POLL_MS * 4);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [pid]);

  /** Run a control-plane action with the logo reflecting progress. */
  const run = async (key: string, fn: () => Promise<unknown>) => {
    setBusy(key);
    setError(null);
    setState("deploying");
    try {
      await fn();
      play([["success", 1400]]);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      play([["error", 1600]]);
    } finally {
      setBusy(null);
    }
  };

  return (
    <AppShell
      crumb="databases"
      actions={
        <button onClick={() => setCreating((c) => !c)} disabled={offline} className={primary}>
          <Plus className="size-3.5" /> New database
        </button>
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-[-0.04em]">Databases</h1>
          <p className="mt-1 text-[13px] text-muted">
            Serverless Postgres — scales to zero, resizes live, branches in milliseconds.
          </p>
        </div>
        {health && (
          <p className="font-mono text-[11px] text-subtle">
            rolld {health.version} · proxy {health.proxy} · {health.host_cpus} vCPU host
          </p>
        )}
      </div>

      {error && (
        <p className="rise mt-4 rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[13px] text-danger">
          {error}
        </p>
      )}

      {creating && health && (
        <CreateDatabase
          health={health}
          busy={busy === "create"}
          onCancel={() => setCreating(false)}
          onCreate={(name, v) =>
            run("create", async () => {
              const p = await api.createProject(name, v);
              setProjectId(p.id);
              setBranchId(null);
              setCreating(false);
            })
          }
        />
      )}

      {offline ? (
        <Offline />
      ) : projects === null ? (
        <p className="mt-10 text-[13px] text-subtle">
          <span className="shimmer">Connecting to rolld…</span>
        </p>
      ) : projects.length === 0 ? (
        <Empty onCreate={() => setCreating(true)} />
      ) : (
        project && (
          <>
            {projects.length > 1 && (
              <div className="mt-6 flex flex-wrap gap-1.5">
                {projects.map((p) => (
                  <button
                    key={p.id}
                    onClick={() => {
                      setProjectId(p.id);
                      setBranchId(null);
                      setUsage(null);
                    }}
                    className={`rounded-full border px-3 py-1 text-[12px] transition ${
                      p.id === project.id ? "border-line-2 bg-panel-2 text-fg" : "border-line text-muted hover:text-fg"
                    }`}
                  >
                    {p.name}
                  </button>
                ))}
              </div>
            )}

            <UsageStats project={project} usage={usage} />

            <div className="mt-4 grid gap-4 lg:grid-cols-5">
              <Branches
                project={project}
                selected={branch?.id ?? null}
                onSelect={setBranchId}
                busy={busy}
                run={run}
              />
              <UsageChart usage={usage} />
            </div>

            {branch && (
              <div className="mt-4 grid gap-4 lg:grid-cols-5">
                <SqlConsole key={branch.id} branch={branch} />
                <BranchSettings key={`s-${branch.id}`} project={project} branch={branch} busy={busy} run={run} />
              </div>
            )}

            <div className="mt-8 flex items-center justify-between border-t border-line pt-4 text-[12px] text-subtle">
              <span className="font-mono">
                {project.id} · postgres {project.pg_version} · {project.region}
              </span>
              <button
                className="inline-flex items-center gap-1.5 hover:text-danger"
                onClick={() => {
                  if (confirm(`Delete ${project.name} and all of its branches? This can't be undone.`)) {
                    run("delete-project", async () => {
                      await api.deleteProject(project.id);
                      setProjectId(null);
                    });
                  }
                }}
              >
                <Trash2 className="size-3.5" /> Delete database
              </button>
            </div>
          </>
        )
      )}
    </AppShell>
  );
}

function CreateDatabase({
  health,
  busy,
  onCreate,
  onCancel,
}: {
  health: Health;
  busy: boolean;
  onCreate: (name: string, version: number) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [version, setVersion] = useState(health.default_pg_version);
  return (
    <form
      className="rise mt-6 flex flex-wrap items-center gap-2 rounded-2xl border border-line bg-panel p-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (name.trim()) onCreate(name.trim(), version);
      }}
    >
      <input
        autoFocus
        value={name}
        onChange={(e) => setName(e.target.value)}
        placeholder="Database name"
        className={`${input} min-w-0 flex-1`}
      />
      <select value={version} onChange={(e) => setVersion(Number(e.target.value))} className={input}>
        {health.pg_versions.map((v) => (
          <option key={v} value={v}>
            Postgres {v}
          </option>
        ))}
      </select>
      <button type="submit" disabled={busy || !name.trim()} className={primary}>
        {busy ? <span className="shimmer">Creating…</span> : "Create"}
      </button>
      <button type="button" onClick={onCancel} className={btn}>
        Cancel
      </button>
    </form>
  );
}

function Empty({ onCreate }: { onCreate: () => void }) {
  return (
    <section className="relative isolate mt-6 overflow-hidden rounded-2xl border border-line bg-panel">
      <DotField spacing={18} radius={90} className="-z-10 text-fg" />
      <div className="flex flex-col items-start gap-3 p-8">
        <h2 className="text-[15px] font-medium">No databases yet</h2>
        <p className="max-w-md text-[13px] text-muted">
          Each database is real Postgres with a <span className="font-mono">main</span> branch. Computes start on the
          first connection and stop when idle, so an unused database costs only its storage.
        </p>
        <button onClick={onCreate} className={primary}>
          <Plus className="size-3.5" /> New database
        </button>
      </div>
    </section>
  );
}

function UsageStats({ project, usage }: { project: Project; usage: Usage | null }) {
  const running = project.branches.filter((b) => b.endpoint?.state === "running");
  const liveCu = running.reduce((s, b) => s + (b.endpoint?.cu ?? 0), 0);
  const savings =
    usage && usage.always_on_month_usd > 0 ? 1 - usage.projected_month_usd / usage.always_on_month_usd : null;
  const stats = [
    {
      label: "Compute now",
      value: `${liveCu.toFixed(2)} CU`,
      hint: `${running.length} of ${project.branches.length} computes awake`,
    },
    {
      label: "Compute · 14d",
      value: usage ? `${usage.compute.cu_hours.toFixed(2)} CU-h` : "—",
      hint: usage ? formatUsd(usage.compute.usd) : " ",
    },
    {
      label: "Storage",
      value: usage ? formatBytes(usage.storage.current_bytes) : "—",
      hint: usage ? `${formatUsd(usage.pricing.usd_per_gb_month)}/GB-mo on gp3` : " ",
    },
    {
      label: "AWS cost · projected / mo",
      value: usage ? formatUsd(usage.projected_month_usd) : "—",
      hint:
        savings != null ? `${Math.round(savings * 100)}% below always-on (${formatUsd(usage!.always_on_month_usd)})` : " ",
      accent: true,
    },
  ];
  return (
    <div className="mt-6 grid grid-cols-2 gap-px overflow-hidden rounded-2xl border border-line bg-line lg:grid-cols-4">
      {stats.map((s) => (
        <div key={s.label} className="bg-panel p-5">
          <p className="text-[12px] text-subtle">{s.label}</p>
          <p className="mt-2 text-2xl font-semibold tracking-[-0.03em] tabular-nums">{s.value}</p>
          <p className={`mt-1 truncate text-[12px] ${s.accent ? "text-ok" : "text-muted"}`}>{s.hint}</p>
        </div>
      ))}
    </div>
  );
}

function UsageChart({ usage }: { usage: Usage | null }) {
  const max = Math.max(0.0001, ...(usage?.daily.map((d) => d.cu_hours) ?? [0]));
  return (
    <section className="flex min-w-0 flex-col rounded-2xl border border-line bg-panel lg:col-span-2">
      <div className="flex items-baseline justify-between border-b border-line px-5 py-4">
        <h2 className="text-[15px] font-medium">Compute usage</h2>
        <span className="font-mono text-[11px] text-subtle">CU-hours / day</span>
      </div>
      <div className="flex flex-1 items-end gap-1 px-5 pt-6 pb-3" style={{ minHeight: 180 }}>
        {(usage?.daily ?? Array.from({ length: 14 }, (_, i) => ({ day: i, cu_hours: 0 }))).map((d) => (
          <div key={d.day} className="group relative flex h-36 flex-1 items-end">
            <div
              className="w-full rounded-sm bg-fg/15 transition-[height] duration-300 group-hover:bg-accent"
              style={{ height: `${Math.max(2, (d.cu_hours / max) * 100)}%` }}
            />
            <span className="pointer-events-none absolute -top-6 left-1/2 hidden -translate-x-1/2 rounded border border-line bg-panel-2 px-1.5 py-0.5 font-mono text-[10px] whitespace-nowrap group-hover:block">
              {d.cu_hours.toFixed(3)}
            </span>
          </div>
        ))}
      </div>
      <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
        {usage
          ? `${formatUsd(usage.pricing.usd_per_cu_hour)} per CU-hour · Graviton on-demand, 1 CU = 1 vCPU + 4 GiB`
          : " "}
      </p>
    </section>
  );
}

function Branches({
  project,
  selected,
  onSelect,
  busy,
  run,
}: {
  project: Project;
  selected: string | null;
  onSelect: (id: string) => void;
  busy: string | null;
  run: (key: string, fn: () => Promise<unknown>) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [from, setFrom] = useState("main");
  const byId = useMemo(() => new Map(project.branches.map((b) => [b.id, b])), [project.branches]);

  return (
    <section className="min-w-0 rounded-2xl border border-line bg-panel lg:col-span-3">
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h2 className="text-[15px] font-medium">Branches</h2>
        <span className="text-[12px] text-subtle">{project.branches.length}</span>
      </div>
      <ul>
        {project.branches.map((b) => {
          const ep = b.endpoint;
          const parent = b.parent_id ? byId.get(b.parent_id)?.name : null;
          const active = b.id === selected;
          return (
            <li
              key={b.id}
              onClick={() => onSelect(b.id)}
              className={`rise flex cursor-pointer items-center gap-3 border-b border-line px-5 py-3 transition last:border-0 ${
                active ? "bg-panel-2" : "hover:bg-panel-2/60"
              }`}
            >
              <StateDot state={ep?.state} />
              <div className="min-w-0 flex-1">
                <p className="flex items-center gap-1.5 truncate text-[13px]">
                  {b.name}
                  {parent && (
                    <span className="flex items-center gap-1 text-[11px] text-subtle">
                      <GitBranch className="size-3" /> {parent}
                    </span>
                  )}
                </p>
                <p className="font-mono text-[11px] text-subtle">
                  {ep?.id} · {formatBytes(b.storage_bytes)} · {ep?.connections ?? 0} conn
                </p>
              </div>
              {ep && <CuMeter ep={ep} />}
              <span className="w-20 text-right text-[12px] text-subtle">
                {ep?.state === "starting" ? (
                  <span className="shimmer">starting</span>
                ) : ep?.state === "running" ? (
                  "running"
                ) : (
                  "idle"
                )}
              </span>
              {ep && (
                <button
                  title={ep.state === "suspended" ? "Start compute" : "Suspend compute"}
                  disabled={busy !== null || ep.state === "starting"}
                  onClick={(e) => {
                    e.stopPropagation();
                    run(`ep-${ep.id}`, () => (ep.state === "suspended" ? api.start(ep.id) : api.suspend(ep.id)));
                  }}
                  className="rounded-md p-1.5 text-subtle transition hover:bg-panel hover:text-fg disabled:opacity-40"
                >
                  {busy === `ep-${ep.id}` ? (
                    <HorizonMark state="thinking" size={14} interactive={false} />
                  ) : ep.state === "suspended" ? (
                    <Power className="size-3.5" />
                  ) : (
                    <Moon className="size-3.5" />
                  )}
                </button>
              )}
            </li>
          );
        })}
      </ul>
      <form
        className="flex flex-wrap items-center gap-2 border-t border-line px-5 py-3"
        onSubmit={(e) => {
          e.preventDefault();
          if (!name.trim()) return;
          run("branch", async () => {
            await api.createBranch(project.id, name.trim(), from);
            setName("");
          });
        }}
      >
        <GitBranch className="size-3.5 text-subtle" />
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="new-branch"
          className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
        />
        <span className="text-[12px] text-subtle">from</span>
        <select value={from} onChange={(e) => setFrom(e.target.value)} className={`${input} text-[12px]`}>
          {project.branches.map((b) => (
            <option key={b.id} value={b.name}>
              {b.name}
            </option>
          ))}
        </select>
        <button type="submit" disabled={busy !== null || !name.trim()} className={btn}>
          {busy === "branch" ? <span className="shimmer">Branching…</span> : "Branch"}
        </button>
      </form>
    </section>
  );
}

/** Live compute size within the autoscaling range. */
function CuMeter({ ep }: { ep: Endpoint }) {
  const on = ep.state === "running";
  const pct = on ? Math.min(100, (ep.cu / ep.max_cu) * 100) : 0;
  return (
    <div className="hidden w-28 sm:block" title={`${ep.min_cu}–${ep.max_cu} CU`}>
      <div className="flex justify-between font-mono text-[10px] text-subtle">
        <span className={on ? "text-fg" : ""}>{on ? `${ep.cu} CU` : "0 CU"}</span>
        <span>{ep.max_cu}</span>
      </div>
      <div className="mt-1 h-1 overflow-hidden rounded-full bg-line">
        <div className="h-full rounded-full bg-accent transition-[width] duration-500" style={{ width: `${pct}%` }} />
      </div>
    </div>
  );
}

function BranchSettings({
  project,
  branch,
  busy,
  run,
}: {
  project: Project;
  branch: Branch;
  busy: string | null;
  run: (key: string, fn: () => Promise<unknown>) => Promise<void>;
}) {
  const ep = branch.endpoint;
  const { copied, copy } = useCopy();
  const [reveal, setReveal] = useState(false);
  const [min, setMin] = useState(ep?.min_cu ?? 0.25);
  const [max, setMax] = useState(ep?.max_cu ?? 2);
  const [suspend, setSuspend] = useState(ep?.suspend_timeout_s ?? 300);
  const dirty = ep && (min !== ep.min_cu || max !== ep.max_cu || suspend !== ep.suspend_timeout_s);
  const uri = branch.connection_uri ?? "";
  const suspendOptions = SUSPEND_OPTIONS.some((o) => o.value === suspend)
    ? SUSPEND_OPTIONS
    : [...SUSPEND_OPTIONS, { label: `${suspend}s`, value: suspend }];

  return (
    <section className="flex min-w-0 flex-col rounded-2xl border border-line bg-panel lg:col-span-2">
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h2 className="text-[15px] font-medium">
          <span className="font-mono">{branch.name}</span>
        </h2>
        <span className="text-[12px] text-subtle">created {ago(branch.created_at)}</span>
      </div>

      <div className="space-y-2 px-5 py-4">
        <p className="text-[12px] text-subtle">Connection string</p>
        <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
          <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{reveal ? uri : maskUri(uri)}</code>
          <button onClick={() => setReveal((r) => !r)} className="p-1 text-subtle hover:text-fg" title="Reveal">
            {reveal ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
          </button>
          <button onClick={() => copy("uri", uri)} className="p-1 text-subtle hover:text-fg" title="Copy">
            {copied === "uri" ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
          </button>
        </div>
        <p className="text-[11px] text-subtle">
          Works with psql, any driver or ORM. The compute wakes on connect.
        </p>
      </div>

      {ep && (
        <div className="space-y-3 border-t border-line px-5 py-4">
          <p className="text-[12px] text-subtle">Autoscaling</p>
          <div className="grid grid-cols-3 gap-2">
            <label className="space-y-1">
              <span className="block text-[11px] text-subtle">Min CU</span>
              <select value={min} onChange={(e) => setMin(Number(e.target.value))} className={`${input} w-full`}>
                {CU_STEPS.filter((s) => s <= max).map((s) => (
                  <option key={s} value={s}>
                    {s}
                  </option>
                ))}
              </select>
            </label>
            <label className="space-y-1">
              <span className="block text-[11px] text-subtle">Max CU</span>
              <select value={max} onChange={(e) => setMax(Number(e.target.value))} className={`${input} w-full`}>
                {CU_STEPS.filter((s) => s >= min).map((s) => (
                  <option key={s} value={s}>
                    {s}
                  </option>
                ))}
              </select>
            </label>
            <label className="space-y-1">
              <span className="block text-[11px] text-subtle">Suspend after</span>
              <select value={suspend} onChange={(e) => setSuspend(Number(e.target.value))} className={`${input} w-full`}>
                {suspendOptions.map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
              </select>
            </label>
          </div>
          <div className="flex items-center justify-between">
            <span className="text-[11px] text-subtle">
              up to {formatUsd(max * 730 * 0.0408)}/mo at max, {suspend === 0 ? "always on" : "$0 while idle"}
            </span>
            <button
              disabled={!dirty || busy !== null}
              onClick={() =>
                run("settings", () =>
                  api.updateEndpoint(ep.id, { min_cu: min, max_cu: max, suspend_timeout_s: suspend }),
                )
              }
              className={btn}
            >
              Save
            </button>
          </div>
        </div>
      )}

      {branch.parent_id && (
        <div className="mt-auto border-t border-line px-5 py-3">
          <button
            disabled={busy !== null}
            onClick={() => {
              if (confirm(`Delete branch ${branch.name}?`)) run("delete-branch", () => api.deleteBranch(project.id, branch.id));
            }}
            className="inline-flex items-center gap-1.5 text-[12px] text-subtle hover:text-danger"
          >
            <Trash2 className="size-3.5" /> Delete branch
          </button>
        </div>
      )}
    </section>
  );
}

const EXAMPLES = [
  "select now(), version();",
  "select relname, n_live_tup from pg_stat_user_tables order by 2 desc limit 10;",
  "select pg_size_pretty(pg_database_size(current_database()));",
];

function SqlConsole({ branch }: { branch: Branch }) {
  const { setState, play } = useMarkState();
  const [sql, setSql] = useState(EXAMPLES[0]);
  const [running, setRunning] = useState(false);
  const [out, setOut] = useState<{ results: QueryResult[]; took_ms: number } | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const ref = useRef<HTMLTextAreaElement>(null);
  const ep = branch.endpoint;

  const exec = async () => {
    if (!ep || running || !sql.trim()) return;
    setRunning(true);
    setErr(null);
    setState("thinking");
    try {
      setOut(await api.query(ep.id, sql));
      play([["success", 1200]]);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      setOut(null);
      play([["error", 1400]]);
    } finally {
      setRunning(false);
    }
  };

  const last = out?.results.findLast((r) => r.columns.length > 0) ?? null;

  return (
    <section className="flex min-w-0 flex-col rounded-2xl border border-line bg-panel lg:col-span-3">
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h2 className="text-[15px] font-medium">SQL</h2>
        <div className="flex gap-1">
          {EXAMPLES.slice(1).map((e, i) => (
            <button key={e} onClick={() => setSql(e)} className="rounded px-1.5 font-mono text-[11px] text-subtle hover:text-fg">
              {["tables", "size"][i]}
            </button>
          ))}
        </div>
      </div>
      <div className="relative">
        <textarea
          ref={ref}
          value={sql}
          spellCheck={false}
          onChange={(e) => setSql(e.target.value)}
          onKeyDown={(e) => {
            if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
              e.preventDefault();
              exec();
            }
          }}
          rows={4}
          className="block w-full resize-y bg-transparent px-5 py-4 font-mono text-[12px] leading-relaxed outline-none"
        />
        <button onClick={exec} disabled={running || !ep} className={`${primary} absolute right-4 bottom-3`}>
          {running ? (
            <>
              <HorizonMark state="thinking" size={14} interactive={false} /> Running
            </>
          ) : (
            <>
              <Play className="size-3" /> Run <kbd className="font-mono text-[10px] opacity-60">⌘↵</kbd>
            </>
          )}
        </button>
      </div>
      <div className="min-h-24 border-t border-line">
        {running && ep?.state === "suspended" && (
          <p className="px-5 py-3 text-[12px] text-subtle">
            <span className="shimmer">Waking compute from zero…</span>
          </p>
        )}
        {err && <p className="rise px-5 py-3 font-mono text-[12px] text-danger">{err}</p>}
        {out && (
          <div className="rise">
            {last ? (
              <div className="max-h-72 overflow-auto">
                <table className="w-full text-left font-mono text-[11px]">
                  <thead className="sticky top-0 bg-panel">
                    <tr>
                      {last.columns.map((c, i) => (
                        <th key={i} className="border-b border-line px-5 py-2 font-medium text-muted">
                          {c}
                        </th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {last.rows.slice(0, 200).map((r, i) => (
                      <tr key={i} className="border-b border-line last:border-0">
                        {r.map((v, j) => (
                          <td key={j} className="max-w-xs truncate px-5 py-1.5">
                            {v ?? <span className="text-subtle">null</span>}
                          </td>
                        ))}
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            ) : null}
            <p className="px-5 py-2 text-[11px] text-subtle">
              {out.results.map((r) => `${r.affected} row${r.affected === 1 ? "" : "s"}`).join(" · ") || "ok"} ·{" "}
              {out.took_ms} ms
            </p>
          </div>
        )}
      </div>
    </section>
  );
}
