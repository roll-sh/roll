"use client";

import { ArrowUpRight, CircleAlert, Folder, FolderGit2, FolderPlus, GitBranch, GitFork, GitPullRequest, LogIn, MoreHorizontal, Plus, RefreshCw, Settings2, SquareTerminal, Trash2 } from "lucide-react";
import { useState, type ReactNode } from "react";
import { coming, isRunning, machineState, reachable, type Machine, type Node, type Project, type ProviderInfo, type Request, type Run, type Thread } from "@/lib/agents-api";
import { projectStats, type ProjectStats } from "@/lib/agents-graph";
import { providersFor } from "@/lib/agents-store";
import { setAgentsView } from "@/lib/agents-surface";
import { primary } from "../ui";
import { ChangeChip, changeStatusLabel, WorkChip } from "./change";
import { ContextMeter } from "./context-meter";
import { Menu } from "./menu";
import { NewProject } from "./project-new";
import { PullLine } from "./project-settings";
import { RequestCard } from "./request-card";
import { SandboxPanel } from "./sandbox";
import { ago, loginCommand, MachineIcon, modelName, providerName, shortPath, Stat, StatusDot, threadStatusLabel, usageLine } from "./shared";
import { orchestration, OrchestratorChip, ProjectMonogram, StatusPill, threadDone, threadPill } from "./thread-status";
import { useAgents } from "./use-agents";

/** The frame every canvas card shares: flat panel, hairline border, the whole card a drag handle. */
export function Card({
  width,
  tone,
  children,
  className = "",
  onClick,
  label,
}: {
  width: number;
  tone?: "busy" | "error" | "muted" | "done";
  children: ReactNode;
  className?: string;
  onClick?: () => void;
  label?: string;
}) {
  const border = tone === "busy" ? "border-accent/45" : tone === "error" ? "border-danger/50" : tone === "done" ? "border-ok/45" : "border-line";
  return (
    <div
      data-drag
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
      style={{ width }}
      className={`group/card rounded-xl border bg-panel text-[13px] transition-colors ${border} ${tone === "muted" ? "opacity-60" : ""} ${
        onClick ? "cursor-pointer hover:border-line-2 focus-visible:outline focus-visible:outline-accent" : "cursor-default"
      } ${className}`}
    >
      {children}
    </div>
  );
}

