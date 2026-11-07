"use client";

import { ArrowUpRight, CircleAlert, FolderGit2, FolderPlus, GitBranch, GitFork, GitPullRequest, LogIn, MoreHorizontal, Plus, RefreshCw, Settings2, SquareTerminal } from "lucide-react";
import { memo, useCallback, useMemo, useState, type ReactNode } from "react";
import { useToolsArmed } from "./card-tools";
import { coming, isRunning, machineState, reachable, type IdentityColor, type Machine, type Node, type Project, type ProviderInfo, type Request, type Run, type Thread } from "@/lib/agents-api";
import { newestRuns, projectStats, type ProjectStats } from "@/lib/agents-graph";
import { providersFor, type AgentsData } from "@/lib/agents-store";
import { setAgentsView } from "@/lib/agents-surface";
import { btn, primary } from "../ui";
import { AutomationChip, useThreadOrigin } from "./automations/chip";
import { ChangeChip, changeStatusLabel, WorkChip } from "./change";
import { ContextMeter } from "./context-meter";
import { Menu } from "./menu";
import { openProjectDialog } from "@/lib/agents-project-dialogs";
import { PullLine } from "./project-settings";
import { PendingRequest } from "./pending-request";
import { SandboxPanel } from "./sandbox";
import { ago, DeliveredText, loginCommand, modelName, providerName, shortPath, Stat, StatusDot, threadStatusLabel, usageLine } from "./shared";
import { orchestration, OrchestratorChip, StatusPill, threadDone, threadPill } from "./thread-status";
import { useCanvasFolds } from "./canvas-folds";
import { MachineIcon, ProjectIcon, tint as tintStyle } from "./identity";
import { machineColor, projectColor } from "@/lib/agents-identity";
import { useAgents } from "./use-agents";
import { MoreProjects } from "./machine-projects";
import { useProjectActions } from "./project-actions";
import { openMachineSettings, ProviderSetup } from "./provider-setup";
import { canRunAgents, notReadyReason, readiness } from "@/lib/agents-setup";
import { cn } from "./ui/cn";

/**
 * The frame every canvas card shares: flat panel, hairline border, the whole card a drag handle.
 * `tint` is the colour of the project or machine it belongs to (docs/code/agents-design.md,
 * "Identity"): its children use it through `--tint`; `edge` paints the card's left edge in it, the
 * whole height and round with the corners. The tone keeps the rest of the border, so state always
 * reads over identity; a busy border breathes (globals.css).
 */
export function Card({
  width,
  tone,
  tint,
  edge = false,
  children,
  className = "",
  onClick,
  label,
}: {
  width: number;
  tone?: "busy" | "error" | "muted" | "done";
  tint?: IdentityColor;
  edge?: boolean;
  children: ReactNode;
  className?: string;
  onClick?: () => void;
  label?: string;
}) {
  const border = tone === "busy" ? "border-accent/40" : tone === "error" ? "border-danger/45" : tone === "done" ? "border-ok/40" : "border-line-2";
  return (
    <div
      data-drag
      data-tone={tone}
      role={onClick ? "button" : undefined}
      tabIndex={onClick ? 0 : undefined}
      aria-label={label}
      onClick={onClick}
      onKeyDown={
        onClick
          ? (e) => {
              if ((e.key === "Enter" || e.key === " ") && e.target === e.currentTarget) {
                e.preventDefault();
                onClick();
              }
            }
          : undefined
      }
      data-tint={tint}
      style={{ width, ...(tint ? tintStyle(tint) : null) }}
      className={`group/card relative rounded-xl border bg-panel text-[13px] shadow-card inset-shadow-highlight transition-[border-color,box-shadow,opacity] duration-150 ease-out ${border} ${tone === "muted" ? "opacity-60" : ""} ${
        onClick ? "cursor-pointer hover:shadow-card-hover focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent/70" : "cursor-default"
      } ${onClick && !tone ? "hover:border-line-3" : ""} ${className}`}
    >
      {tint && edge && (
        <span aria-hidden data-edge className="pointer-events-none absolute -inset-px overflow-hidden rounded-[inherit]">
          <span className="absolute inset-y-0 left-0 w-[3px] bg-(--tint)" />
        </span>
      )}
      {children}
    </div>
  );
}

/**
 * The header of a machine's or project's card: its colour at 6% behind it, flat (no gradient),
 * fading nothing; the body below stays the plain panel.
 */
