"use client";

import {
  ArrowUpRight,
  Check,
  CircleAlert,
  Folder,
  FolderPlus,
  GitBranch,
  GitFork,
  LogIn,
  MessageSquarePlus,
  RefreshCw,
  SquareTerminal,
  Trash2,
  X,
} from "lucide-react";
import { useState, type ReactNode } from "react";
import type { Machine, Node, Project, ProviderInfo, Request, Run, Thread } from "@/lib/agents-api";
import { providersFor } from "@/lib/agents-store";
import { btn, primary } from "../ui";
import { ChangeChip, WorkChip } from "./change";
import { Composer as ComposerSlot } from "./composer";
import { NewProject } from "./project-new";
import { RequestCard } from "./request-card";
import { SandboxPanel } from "./sandbox";
import {
  ago,
  loginCommand,
  MachineIcon,
  modelName,
  providerName,
  shortPath,
  Stat,
  StatusDot,
  threadStatusLabel,
  usageLine,
} from "./shared";
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
  tone?: "busy" | "error" | "muted";
  children: ReactNode;
  className?: string;
  onClick?: () => void;
  label?: string;
}) {
  const border = tone === "busy" ? "border-accent/45" : tone === "error" ? "border-danger/50" : "border-line";
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
      className={`cursor-default rounded-xl border bg-panel text-[13px] transition-colors ${border} ${tone === "muted" ? "opacity-60" : ""} ${
        onClick ? "hover:border-line-2 focus-visible:outline focus-visible:outline-accent" : ""
      } ${className}`}
    >
      {children}
    </div>
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
  const { focus } = useAgents();
  const [adding, setAdding] = useState(false);
  const info = machine.info;
  const sandbox = machine.sandbox;
  const projects = [...client.data.projects.values()].filter((r) => r.machine === machine.id).length;
  const os = info ? `${OS_NAMES[info.os] ?? info.os} · ${info.arch}` : sandbox ? null : "Never connected";
  // A sandbox that's up but whose daemon hasn't connected (yet, or again) isn't "offline".
  const state = machine.online ? "online" : sandbox?.status === "creating" ? "booting" : sandbox?.status === "running" ? "connecting" : "offline";
  const missing = info?.providers.filter((p) => p.installed && p.auth === "missing") ?? [];
  const ready = info?.providers.some((p) => p.installed && p.auth === "ready") ?? false;
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
    <Card width={288} tone={state === "offline" ? "muted" : undefined} label={`Machine ${machine.name}`}>
      <div className="flex items-center gap-2 border-b border-line px-3.5 py-3">
        <MachineIcon machine={machine} className="size-4 text-muted" />
        <span className="min-w-0 flex-1 truncate font-medium tracking-[-0.01em]">{machine.name}</span>
        <span className="flex items-center gap-1.5 text-[11px] text-subtle">
          <StatusDot status={machine.online ? "online" : "offline"} />
          <span className={state === "booting" || state === "connecting" ? "shimmer" : ""}>{state}</span>
        </span>
      </div>
      <div className="space-y-3 px-3.5 py-3">
        {os && (
          <p className="font-mono text-[11px] text-subtle">
            {os}
            {info && ` · roll ${info.version}`}
          </p>
        )}
        {!machine.online && !sandbox && (
          <p className="text-[11.5px] text-subtle">
            {machine.last_seen ? `Last seen ${ago(machine.last_seen)}.` : "Not connected yet."} Start it with <code className="font-mono text-muted">roll agent up</code>.
          </p>
        )}
        {sandbox && <SandboxPanel machine={machine} />}
        {loginFirst && (
          <div className="space-y-2 rounded-lg border border-accent/35 px-3 py-2.5" role="group" aria-label="Log in to a provider">
            <p className="text-[12px]">Log in to a provider to start working here.</p>
            <div className="flex flex-wrap gap-1.5">
              {missing.map((p) => (
                <button key={p.id} data-nodrag disabled={busy !== null} onClick={() => login(p)} className={primary}>
                  <LogIn className="size-3.5" /> {busy === `login:${p.id}` ? <span className="shimmer">Opening…</span> : `Log in to ${p.name}`}
                </button>
              ))}
            </div>
            <p className="text-[11px] text-subtle">It opens a terminal here: follow the link it prints, then paste the code.</p>
          </div>
        )}
        {info && info.providers.length > 0 && (
          <ul className="space-y-1.5" aria-label="Providers">
            {info.providers.map((p) => (
              <li key={p.id} className="flex items-center gap-2 text-[12px]">
                <span className="min-w-0 flex-1 truncate" title={p.detail ?? undefined}>
                  {p.name}
                  {p.version && <span className="ml-1.5 font-mono text-[10.5px] text-subtle">{p.version}</span>}
                </span>
                {!p.installed ? (
                  <span className="text-[11px] text-subtle">not installed</span>
                ) : p.auth === "ready" ? (
                  <Check className="size-3.5 text-ok" aria-label="Logged in" />
                ) : p.auth === "missing" ? (
                  <button
                    data-nodrag
                    disabled={!machine.online || busy !== null}
                    onClick={() => login(p)}
                    className="inline-flex items-center gap-1 rounded-md border border-line px-1.5 py-0.5 text-[11px] text-fg hover:border-line-2 disabled:opacity-40"
                    title={p.detail ?? `Log in to ${p.name}`}
                  >
                    <LogIn className="size-3" /> Log in
                  </button>
                ) : (
                  <span className="text-[11px] text-subtle" title={p.detail ?? undefined}>
                    unknown
                  </span>
                )}
              </li>
            ))}
          </ul>
        )}
        {adding ? (
          <NewProject machine={machine} onClose={() => setAdding(false)} onAdded={(id) => focus(`p:${id}`)} />
        ) : machine.online ? (
          <div className="space-y-2">
            {projects === 0 && !loginFirst && <p className="text-[12px] text-muted">Add a project: a folder here, or one of your roll code repos.</p>}
            <div className="flex flex-wrap gap-1.5">
              <button data-nodrag onClick={() => setAdding(true)} className={projects === 0 && !loginFirst ? primary : btn}>
                <FolderPlus className="size-3.5" /> Add project
              </button>
              <button
                data-nodrag
                disabled={busy !== null}
                onClick={() => run("refresh", () => client.run(machine.id, { type: "provider.refresh" }))}
                className={btn}
                title="Check providers again (after a login)"
                aria-label="Check providers again"
              >
                <RefreshCw className={`size-3.5 ${busy === "refresh" ? "animate-spin motion-reduce:animate-none" : ""}`} />
              </button>
              <button data-nodrag disabled={busy !== null} onClick={openTerminal} className={btn} title="Open a terminal" aria-label="Open a terminal">
                <SquareTerminal className="size-3.5" />
              </button>
            </div>
          </div>
        ) : null}
        {error && <p className="text-[12px] text-danger">{error}</p>}
      </div>
    </Card>
  );
}