/** Buttons a card shows while the pointer is on it (always on touch screens and keyboard focus). */
function HoverActions({ children, className = "" }: { children: ReactNode; className?: string }) {
  return (
    <div
      className={`flex items-center gap-0.5 text-subtle opacity-0 transition-opacity duration-150 group-hover/card:opacity-100 focus-within:opacity-100 [@media(hover:none)]:opacity-100 ${className}`}
      data-nodrag
    >
      {children}
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
      className={`grid size-6 place-items-center rounded-md transition hover:bg-panel-2 hover:text-fg disabled:opacity-30 [&>svg]:size-3.5 ${launch && !disabled ? "cursor-grab active:cursor-grabbing" : ""}`}
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
  const [adding, setAdding] = useState(false);
  const info = machine.info;
  const sandbox = machine.sandbox;
  const projects = [...client.data.projects.values()].filter((r) => r.machine === machine.id).length;
  const os = info ? `${OS_NAMES[info.os] ?? info.os} · ${info.arch} · roll ${info.version}` : sandbox ? null : "Never connected";
  // A sandbox that isn't connected isn't "offline": it's asleep, stopped, or on its way up.
  const state = machineState(machine);
  const installed = info?.providers.filter((p) => p.installed) ?? [];
  const missing = installed.filter((p) => p.auth === "missing");
  const ready = installed.some((p) => p.auth === "ready");
  // A sandbox can't do anything until a provider is logged in: say so up front.
  const loginFirst = !!sandbox && machine.online && missing.length > 0 && !ready;
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
    <Card width={272} tone={state === "offline" ? "muted" : undefined} label={`Machine ${machine.name}`}>
      <div className="relative flex items-center gap-2 px-3.5 pt-3">
        <MachineIcon machine={machine} className="size-4 text-muted" />
        <span className="min-w-0 flex-1 truncate font-medium tracking-[-0.01em]">{machine.name}</span>
        <span className="flex shrink-0 items-center gap-1.5 text-[11px] text-subtle">
          <StatusDot status={machine.online ? "online" : "offline"} />
          <span className={coming(state) ? "shimmer" : ""}>{state}</span>
        </span>
        {/* Over the status while the pointer is on the card, so the name keeps its room. */}
        <HoverActions className="pointer-events-none absolute top-2.5 right-2.5 rounded-md bg-panel pl-1 group-hover/card:pointer-events-auto focus-within:pointer-events-auto [@media(hover:none)]:pointer-events-auto">
          {startNew && reachable(machine) && (
            <Tool label="New agent on this machine (or drag onto the canvas)" launch={`m:${machine.id}`} onClick={() => startNew(undefined, { machine: machine.id })}>
              <Plus />
            </Tool>
          )}
          {machine.online && (
            <>
              <Tool label="Add a project" onClick={() => setAdding(true)}>
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
      </div>
      <div className="space-y-2.5 px-3.5 pt-1 pb-3">
        {os && <p className="truncate font-mono text-[11px] text-subtle">{os}</p>}
        {!machine.online && !sandbox && (
          <p className="text-[11.5px] text-subtle">
            {machine.last_seen ? `Last seen ${ago(machine.last_seen)}.` : "Not connected yet."} Start it with <code className="font-mono text-muted">roll agent up</code>.
          </p>
        )}
        {sandbox && <SandboxPanel machine={machine} />}
        {loginFirst ? (
          <div className="space-y-2 rounded-lg border border-accent/35 px-3 py-2.5" role="group" aria-label="Log in to a provider">
            <p className="text-[12px]">Log in to a provider to start working here.</p>
            <div className="flex flex-wrap gap-1.5">
              {missing.map((p) => (
                <button key={p.id} data-nodrag disabled={busy !== null} onClick={() => login(p)} className={primary}>
                  <LogIn className="size-3.5" /> {busy === `login:${p.id}` ? <span className="shimmer">Opening…</span> : `Log in to ${p.name}`}
                </button>
              ))}
            </div>
            <p className="text-[11px] text-subtle">It opens a terminal: follow the link it prints, then paste the code.</p>
          </div>
        ) : (
          installed.length > 0 && (
            <ul className="flex flex-wrap gap-1" aria-label="Providers">
              {installed.map((p) => (
                <li key={p.id}>
                  {p.auth === "missing" ? (
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
                      className="inline-flex items-center gap-1.5 rounded-md border border-line px-1.5 py-0.5 text-[11px] text-muted"
                      title={[p.version, p.auth === "ready" ? "logged in" : p.detail].filter(Boolean).join(" · ") || undefined}
                    >
                      <span className={`size-1 rounded-full ${p.auth === "ready" ? "bg-ok" : "bg-subtle"}`} aria-label={p.auth === "ready" ? "Logged in" : "Unknown"} />
                      {p.name}
                    </span>
                  )}
                </li>
              ))}
            </ul>
          )
        )}
        {adding ? (
          <div data-nodrag>
            <NewProject machine={machine} onClose={() => setAdding(false)} onAdded={(id) => focus(`p:${id}`)} />
          </div>
        ) : (
          machine.online &&
          projects === 0 &&
          !loginFirst && (
            <button data-nodrag onClick={() => setAdding(true)} className={`${primary} w-full justify-center`}>
              <FolderPlus className="size-3.5" /> Add a project
            </button>
          )
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
  const stats = projectStats(client.data, project.id);
  return (
    <Card width={256} label={`Project ${project.name}`}>
      <div className="flex items-center gap-2 px-3.5 pt-3">
        <Folder className="size-4 text-muted" />
        <span className="min-w-0 flex-1 truncate font-medium tracking-[-0.01em]">{project.name}</span>
        <HoverActions>
          {startNew && (
            <Tool label="New agent here (or drag onto the canvas)" launch={`p:${project.id}`} disabled={!online} onClick={() => startNew(project.id)}>
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
          <Menu
            label="Project"
            trigger={<MoreHorizontal className="size-3.5" />}
            items={[
              { label: "Settings", icon: <Settings2 />, onSelect: () => setAgentsView({ kind: "settings", target: `p:${project.id}` }) },
              {
                label: "Remove from roll",
                confirm: "Remove? (the folder stays)",
                icon: <Trash2 />,
                danger: true,
                disabled: !online,
                onSelect: () => run("remove", () => client.run(machine!.id, { type: "project.remove", project: project.id })),
              },
            ]}
          />
        </HoverActions>
      </div>
      <div className="flex items-center gap-2 px-3.5 pt-1 pb-3 font-mono text-[11px] text-subtle">
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
      {project.settings.auto_pull && project.pull && <PullLine pull={project.pull} className="mx-3.5 mb-3 -mt-1" />}
      {!stats.threads && startNew && online && (
        <div className="px-3 pb-3">
          <button data-nodrag onClick={() => startNew(project.id)} className={`${primary} w-full justify-center`}>
            <Plus className="size-3.5" /> Start an agent
          </button>
        </div>
      )}
      {error && <p className="px-3.5 pb-3 text-[12px] text-danger">{error}</p>}
    </Card>
  );
}

/** Its work, the PRs it made and the agents in it. */
function StatsRows({ stats, providers }: { stats: ProjectStats; providers: readonly ProviderInfo[] }) {
  const landed = stats.changes.filter((c) => c.status === "landed").length;
  const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;
  return (
    <div className="mx-3.5 mb-3 space-y-1 border-t border-line pt-2.5 text-[11px] text-subtle">
      <p className="flex items-center gap-1.5">
        <span className="min-w-0 truncate">
          {plural(stats.threads, "thread", "threads")}
          {stats.running > 0 && <span className="text-fg"> · {stats.running} running</span>}
          {stats.waiting > 0 && <span className="text-muted"> · {stats.waiting} waiting</span>}
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

// ── thread ──

/**
 * A thread as T3 Code's thread rows show it, as a card: the project (its monogram and name) and the
 * machine it runs on, the status pill, the title and what it's doing, the PR and work item it's
 * attached to, its worktree and running terminals, and what it cost. A finished run you haven't
 * opened yet gives it T3's completed tone: a green border.
 */
export function ThreadNode({ thread, machine }: { thread: Thread; machine: Machine | undefined }) {
  const { client, openThread, startNew, readOnly } = useAgents();
  const data = client.data;
  const providers = providersFor(data, machine?.id);
  const { run: runFork, error: forkError } = useRun();
  // A fork from the card (T3's fork from the latest point): click, or drag the handle onto the canvas.
  const forkable = !!startNew && !readOnly && thread.archived_at === null && reachable(machine) && providers.find((p) => p.id === thread.provider)?.capabilities.fork !== false;
  const fork = () =>
    runFork("fork", async () => {
      const r = await client.run<{ type: "created"; id: string }>(machine!.id, { type: "thread.fork", thread: thread.id }, "created");
      openThread(r.id);
    });
  const running = isRunning(thread.status);
  const lastRun = thread.active_run ? data.runs.get(thread.active_run)?.value : latestRun(data.runs.values(), thread.id);
  const pending = [...data.requests.values()].map((r) => r.value).filter((r) => r.thread === thread.id && r.status === "pending");
  const pill = threadPill(thread, pending, lastRun);
  const done = threadDone(thread, lastRun);
  const usage = usageLine(thread.usage);
  const model = modelName(providers, thread.provider, thread.model);
  const project = data.projects.get(thread.project)?.value;
  const terminals = [...data.terminals.values()].filter((r) => r.value.thread === thread.id && r.value.status !== "exited").length;
  const orchestrated = thread.orchestrator ? orchestration(thread, [...data.threads.values()].map((r) => r.value)) : null;
  // Idle with nothing to say but the last prompt: no line when that's the title again.
  const quiet = !thread.error && !running && thread.status !== "waiting";
  const tone = thread.status === "error" ? "error" : running ? "busy" : thread.archived_at ? "muted" : done ? "done" : undefined;
  return (
    <Card width={296} tone={tone} onClick={() => openThread(thread.id)} label={`Thread ${thread.title}`}>
      <div className="px-3.5 pt-3 pb-3">
        <div className="flex min-w-0 items-center gap-1.5 text-[11px] text-subtle">
          {project && <ProjectMonogram name={project.name} className="size-3.5" />}
          {project && <span className="min-w-0 truncate text-muted">{project.name}</span>}
          {machine && (
            <span className="flex min-w-0 shrink items-center gap-1" title={`On ${machine.name}`} aria-label={`On ${machine.name}`}>
              <MachineIcon machine={machine} className="size-3 shrink-0" />
              <span className="truncate">{machine.name}</span>
            </span>
          )}
          <span className="ml-auto flex shrink-0 items-center gap-1.5">{pill ? <StatusPill pill={pill} /> : <span className="tabular-nums">{ago(thread.updated_at)}</span>}</span>
        </div>
        <div className="mt-1.5 flex items-start gap-2">
          <p className="line-clamp-2 min-w-0 flex-1 text-[13.5px] leading-snug font-medium tracking-[-0.01em]">{thread.title}</p>
          {forkable && (
            <HoverActions className="-my-0.5">
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
          ) : lastRun ? (
            lastRun.prompt
          ) : (
            <span className="text-subtle">No runs yet</span>
          )}
        </p>
        )}
        {(thread.orchestrator || thread.change || thread.work || thread.worktree || terminals > 0) && (
          <div className="mt-2 flex min-w-0 items-center gap-2 text-[11px] text-subtle">
            {orchestrated && <OrchestratorChip {...orchestrated} />}
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
        <p className="mt-2 flex items-center gap-1.5 border-t border-line pt-2 text-[11px] text-subtle">
          {thread.lineage && <GitFork className="size-3 shrink-0" aria-label={thread.lineage.relation === "fork" ? "Fork" : "Delegated"} />}
          <span className="min-w-0 truncate">
            {providerName(providers, thread.provider)}
            {model && ` · ${model}`}
            {thread.interaction_mode === "plan" && " · plan"}
            {thread.queued.length > 0 && ` · ${thread.queued.length} queued`}
          </span>
          <span className="ml-auto flex shrink-0 items-center gap-1.5 font-mono text-[10.5px]">
            {usage}
            <Stat stat={lastRun?.status === "rolled_back" ? null : lastRun?.stat} />
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
}

function latestRun(runs: Iterable<{ value: Run }>, thread: string) {
  let best: Run | undefined;
  for (const r of runs) if (r.value.thread === thread && (!best || r.value.ordinal > best.ordinal)) best = r.value;
  return best;
}

// ── provider subagent ──

export function SubagentNode({ node }: { node: Node }) {
  const { openThread } = useAgents();
  if (node.body.type !== "subagent") return null;
  const b = node.body;
  const running = node.status === "running";
  return (
    <Card width={240} tone={running ? "busy" : undefined} onClick={() => openThread(node.thread)} label={`Subagent ${b.agent}`}>
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

export function RequestNode({ request }: { request: Request }) {
  const { client, openThread, readOnly } = useAgents();
  const t = client.data.threads.get(request.thread);
  const machine = t?.machine;
  // Questions want room to think: the card says what's asked and the conversation answers it.
  if (request.body.type === "questions" && !readOnly) {
    const qs = request.body.questions;
    return (
      <Card width={280} label="Waiting for you" onClick={() => openThread(request.thread)}>
        <div className="px-3.5 py-3">
          <p className="flex items-center gap-1.5 text-[11px] text-subtle">
            <span className="size-1.5 shrink-0 rounded-full bg-accent" aria-hidden />
            <span className="truncate">Waiting for you · {t?.value.title}</span>
          </p>
          <p className="mt-1.5 line-clamp-2 text-[12.5px] leading-snug font-medium">{qs[0]?.question}</p>
          {qs.length > 1 && <p className="mt-0.5 text-[11.5px] text-subtle">and {qs.length - 1} more</p>}
          <span className="mt-2.5 inline-flex items-center gap-1 rounded-md bg-fg px-2 py-1 text-[11.5px] font-medium text-bg">Answer</span>
        </div>
      </Card>
    );
  }
  return (
    <Card width={320} label="Waiting for you">
      <div className="px-3.5 py-3" data-nodrag>
        <RequestCard
          request={request}
          compact
          context={
            <button className="flex min-w-0 items-center gap-1.5 hover:text-fg" onClick={() => openThread(request.thread)} title="Open the conversation">
              <span className="size-1.5 shrink-0 rounded-full bg-accent" aria-hidden />
              <span className="truncate">Waiting for you · {t?.value.title}</span>
            </button>
          }
          onRespond={readOnly ? undefined : (response) => client.run(machine!, { type: "request.respond", request: request.id, response })}
        />
      </div>
    </Card>
  );
}