function TintedHeader({ children }: { children: ReactNode }) {
  return <div className="flex items-center gap-2.5 rounded-t-[11px] border-b border-line bg-(--tint)/[0.06] px-3 py-2.5">{children}</div>;
}

/**
 * A card's tools: a small floating toolbar over its top-right corner while the pointer is on it
 * (always on touch screens, and while one has keyboard focus). It takes no room in the card, so
 * names never truncate for buttons that aren't showing. Its buttons mount once they're first
 * needed (`useToolsArmed`).
 */
function HoverActions({ children, className = "" }: { children: ReactNode; className?: string }) {
  const { armed, ref } = useToolsArmed();
  return (
    <div
      ref={ref}
      className={`pointer-events-none absolute -top-3.5 right-2.5 z-10 flex translate-y-1 items-center gap-0.5 rounded-lg border border-line-2 p-0.5 text-muted opacity-0 shadow-pop inset-shadow-highlight glass transition-[opacity,translate] duration-150 ease-out starting:translate-y-1! starting:opacity-0! motion-reduce:transition-none group-hover/card:pointer-events-auto group-hover/card:translate-y-0 group-hover/card:opacity-100 focus-within:pointer-events-auto focus-within:translate-y-0 focus-within:opacity-100 ${armed ? "" : "hidden"} ${className}`}
      data-nodrag
      data-touch-tools
    >
      {armed && children}
    </div>
  );
}

/** A card's button; `launch` also makes it a handle to drag onto the canvas (`p:…`, `m:…`, `t:…`). */
function Tool({ label, onClick, disabled, launch, children }: { label: string; onClick: () => void; disabled?: boolean; launch?: string; children: ReactNode }) {
  return (
    <button
      onClick={(e) => {
        e.stopPropagation();
        onClick();
      }}
      disabled={disabled}
      title={label}
      aria-label={label}
      data-launch={disabled ? undefined : launch}
      className={`grid size-6 place-items-center rounded-md transition-colors duration-150 hover:bg-panel-3 hover:text-fg disabled:opacity-30 [&>svg]:size-3.5 [&>svg]:stroke-[1.75] ${launch && !disabled ? "cursor-grab active:cursor-grabbing" : ""}`}
    >
      {children}
    </button>
  );
}