// ── project ──

export function ProjectNode({ project, machine }: { project: Project; machine: Machine | undefined }) {
  const { client, busy, error, run } = useRun();
  const { setExpanded, demo } = useAgents();
  const [composing, setComposing] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const online = !!machine?.online;
  const threads = [...client.data.threads.values()].some((r) => r.value.project === project.id);
  return (
    <Card width={272} label={`Project ${project.name}`}>
      <div className="flex items-center gap-2 px-3.5 pt-3">
        <Folder className="size-4 text-muted" />
        <span className="min-w-0 flex-1 truncate font-medium tracking-[-0.01em]">{project.name}</span>
        {project.git?.branch && (
          <span className="flex items-center gap-1 font-mono text-[11px] text-subtle" title={project.git.dirty ? "Uncommitted changes" : undefined}>
            <GitBranch className="size-3" />
            {project.git.branch}
            {project.git.dirty && <span className="size-1 rounded-full bg-muted" aria-label="dirty" />}
          </span>
        )}
      </div>
      <div className="flex items-center gap-2 px-3.5 pt-1 pb-3 font-mono text-[11px] text-subtle">
        <span className="min-w-0 flex-1 truncate" title={project.path}>
          {shortPath(project.path, machine?.info?.home)}
        </span>
        {project.repo && (
          <a
            data-nodrag
            href={demo ? undefined : `/${project.repo.owner}/${project.repo.repo}`}
            className="flex shrink-0 items-center gap-0.5 hover:text-fg"
          >
            {project.repo.owner}/{project.repo.repo}
            <ArrowUpRight className="size-3" />
          </a>
        )}
      </div>
      {composing && machine ? (
        <div className="border-t border-line p-3" data-nodrag>
          <NewThread
            project={project}
            machine={machine}
            onCancel={() => setComposing(false)}
            onCreated={(id) => {
              setComposing(false);
              setExpanded(id, true);
            }}
          />
        </div>
      ) : (
        <div className="flex items-center gap-1.5 border-t border-line px-3 py-2">
          <button data-nodrag disabled={!online} onClick={() => setComposing(true)} className={threads ? btn : primary}>
            <MessageSquarePlus className="size-3.5" /> {threads ? "New thread" : "Start a thread"}
          </button>
          <button
            data-nodrag
            disabled={!online || busy !== null}
            onClick={() => run("term", () => client.run(machine!.id, { type: "terminal.open", project: project.id, cols: 100, rows: 26 }))}
            className={btn}
            title="Open a terminal here"
          >
            <SquareTerminal className="size-3.5" />
          </button>
          {confirm ? (
            <span className="ml-auto flex items-center gap-1">
              <button
                data-nodrag
                onClick={() => run("remove", () => client.run(machine!.id, { type: "project.remove", project: project.id }))}
                className="rounded-md px-1.5 py-1 text-[11px] text-danger hover:bg-danger/10"
              >
                Remove
              </button>
              <button data-nodrag onClick={() => setConfirm(false)} className="p-1 text-subtle hover:text-fg" aria-label="Keep">
                <X className="size-3.5" />
              </button>
            </span>
          ) : (
            <button
              data-nodrag
              disabled={!online}
              onClick={() => setConfirm(true)}
              className="ml-auto p-1 text-subtle hover:text-fg disabled:opacity-40"
              title="Remove the project from roll (the folder stays)"
              aria-label="Remove project"
            >
              <Trash2 className="size-3.5" />
            </button>
          )}
        </div>
      )}
      {error && <p className="px-3.5 pb-3 text-[12px] text-danger">{error}</p>}
    </Card>
  );
}

