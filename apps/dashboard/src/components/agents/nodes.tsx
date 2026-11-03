"use client";

import {
  ArrowUpRight,
  Check,
  ChevronRight,
  CircleAlert,
  Folder,
  FolderOpen,
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
import { useEffect, useState, type ReactNode } from "react";
import type { FsEntry, Machine, Node, Project, Request, Thread } from "@/lib/agents-api";
import { providersFor } from "@/lib/agents-store";
import { btn, input, primary } from "../ui";
import { Composer as ComposerSlot } from "./composer";
import { RequestCard } from "./request-card";
import {
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

export function MachineNode({ machine }: { machine: Machine }) {
  const { client, busy, error, run } = useRun();
  const [browsing, setBrowsing] = useState(false);
  const info = machine.info;
  const os = info ? `${info.os === "macos" ? "macOS" : info.os === "linux" ? "Linux" : info.os} · ${info.arch}` : "Never connected";
  return (
    <Card width={288} tone={machine.online ? undefined : "muted"} label={`Machine ${machine.name}`}>
      <div className="flex items-center gap-2 border-b border-line px-3.5 py-3">
        <MachineIcon machine={machine} className="size-4 text-muted" />
        <span className="min-w-0 flex-1 truncate font-medium tracking-[-0.01em]">{machine.name}</span>
        <span className="flex items-center gap-1.5 text-[11px] text-subtle">
          <StatusDot status={machine.online ? "online" : "offline"} />
          {machine.online ? "online" : "offline"}
        </span>
      </div>
      <div className="space-y-3 px-3.5 py-3">
        <p className="font-mono text-[11px] text-subtle">
          {os}
          {info && ` · roll ${info.version}`}
          {machine.sandbox && ` · sandbox ${machine.sandbox.size}`}
        </p>
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
                    onClick={() =>
                      run(`login:${p.id}`, () =>
                        client.run(machine.id, { type: "terminal.open", command: loginCommand(p), cols: 100, rows: 26 }),
                      )
                    }
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
        {browsing ? (
          <DirBrowser machine={machine} onClose={() => setBrowsing(false)} />
        ) : (
          <div className="flex flex-wrap gap-1.5">
            <button data-nodrag disabled={!machine.online} onClick={() => setBrowsing(true)} className={btn}>
              <FolderPlus className="size-3.5" /> Add project
            </button>
            <button
              data-nodrag
              disabled={!machine.online || busy !== null}
              onClick={() => run("refresh", () => client.run(machine.id, { type: "provider.refresh" }))}
              className={btn}
              title="Check providers again (after a login)"
            >
              <RefreshCw className={`size-3.5 ${busy === "refresh" ? "animate-spin motion-reduce:animate-none" : ""}`} />
            </button>
            <button
              data-nodrag
              disabled={!machine.online || busy !== null}
              onClick={() => run("term", () => client.run(machine.id, { type: "terminal.open", cols: 100, rows: 26 }))}
              className={btn}
              title="Open a terminal"
            >
              <SquareTerminal className="size-3.5" />
            </button>
          </div>
        )}
        {error && <p className="text-[12px] text-danger">{error}</p>}
      </div>
    </Card>
  );
}

/** Pick a directory on the machine (`fs.list`) and add it as a project. */
export function DirBrowser({ machine, onClose }: { machine: Machine; onClose: () => void }) {
  const { client, busy, error, run } = useRun();
  const [path, setPath] = useState(machine.info?.home ?? "/");
  const [entries, setEntries] = useState<FsEntry[] | null>(null);
  const [typed, setTyped] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setEntries(null);
    client
      .run<Extract<Awaited<ReturnType<typeof client.send>>, { type: "entries" }>>(machine.id, { type: "fs.list", path }, "entries")
      .then((r) => live && setEntries(r.entries.filter((e) => e.dir && !e.name.startsWith(".")).sort((a, b) => a.name.localeCompare(b.name))))
      .catch(() => live && setEntries([]));
    return () => {
      live = false;
    };
  }, [client, machine.id, path]);

  const up = path.replace(/\/[^/]+\/?$/, "") || "/";
  const join = (name: string) => `${path.replace(/\/$/, "")}/${name}`;
  return (
    <div data-nodrag className="space-y-2 rounded-lg border border-line bg-bg p-2">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (typed?.trim()) setPath(typed.trim());
          setTyped(null);
        }}
      >
        <input
          value={typed ?? shortPath(path, machine.info?.home)}
          onFocus={() => setTyped(path)}
          onBlur={() => setTyped(null)}
          onChange={(e) => setTyped(e.target.value)}
          aria-label="Directory"
          spellCheck={false}
          className={`${input} w-full font-mono text-[11.5px]`}
        />
      </form>
      <ul className="max-h-44 overflow-auto text-[12px]" data-scroll aria-label="Folders">
        {path !== "/" && (
          <li>
            <button onClick={() => setPath(up)} className="flex w-full items-center gap-1.5 rounded px-1.5 py-1 text-subtle hover:bg-panel-2 hover:text-fg">
              <FolderOpen className="size-3.5" /> ..
            </button>
          </li>
        )}
        {entries === null ? (
          <li className="px-1.5 py-1 text-subtle">
            <span className="shimmer">Listing…</span>
          </li>
        ) : entries.length === 0 ? (
          <li className="px-1.5 py-1 text-subtle">No folders</li>
        ) : (
          entries.map((e) => (
            <li key={e.name}>
              <button
                onClick={() => setPath(join(e.name))}
                className="flex w-full items-center gap-1.5 rounded px-1.5 py-1 text-left text-muted hover:bg-panel-2 hover:text-fg"
              >
                <Folder className="size-3.5 text-subtle" />
                <span className="flex-1 truncate">{e.name}</span>
                <ChevronRight className="size-3 text-subtle" />
              </button>
            </li>
          ))
        )}
      </ul>
      <div className="flex items-center gap-1.5">
        <button
          disabled={busy !== null}
          onClick={() =>
            run("add", async () => {
              await client.run(machine.id, { type: "project.add", path }, "created");
              onClose();
            })
          }
          className={primary}
        >
          {busy ? <span className="shimmer">Adding…</span> : "Add this folder"}
        </button>
        <button onClick={onClose} className={btn}>
          Cancel
        </button>
      </div>
      {error && <p className="text-[12px] text-danger">{error}</p>}
    </div>
  );
}

// ── project ──

export function ProjectNode({ project, machine }: { project: Project; machine: Machine | undefined }) {
  const { client, busy, error, run } = useRun();
  const { setExpanded, demo } = useAgents();
  const [composing, setComposing] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const online = !!machine?.online;
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
          <button data-nodrag disabled={!online} onClick={() => setComposing(true)} className={btn}>
            <MessageSquarePlus className="size-3.5" /> New thread
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
        <Stat stat={lastRun?.stat} className="ml-auto" />
      </div>
    </Card>
  );
}

function latestRun(runs: Iterable<{ value: import("@/lib/agents-api").Run }>, thread: string) {
  let best: import("@/lib/agents-api").Run | undefined;
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
  const { client, focus } = useAgents();
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
          onRespond={(response) => client.run(machine!, { type: "request.respond", request: request.id, response })}
        />
      </div>
    </Card>
  );
}