function useRun() {
  const { client } = useAgents();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const run = async (key: string, fn: () => Promise<unknown>) => {
    setBusy(key);
    setError(null);
    try {
      return await fn();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };
  return { client, busy, error, run, setError };
}

// ── machine ──

const OS_NAMES: Record<string, string> = { macos: "macOS", linux: "Linux", windows: "Windows" };

export function MachineNode({ machine }: { machine: Machine }) {
  const { client, busy, error, run } = useRun();
  const { focus, startNew } = useAgents();
  // Adding a project is one dialog everywhere (project-new.tsx), never a form growing the card.
  const add = () => openProjectDialog({ kind: "add", machine: machine.id });
  const info = machine.info;
  const sandbox = machine.sandbox;
  const projects = [...client.data.projects.values()].filter((r) => r.machine === machine.id).length;
  const os = info ? `${OS_NAMES[info.os] ?? info.os} · ${info.arch} · roll ${info.version}` : sandbox ? null : "Never connected";
  // A sandbox that isn't connected isn't "offline": it's asleep, stopped, or on its way up.
  const state = machineState(machine);
  // Shown too while the daemon is still probing them (it says hello without waiting).
  const listed = info?.providers.filter((p) => p.installed || p.checking) ?? [];
  // Two chips, the rest one "+4" (to the Providers settings: three names truncated on a card); a login to do or a probe going always shows.
  const chips = listed.length > 4 ? listed.filter((p, i) => i < 2 || p.auth === "missing" || p.checking) : listed;
  const more = listed.filter((p) => !chips.includes(p));
  // Can't run an agent yet (no provider installed, or none logged in): the next step comes first.
  const ready = readiness(machine);
  const setup = machine.online && (ready.state === "none" || ready.state === "login");
  const login = (p: ProviderInfo) =>
    run(`login:${p.id}`, async () => {
      const r = await client.run<{ type: "created"; id: string }>(machine.id, { type: "terminal.open", command: loginCommand(p, !!sandbox), cols: 100, rows: 26 }, "created");
      focus(`x:${r.id}`);
    });
  const openTerminal = () =>
    run("term", async () => {
      const r = await client.run<{ type: "created"; id: string }>(machine.id, { type: "terminal.open", cols: 100, rows: 26 }, "created");
      focus(`x:${r.id}`);
    });
  return (
    <Card width={272} tone={state === "offline" ? "muted" : undefined} tint={machineColor(machine)} label={`Machine ${machine.name}`}>
      <FarLabel title={machine.name} />
      <TintedHeader>
        <MachineIcon machine={machine} tile className="size-6" />
        <span className="min-w-0 flex-1 truncate text-[13.5px] font-semibold tracking-[-0.015em]">{machine.name}</span>
        <span className="flex shrink-0 items-center gap-1.5 text-[11px] text-subtle">
          <StatusDot status={machine.online ? "online" : "offline"} />
          <span className={coming(state) ? "shimmer" : ""}>{state}</span>
        </span>
        <HoverActions>
          {startNew && reachable(machine) && (
            <Tool label="New agent on this machine (or drag onto the canvas)" launch={`m:${machine.id}`} onClick={() => startNew(undefined, { machine: machine.id })}>
              <Plus />
            </Tool>
          )}
          {machine.online && (
            <>
              <Tool label="Add a project" onClick={add}>
                <FolderPlus />
              </Tool>
              <Tool label="Open a terminal" disabled={busy !== null} onClick={openTerminal}>
                <SquareTerminal />
              </Tool>
              <Tool label="Check providers again (after a login)" disabled={busy !== null} onClick={() => run("refresh", () => client.run(machine.id, { type: "provider.refresh" }))}>
                <RefreshCw className={busy === "refresh" ? "animate-spin motion-reduce:animate-none" : ""} />
              </Tool>
            </>
          )}
          <Tool label="Machine settings" onClick={() => setAgentsView({ kind: "settings", target: `m:${machine.id}` })}>
            <Settings2 />
          </Tool>
        </HoverActions>
      </TintedHeader>
      <div className="space-y-2.5 px-3 pt-2.5 pb-3">
        {os && (
          <p data-detail className="truncate font-mono text-[11px] text-subtle">
            {os}
          </p>
        )}
        {!machine.online && !sandbox && (
          <p className="text-[11.5px] text-subtle">
            {machine.last_seen ? `Last seen ${ago(machine.last_seen)}.` : "Not connected yet."} Start it with <code className="font-mono text-muted">roll agent up</code>.
          </p>
        )}
        {sandbox && <SandboxPanel machine={machine} />}
        {setup ? (
          // No provider, or none logged in: an agent can't run here yet; say what to do next.
          <ProviderSetup machine={machine} />
        ) : (
          listed.length > 0 && (
            // Wrapping, the "+3" went alone onto a line of its own: one line, names give way first.
            <ul className={cn("flex gap-1", more.length > 0 ? "flex-nowrap" : "flex-wrap")} aria-label="Providers">
              {chips.map((p) => (
                <li key={p.id} className="flex min-w-0">
                  {p.checking ? (
                    <span className="inline-flex items-center gap-1.5 rounded-md border border-line px-1.5 py-0.5 text-[11px] text-subtle" title={`Checking ${p.name}…`}>
                      <span className="size-1 rounded-full bg-subtle" aria-hidden />
                      <span className="shimmer">{p.name}</span>
                      <span className="sr-only">Checking…</span>
                    </span>
                  ) : p.auth === "missing" ? (
                    <button
                      data-nodrag
                      disabled={!machine.online || busy !== null}
                      onClick={() => login(p)}
                      className="inline-flex items-center gap-1 rounded-md border border-dashed border-line-2 px-1.5 py-0.5 text-[11px] text-muted hover:border-subtle hover:text-fg disabled:opacity-40"
                      title={p.detail ?? `Log in to ${p.name}`}
                    >
                      <LogIn className="size-3" /> {p.name}
                    </button>
                  ) : (
                    <span
                      className="inline-flex min-w-0 items-center gap-1.5 rounded-md border border-line px-1.5 py-0.5 text-[11px] text-muted"
                      title={[p.name, p.version, p.auth === "ready" ? "logged in" : p.detail].filter(Boolean).join(" · ") || undefined}
                    >
                      <span className={`size-1 shrink-0 rounded-full ${p.auth === "ready" ? "bg-ok" : "bg-subtle"}`} aria-label={p.auth === "ready" ? "Logged in" : "Unknown"} />
                      <span className="truncate">{p.name}</span>
                    </span>
                  )}
                </li>
              ))}
              {more.length > 0 && (
                <li className="shrink-0">
                  <button
                    type="button"
                    data-nodrag
                    onClick={() => openMachineSettings(machine.id, "Providers")}
                    title={more.map((p) => p.name).join(", ")}
                    aria-label={`${more.length} more providers: ${more.map((p) => p.name).join(", ")}`}
                    className="inline-flex items-center rounded-md border border-line px-1.5 py-0.5 font-mono text-[11px] text-subtle transition-colors hover:border-line-2 hover:text-fg"
                  >
                    +{more.length}
                  </button>
                </li>
              )}
            </ul>
          )
        )}
        <MoreProjects machine={machine} onAdd={add} />
        {machine.online && projects === 0 && (
          <button data-nodrag onClick={add} className={`${setup ? btn : primary} w-full justify-center`}>
            <FolderPlus className="size-3.5" /> Add a project
          </button>
        )}
        {error && <p className="text-[12px] text-danger">{error}</p>}
      </div>
    </Card>
  );
}

// ── project ──

export function ProjectNode({ project, machine }: { project: Project; machine: Machine | undefined }) {
  const { client, busy, error, run } = useRun();
  const { startNew, demo } = useAgents();
  // An asleep sandbox counts: what's sent to it wakes it.
  const online = reachable(machine);
  // Online is not enough: an agent needs a provider installed and logged in there.
  const runnable = canRunAgents(machine);
  const stats = projectStats(client.data, project.id);
  // The same actions everywhere a project shows (its settled and archived conversations among them).
  const actions = useProjectActions(project, { remove: () => void run("remove", () => client.run(machine!.id, { type: "project.remove", project: project.id })) });
  // A click goes into the project's space (its threads by status), as F does from the keyboard;
  // Enter keeps opening its settings. Its own buttons, links and menus keep their clicks.
  const { drill, drilled } = useCanvasFolds();
  const enter = drill && drilled !== project.id && stats.threads > 0 ? () => drill(project.id) : undefined;
  return (
    <div
      onClick={enter && ((e) => !(e.target as HTMLElement).closest("button, a, input, [role=menu], [role=menuitem], [data-nodrag]") && enter())}
      title={enter ? `Open ${project.name}: its threads by status (F)` : undefined}
      className={enter ? "cursor-pointer" : undefined}
    >
    <Card width={256} tint={projectColor(project)} label={`Project ${project.name}`}>
      <FarLabel title={project.name} />
      <TintedHeader>
        <ProjectIcon project={project} className="size-6" />
        <span className="min-w-0 flex-1 truncate text-[13.5px] font-semibold tracking-[-0.015em]">{project.name}</span>
        <HoverActions>
          {startNew && (
            <Tool label={runnable ? "New agent here (or drag onto the canvas)" : (notReadyReason(machine) ?? "New agent here")} launch={`p:${project.id}`} disabled={!runnable} onClick={() => startNew(project.id)}>
              <Plus />
            </Tool>
          )}
          <Tool
            label="Open a terminal here"
            disabled={!online || busy !== null}
            onClick={() => run("term", () => client.run(machine!.id, { type: "terminal.open", project: project.id, cols: 100, rows: 26 }))}
          >
            <SquareTerminal />
          </Tool>
          <Menu label="Project" className="[&>button]:size-6" trigger={<MoreHorizontal className="size-3.5" />} items={actions} />
        </HoverActions>
      </TintedHeader>
      <div className="flex items-center gap-2 px-3 pt-2.5 pb-3 font-mono text-[11px] text-subtle">
        {project.git?.branch && (
          <span className="flex shrink-0 items-center gap-1" title={project.git.dirty ? "Uncommitted changes" : undefined}>
            <GitBranch className="size-3" />
            {project.git.branch}
            {project.git.dirty && <span className="size-1 rounded-full bg-muted" aria-label="dirty" />}
          </span>
        )}
        <span className="min-w-0 flex-1 truncate" title={project.path}>
          {project.repo ? `${project.repo.owner}/${project.repo.repo}` : shortPath(project.path, machine?.info?.home)}
        </span>
        {project.repo && !demo && (
          <a data-nodrag href={`/${project.repo.owner}/${project.repo.repo}`} className="shrink-0 hover:text-fg" aria-label="Open the repo" title="Open the repo">
            <ArrowUpRight className="size-3" />
          </a>
        )}
      </div>
      {stats.threads > 0 && <StatsRows stats={stats} providers={providersFor(client.data, machine?.id)} />}
      {project.settings.auto_pull && project.pull && <PullLine pull={project.pull} className="mx-3 mb-3 -mt-1" />}
      {!stats.threads && startNew && online && (
        <div className="px-3 pb-3">
          {runnable ? (
            <button data-nodrag onClick={() => startNew(project.id)} className={`${primary} w-full justify-center`}>
              <Plus className="size-3.5" /> Start an agent
            </button>
          ) : (
            // The machine can't run one yet: its card has the steps; this says where.
            <p className="text-[11.5px] text-subtle">
              {notReadyReason(machine)}.{" "}
              <button type="button" data-nodrag onClick={() => openMachineSettings(machine!.id, "Providers")} className="text-muted underline-offset-2 hover:text-fg hover:underline">
                Set one up
              </button>
            </p>
          )}
        </div>
      )}
      {error && <p className="px-3 pb-3 text-[12px] text-danger">{error}</p>}
    </Card>
    </div>
  );
}

/** Its work, the PRs it made and the agents in it. */
function StatsRows({ stats, providers }: { stats: ProjectStats; providers: readonly ProviderInfo[] }) {
  const landed = stats.changes.filter((c) => c.status === "landed").length;
  const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;
  return (
    <div data-detail className="mx-3 mb-3 space-y-1 border-t border-line pt-2.5 text-[11px] text-subtle">
      <StatusStrip stats={stats} />
      <p className="flex items-center gap-1.5">
        <span className="min-w-0 truncate">
          {plural(stats.threads, "thread", "threads")}
          {stats.waiting > 0 && <span className="text-accent"> · {stats.waiting} need you</span>}
          {stats.running > 0 && <span className="text-fg"> · {stats.running} running</span>}
          {stats.failed > 0 && <span className="text-danger"> · {stats.failed} failed</span>}
          {stats.done > 0 && <span className="text-ok"> · {stats.done} done</span>}
        </span>
        {stats.changes.length > 0 && (
          <span className="ml-auto flex shrink-0 items-center gap-1" title={stats.changes.map((c) => `#${c.number} ${changeStatusLabel[c.status]}`).join(" · ")}>
            <GitPullRequest className="size-3" />
            {plural(stats.changes.length, "PR", "PRs")}
            {landed > 0 && <span className="text-ok">· {landed} landed</span>}
          </span>
        )}
      </p>
      <p className="truncate" aria-label="Agents in this project">
        {stats.agents.map((a, i) => (
          <span key={a.provider} title={`${plural(a.threads, "thread", "threads")}: ${a.models.map((m) => modelName(providers, a.provider, m) ?? "default model").join(", ")}`}>
            {i > 0 && " · "}
            <span className="text-muted">{providerName(providers, a.provider)}</span>
            {a.threads > 1 && <span className="font-mono tabular-nums"> ×{a.threads}</span>}
          </span>
        ))}
        {stats.cost_usd != null && <span className="font-mono tabular-nums"> · ${stats.cost_usd.toFixed(2)}</span>}
      </p>
    </div>
  );
}

/**
 * Where a project's threads stand at a glance: one hairline bar split in proportion, needs you and
 * running in the accent, failed in red, done in green, the quiet rest in the line colour.
 */
function StatusStrip({ stats }: { stats: ProjectStats }) {
  if (stats.threads < 2) return null;
  const quiet = Math.max(0, stats.threads - stats.waiting - stats.running - stats.failed - stats.done);
  const parts: [number, string][] = [
    [stats.waiting, "bg-accent"],
    [stats.running, "bg-accent/50"],
    [stats.failed, "bg-danger"],
    [stats.done, "bg-ok/70"],
    [quiet, "bg-fg/12"],
  ];
  return (
    <div aria-hidden className="mb-1.5 flex h-1 gap-px overflow-hidden rounded-full">
      {parts.map(([n, tone], i) => n > 0 && <span key={i} className={tone} style={{ flexGrow: n, flexBasis: 0, minWidth: 3 }} />)}
    </div>
  );
}

// ── thread ──

/**
 * A thread as T3 Code's thread rows show it, as a card: the project (its monogram and name) and the
 * machine it runs on, the status pill, the title and what it's doing, the PR and work item it's
 * attached to, its worktree and running terminals, and what it cost. A finished run you haven't
 * opened yet gives it T3's completed tone: a green border.
 */
export function ThreadNode({ thread, machine }: { thread: Thread; machine: Machine | undefined }) {
  const facts = useThreadFacts(useAgents().client.data);
  return <ThreadCard thread={thread} machine={machine} {...facts(thread)} />;
}

const NO_REQUESTS: readonly Request[] = [];
const NO_PROVIDERS: readonly ProviderInfo[] = [];

/** What a thread's card shows besides the thread and its machine (`useThreadFacts`). */
export interface ThreadFacts {
  project: Project | undefined;
  /** Its running run, else its newest. */
  lastRun: Run | undefined;
  /** Its requests waiting for an answer. */
  pending: readonly Request[];
  /** Its terminals still running. */
  terminals: number;
  /** An orchestrator's agents (`orchestration`). */
  orchestrated: ReturnType<typeof orchestration> | null;
}

/**
 * Each thread card's facts, indexed once per change of what they read: a card used to scan every
 * run, request, terminal and thread itself (200 cards: 200 scans each render). Unchanged facts keep
 * their identity, so a memoised `ThreadCard` renders again only when its own thread's changed.
 */
export function useThreadFacts(data: AgentsData): (thread: Thread) => ThreadFacts {
  const newest = useMemo(() => newestRuns(data), [data.runs]); // eslint-disable-line react-hooks/exhaustive-deps
  const pending = useMemo(() => {
    const out = new Map<string, Request[]>();
    for (const r of data.requests.values()) if (r.value.status === "pending") out.set(r.value.thread, [...(out.get(r.value.thread) ?? []), r.value]);
    return out;
  }, [data.requests]);
  const terminals = useMemo(() => {
    const out = new Map<string, number>();
    for (const r of data.terminals.values()) if (r.value.thread && r.value.status !== "exited") out.set(r.value.thread, (out.get(r.value.thread) ?? 0) + 1);
    return out;
  }, [data.terminals]);
  const orchestrated = useMemo(() => {
    const all = [...data.threads.values()].map((r) => r.value);
    return new Map(all.filter((t) => t.orchestrator).map((t) => [t.id, orchestration(t, all)]));
  }, [data.threads]);
  const { runs, projects } = data;
  return useCallback(
    (t: Thread) => ({
      project: projects.get(t.project)?.value,
      lastRun: t.active_run ? runs.get(t.active_run)?.value : newest.get(t.id),
      pending: pending.get(t.id) ?? NO_REQUESTS,
      terminals: terminals.get(t.id) ?? 0,
      orchestrated: orchestrated.get(t.id) ?? null,
    }),
    [projects, runs, newest, pending, terminals, orchestrated],
  );
}

/** A thread's card from its facts alone, so it renders again only when they change. */
export const ThreadCard = memo(function ThreadCard({ thread, machine, project, lastRun, pending, terminals, orchestrated }: { thread: Thread; machine: Machine | undefined } & ThreadFacts) {
  const { client, openThread, startNew, readOnly } = useAgents();
  const providers = machine?.info?.providers ?? NO_PROVIDERS;
  const { run: runFork, error: forkError } = useRun();
  // A fork from the card (T3's fork from the latest point): click, or drag the handle onto the canvas.
  const forkable = !!startNew && !readOnly && thread.archived_at === null && reachable(machine) && providers.find((p) => p.id === thread.provider)?.capabilities.fork !== false;
  const fork = () =>
    runFork("fork", async () => {
      const r = await client.run<{ type: "created"; id: string }>(machine!.id, { type: "thread.fork", thread: thread.id }, "created");
      openThread(r.id);
    });
  const running = isRunning(thread.status);
  // The card doesn't load runs: the thread record says whether its newest run left a plan (T3's sidebar "Plan ready").
  const pill = threadPill(thread, pending, lastRun, thread.plan_ready && thread.interaction_mode === "plan");
  const done = threadDone(thread, lastRun);
  const usage = usageLine(thread.usage);
  const model = modelName(providers, thread.provider, thread.model);
  // Started by an automation: its tag, opening the run.
  const origin = useThreadOrigin(thread.id);
  // Idle with nothing to say but the last prompt: no line when that's the title again.
  const quiet = !thread.error && !running && thread.status !== "waiting";
  const tone = thread.status === "error" ? "error" : running ? "busy" : thread.archived_at ? "muted" : done ? "done" : undefined;
  return (
    <Card width={296} tone={tone} tint={project ? projectColor(project) : undefined} edge onClick={() => openThread(thread.id)} label={`Thread ${thread.title}`}>
      <FarLabel title={thread.title} tone={tone} />
      <div className="px-3.5 pt-3 pb-3">
        <div className="flex min-w-0 items-center gap-1.5 text-[11px] text-subtle">
          {project && <ProjectIcon project={project} className="size-3.5" />}
          {project && <span className="min-w-0 truncate text-muted">{project.name}</span>}
          {machine && (
            <span className="flex min-w-0 shrink items-center gap-1" title={`On ${machine.name}`} aria-label={`On ${machine.name}`}>
              <MachineIcon machine={machine} className="size-3" />
              <span className="truncate">{machine.name}</span>
            </span>
          )}
          <span className="ml-auto flex shrink-0 items-center gap-1.5">{pill ? <StatusPill pill={pill} /> : <span className="tabular-nums" title="Last activity">{ago(lastActive(thread, lastRun))}</span>}</span>
        </div>
        <div className="mt-1.5 flex items-start gap-2">
          <p className="line-clamp-2 min-w-0 flex-1 text-[13.5px] leading-snug font-medium tracking-[-0.01em]">{thread.title}</p>
          {forkable && (
            <HoverActions>
              <Tool label="Fork (or drag onto the canvas)" launch={`t:${thread.id}`} onClick={fork}>
                <GitFork />
              </Tool>
            </HoverActions>
          )}
          {thread.pending_requests > 0 && (
            <span className="mt-px shrink-0 rounded-full bg-accent px-1.5 py-px font-mono text-[10.5px] font-semibold text-white tabular-nums" title={`${thread.pending_requests} waiting for you`}>
              {thread.pending_requests}
            </span>
          )}
        </div>
        {!(quiet && lastRun?.prompt.trim() === thread.title.trim()) && (
        <p className={`mt-1 truncate text-[12px] ${running || pending.length ? "" : "text-muted"}`}>
          {thread.error ? (
            <span className="flex items-center gap-1 text-danger">
              <CircleAlert className="size-3 shrink-0" /> <span className="truncate">{thread.error}</span>
            </span>
          ) : running ? (
            <span className="shimmer">{thread.activity ?? threadStatusLabel[thread.status]}</span>
          ) : thread.status === "waiting" ? (
            (thread.activity ?? threadStatusLabel.waiting)
          ) : lastRun?.delivers?.length ? (
            <DeliveredText run={lastRun} />
          ) : lastRun ? (
            lastRun.prompt
          ) : (
            <span className="text-subtle">No runs yet</span>
          )}
        </p>
        )}
        {(thread.orchestrator || thread.change || thread.work || thread.worktree || terminals > 0 || origin) && (
          <div data-detail className="mt-2 flex min-w-0 items-center gap-2 text-[11px] text-subtle">
            {origin && <AutomationChip origin={origin} />}
            {orchestrated && <OrchestratorChip {...orchestrated} onOpen={openThread} />}
            {thread.change && <ChangeChip change={thread.change} />}
            {thread.work && <WorkChip work={thread.work} className="min-w-0 flex-1" />}
            {thread.worktree && (
              <span className="flex min-w-0 items-center gap-1 font-mono" role="img" aria-label={`Worktree ${thread.worktree.branch}`} title={`Worktree: ${thread.worktree.path} (${thread.worktree.branch})`}>
                <FolderGit2 className="size-3 shrink-0" />
                {!thread.work && <span className="truncate">{thread.worktree.branch}</span>}
              </span>
            )}
            {terminals > 0 && (
              <span className="flex shrink-0 items-center gap-0.5" role="img" aria-label={`${terminals} terminal${terminals === 1 ? "" : "s"} running`} title="Terminal process running">
                <SquareTerminal className="size-3" />
                {terminals > 1 && <span className="font-mono tabular-nums">{terminals}</span>}
              </span>
            )}
          </div>
        )}
        <p data-detail className="mt-2 flex items-center gap-1.5 border-t border-line pt-2 text-[11px] text-subtle">
          {thread.lineage && <GitFork className="size-3 shrink-0" aria-label={thread.lineage.relation === "fork" ? "Fork" : "Delegated"} />}
          <span className="min-w-0 truncate">
            {providerName(providers, thread.provider)}
            {model && ` · ${model}`}
            {thread.interaction_mode === "plan" && " · plan"}
            {thread.queued.length > 0 && ` · ${thread.queued.length} queued`}
          </span>
          <span className="ml-auto flex shrink-0 items-center gap-1.5 font-mono text-[10.5px]">
            {usage}
            {lastRun?.stat && lastRun.status !== "rolled_back" && (lastRun.stat.additions > 0 || lastRun.stat.deletions > 0) && (
              <span title={`Run ${lastRun.ordinal}'s changes`}>
                <Stat stat={lastRun.stat} />
              </span>
            )}
            {thread.context && <ContextMeter context={thread.context} size={12} label />}
          </span>
        </p>
        {forkError && (
          <p role="alert" className="mt-1.5 text-[11.5px] text-danger">
            Couldn&apos;t fork: {forkError}
          </p>
        )}
      </div>
    </Card>
  );
});

/**
 * When the thread last did something: made, its newest run asked for or ended. Not `updated_at`,
 * which a settle, an undo or a rename moves too (a settle undone said "just now").
 */
function lastActive(thread: Thread, run: Run | undefined): number {
  return run ? Math.max(thread.created_at, run.requested_at, run.completed_at ?? 0) : thread.created_at;
}


/**
 * Semantic zoom: far out (`[data-far]` on the canvas) a card shows its name big, over its details:
 * at 40 % a 13.5 px title is 5 px on screen, this one 11 px; further out (`data-far="2"`) bigger
 * still. The card keeps its size, so the layout doesn't move as the zoom crosses a step.
 */
export function FarLabel({ title, tone }: { title: string; tone?: string }) {
  const dot = tone === "busy" ? "bg-accent" : tone === "error" ? "bg-danger" : tone === "done" ? "bg-ok" : "bg-subtle/60";
  return (
    <div aria-hidden className="absolute inset-0 z-[5] hidden items-center gap-3 rounded-[inherit] bg-panel px-5 [[data-far]_&]:flex">
      <span className={`size-4 shrink-0 rounded-full ${dot} [[data-far='2']_&]:size-6`} />
      <span className="line-clamp-2 min-w-0 text-[28px] leading-[1.15] font-semibold tracking-[-0.02em] [[data-far='2']_&]:text-[56px] [[data-far='2']_&]:leading-[1.05]">{title}</span>
    </div>
  );
}

// ── provider subagent ──

export function SubagentNode({ node }: { node: Node }) {
  const { openThread, client } = useAgents();
  if (node.body.type !== "subagent") return null;
  const b = node.body;
  const running = node.status === "running";
  const project = client.data.projects.get(client.data.threads.get(node.thread)?.value.project ?? "")?.value;
  return (
    <Card width={240} tone={running ? "busy" : undefined} tint={project ? projectColor(project) : undefined} edge onClick={() => openThread(node.thread)} label={`Subagent ${b.agent}`}>
      <div className="px-3 py-2.5">
        <p className="flex items-center gap-1.5 text-[12px] font-medium">
          <StatusDot status={running ? "running" : node.status === "failed" ? "error" : "idle"} />
          {b.agent}
          <span className="ml-auto text-[10.5px] font-normal text-subtle">{running ? <span className="shimmer">working</span> : node.status}</span>
        </p>
        <p className="mt-1 line-clamp-2 pl-3 text-[11.5px] text-muted">{b.result ?? b.prompt}</p>
      </div>
    </Card>
  );
}

// ── request beside its thread ──

/**
 * A request beside its thread, answered right on the card: approvals and plans with their buttons,
 * questions one at a time with their options (T3's user-input panel), and the last steps that led
 * to it above (`PendingRequest`). No need to open the conversation.
 */
export function RequestNode({ request }: { request: Request }) {
  const { client, openThread, readOnly } = useAgents();
  const machine = client.data.threads.get(request.thread)?.machine;
  return (
    <Card width={352} label="Waiting for you">
      <div className="px-3.5 py-3" data-nodrag>
        <PendingRequest
          request={request}
          data={client.data}
          onOpen={() => openThread(request.thread)}
          onRespond={readOnly ? undefined : (response) => client.run(machine!, { type: "request.respond", request: request.id, response })}
        />
      </div>
    </Card>
  );
}