/** Start a thread from a project: the composer in "new thread" mode. */
function NewThread({
  project,
  machine,
  onCancel,
  onCreated,
}: {
  project: Project;
  machine: Machine;
  onCancel: () => void;
  onCreated: (id: string) => void;
}) {
  const { client } = useAgents();
  return (
    <ComposerSlot
      mode={{ kind: "new", project, machine }}
      onCancel={onCancel}
      onSubmit={async (c) => {
        const r = await client.run<{ type: "created"; id: string }>(machine.id, { type: "thread.create", project: project.id, ...c }, "created");
        onCreated(r.id);
      }}
    />
  );
}

// ── thread (collapsed) ──

export function ThreadNode({ thread, machine }: { thread: Thread; machine: Machine | undefined }) {
  const { client, setExpanded } = useAgents();
  const providers = providersFor(client.data, machine?.id);
  const busy = thread.status === "starting" || thread.status === "running" || thread.status === "waiting";
  const lastRun = thread.active_run ? client.data.runs.get(thread.active_run)?.value : latestRun(client.data.runs.values(), thread.id);
  const usage = usageLine(thread.usage);
  const model = modelName(providers, thread.provider, thread.model);
  return (
    <Card
      width={304}
      tone={thread.status === "error" ? "error" : busy ? "busy" : thread.archived_at ? "muted" : undefined}
      onClick={() => setExpanded(thread.id, true)}
      label={`Thread ${thread.title}`}
    >
      <div className="px-3.5 pt-3 pb-2.5">
        <div className="flex items-start gap-2">
          <StatusDot status={thread.status} className="mt-[7px]" />
          <p className="line-clamp-2 min-w-0 flex-1 leading-snug font-medium tracking-[-0.01em]">{thread.title}</p>
          {thread.pending_requests > 0 && (
            <span
              className="shrink-0 rounded-full bg-accent px-1.5 py-px font-mono text-[10.5px] font-semibold text-white tabular-nums"
              title={`${thread.pending_requests} waiting for you`}
            >
              {thread.pending_requests}
            </span>
          )}
        </div>
        <p className="mt-1 flex items-center gap-1.5 pl-3.5 text-[11.5px] text-subtle">
          {thread.lineage && (
            <GitFork className="size-3" aria-label={thread.lineage.relation === "fork" ? "Fork" : "Delegated"} />
          )}
          <span className="truncate">
            {providerName(providers, thread.provider)}
            {model && ` · ${model}`}
            {thread.effort && ` · ${thread.effort}`}
            {thread.interaction_mode === "plan" && " · plan"}
          </span>
        </p>
        {(thread.change || thread.work) && (
          <div className="mt-1.5 flex min-w-0 items-center gap-2 pl-3.5">
            {thread.change && <ChangeChip change={thread.change} />}
            {thread.work && <WorkChip work={thread.work} className="min-w-0 flex-1" />}
          </div>
        )}
        <p className={`mt-2 truncate pl-3.5 text-[12px] ${busy ? "" : "text-muted"}`}>
          {thread.error ? (
            <span className="flex items-center gap-1 text-danger">
              <CircleAlert className="size-3" /> {thread.error}
            </span>
          ) : busy ? (
            <span className="shimmer">{thread.activity ?? threadStatusLabel[thread.status]}</span>
          ) : lastRun ? (
            <span className="truncate">{lastRun.prompt}</span>
          ) : (
            <span className="text-subtle">No runs yet</span>
          )}
        </p>
      </div>
      <div className="flex items-center gap-2 border-t border-line px-3.5 py-1.5 font-mono text-[10.5px] text-subtle">
        <span>
          {thread.run_count} run{thread.run_count === 1 ? "" : "s"}
        </span>
        {usage && <span>· {usage}</span>}
        {thread.queued.length > 0 && <span className="text-muted">· {thread.queued.length} queued</span>}
        <Stat stat={lastRun?.status === "rolled_back" ? null : lastRun?.stat} className="ml-auto" />
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
  const { setExpanded } = useAgents();
  if (node.body.type !== "subagent") return null;
  const b = node.body;
  const running = node.status === "running";
  return (
    <Card width={248} tone={running ? "busy" : undefined} onClick={() => setExpanded(node.thread, true)} label={`Subagent ${b.agent}`}>
      <div className="px-3 py-2.5">
        <p className="flex items-center gap-1.5 text-[12px] font-medium">
          <StatusDot status={running ? "running" : node.status === "failed" ? "error" : "idle"} />
          {b.agent}
          <span className="ml-auto text-[10.5px] font-normal text-subtle">{running ? "working" : node.status}</span>
        </p>
        <p className="mt-1 line-clamp-2 text-[11.5px] text-muted">{b.prompt}</p>
        {b.result && <p className="mt-1.5 line-clamp-2 border-l border-line-2 pl-2 text-[11.5px] text-subtle">{b.result}</p>}
      </div>
    </Card>
  );
}

// ── request beside its thread ──

export function RequestNode({ request }: { request: Request }) {
  const { client, focus, readOnly } = useAgents();
  const t = client.data.threads.get(request.thread);
  const machine = t?.machine;
  return (
    <Card width={340} tone="busy" label="Waiting for you">
      <div className="flex items-center gap-1.5 border-b border-line px-3.5 py-2 text-[11px] text-subtle" data-drag>
        <span className="size-1.5 rounded-full bg-accent" aria-hidden />
        Waiting for you
        <button className="truncate hover:text-fg" onClick={() => focus(`t:${request.thread}`)} title="Open the thread">
          · {t?.value.title}
        </button>
      </div>
      <div className="px-3.5 py-3" data-nodrag>
        <RequestCard
          request={request}
          onRespond={readOnly ? undefined : (response) => client.run(machine!, { type: "request.respond", request: request.id, response })}
        />
      </div>
    </Card>
  );
}
