import { Plus, X } from "lucide-react";
import type { AgentsData } from "@/lib/agents-store";
import type { Thread } from "@/lib/agents-api";
import { showAgents } from "@/lib/agents-surface";
import { useAgentsSnapshot, useHold } from "../agents/use-agents";
import { useAgentsConnection } from "../agents/use-connection";

/**
 * The agents where the work is (docs/code/agents.md): a few rows in the sidebar, a project's own
 * agents on its canvas. A row opens its conversation over the page; nothing navigates away.
 * A project's agents are the threads in agent projects whose repository the project holds.
 */
export function useAgents(): AgentsData | null {
  return useAgentsState().data;
}

/** The agents, or why there are none to show (signed out, no owner yet, the API down). */
export function useAgentsState(): { data: AgentsData | null; problem: string | null } {
  const conn = useAgentsConnection({});
  useHold(conn.client);
  const data = useAgentsSnapshot(conn.client)?.data ?? null;
  return { data, problem: data ? null : conn.error };
}

const ORDER: Record<Thread["status"], number> = { waiting: 0, running: 1, starting: 1, error: 2, idle: 3 };
const DAY = 24 * 60 * 60 * 1000;

/** Live threads first (waiting on you, then running, then failed), then the last day's, newest first. */
export function threadsOf(data: AgentsData, repos?: ReadonlySet<string>): Thread[] {
  const now = Date.now();
  const inRepo = (t: Thread) => {
    if (!repos) return true;
    const r = data.projects.get(t.project)?.value.repo;
    return !!r && repos.has(`${r.owner}/${r.repo}`);
  };
  return [...data.threads.values()]
    .map((r) => r.value)
    .filter((t) => t.archived_at === null && t.lineage?.relation !== "subagent" && inRepo(t) && (t.status !== "idle" || now - t.updated_at < DAY))
    .sort((a, b) => ORDER[a.status] - ORDER[b.status] || b.updated_at - a.updated_at);
}

/** The agent project to start a new agent in for these repos: the most recently used one. */
export function agentProjectFor(data: AgentsData, repos: ReadonlySet<string>): string | undefined {
  return [...data.projects.values()]
    .map((r) => r.value)
    .filter((p) => p.repo && repos.has(`${p.repo.owner}/${p.repo.repo}`))
    .sort((a, b) => b.updated_at - a.updated_at)[0]?.id;
}

function Dot({ status }: { status: Thread["status"] }) {
  const cls =
    status === "running" || status === "starting"
      ? "bg-accent pulse-dot"
      : status === "waiting"
        ? "shadow-[inset_0_0_0_1.5px_var(--accent)]"
        : status === "error"
          ? "bg-danger"
          : "bg-subtle";
  return <i aria-hidden className={`size-1.5 shrink-0 rounded-full ${cls}`} />;
}

const LABEL: Record<Thread["status"], string> = { running: "Working", starting: "Starting", waiting: "Waiting for you", error: "Failed", idle: "Done" };

export function AgentRow({ thread, compact = false }: { thread: Thread; compact?: boolean }) {
  const busy = thread.status === "running" || thread.status === "starting";
  const line = thread.status === "waiting" ? LABEL.waiting : (busy && thread.activity) || LABEL[thread.status];
  return (
    <button
      onClick={() => showAgents({ kind: "thread", thread: thread.id })}
      className="group flex w-full items-start gap-2 rounded-md px-2 py-1.5 text-left transition-colors hover:bg-white/[0.04]"
      title={thread.title}
    >
      <span className="mt-[5px]">
        <Dot status={thread.status} />
      </span>
      <span className="min-w-0 flex-1 leading-tight">
        <span className={`block truncate ${compact ? "text-[12px]" : "text-[12.5px]"} ${thread.status === "idle" ? "text-muted" : "text-fg"}`}>{thread.title || "New agent"}</span>
        <span className={`mt-0.5 block truncate text-[10.5px] ${busy ? "shimmer text-muted" : thread.status === "waiting" ? "text-accent" : "text-subtle"}`}>{line}</span>
      </span>
    </button>
  );
}

/** The sidebar's agents: what's working or waiting right now, and a way to start one. */
export function SidebarAgents() {
  const data = useAgents();
  const threads = data ? threadsOf(data).filter((t) => t.status !== "idle") : [];
  return (
    <div className="mt-4">
      <div className="flex items-center justify-between px-2 pb-1 text-[11px] font-medium text-subtle">
        <button onClick={() => showAgents({ kind: "canvas" })} className="flex items-center gap-1.5 transition-colors hover:text-fg" title="All agents (⌘J)">
          Agents
          {threads.length > 0 && <span className="font-mono text-[10px] text-accent tabular-nums">{threads.length}</span>}
        </button>
        <button
          onClick={() => showAgents({ kind: "new" })}
          aria-label="New agent"
          title="New agent"
          className="grid size-5 place-items-center rounded-md text-muted transition-colors hover:bg-white/[0.06] hover:text-fg"
        >
          <Plus className="size-3.5" />
        </button>
      </div>
      {threads.length ? (
        <div className="space-y-px">
          {threads.slice(0, 4).map((t) => (
            <AgentRow key={t.id} thread={t} compact />
          ))}
          {threads.length > 4 && (
            <button onClick={() => showAgents({ kind: "canvas" })} className="px-2 py-1 text-[11px] text-subtle hover:text-fg">
              {threads.length - 4} more
            </button>
          )}
        </div>
      ) : (
        <button onClick={() => showAgents({ kind: "canvas" })} className="w-full px-2 py-1 text-left text-[11.5px] text-subtle transition-colors hover:text-muted">
          {data ? "None running" : "Open agents"}
        </button>
      )}
    </div>
  );
}

/**
 * A project's agents, in a panel on the left of its canvas: the threads in its repositories.
 * Slides in and out (the canvas's sheets move the same way).
 */
export function ProjectAgents({ open, repos, onClose }: { open: boolean; repos: ReadonlySet<string>; onClose: () => void }) {
  const { data, problem } = useAgentsState();
  const threads = data ? threadsOf(data, repos) : [];
  const target = data ? agentProjectFor(data, repos) : undefined;
  return (
    <aside
      aria-hidden={!open}
      className={`absolute top-2 bottom-2 left-2 z-20 flex w-[260px] flex-col overflow-hidden rounded-xl border border-line-2 bg-sheet shadow-sheet inset-shadow-highlight transition-transform duration-300 ease-drawer motion-reduce:transition-none ${
        open ? "translate-x-0" : "pointer-events-none -translate-x-[calc(100%+1rem)]"
      }`}
    >
      <div className="flex items-center gap-2 border-b border-line px-3 py-2">
        <span className="text-[12.5px] font-semibold tracking-[-0.015em]">Agents</span>
        <span className="font-mono text-[10.5px] text-subtle">{threads.length}</span>
        <button
          onClick={() => showAgents({ kind: "new", project: target })}
          disabled={!repos.size}
          className="ml-auto flex h-6 items-center gap-1 rounded-md border border-line-2 bg-raised px-1.5 text-[11px] text-muted inset-shadow-highlight transition-colors hover:text-fg disabled:opacity-40"
        >
          <Plus className="size-3" /> New
        </button>
        <button onClick={onClose} aria-label="Close agents" className="grid size-6 place-items-center rounded-md text-muted hover:bg-white/[0.06] hover:text-fg">
          <X className="size-3.5" />
        </button>
      </div>
      <div className="flex-1 overflow-y-auto p-1 [scrollbar-width:none]">
        {!repos.size ? (
          <p className="px-2 py-2 text-[11.5px] leading-relaxed text-subtle">Add a repository to this project (Add, then pick it). Agents working in it show here.</p>
        ) : !data ? (
          <p className={`px-2 py-2 text-[11.5px] leading-relaxed text-subtle ${problem ? "" : "shimmer"}`}>{problem ?? "Connecting to your agents…"}</p>
        ) : threads.length ? (
          threads.map((t) => <AgentRow key={t.id} thread={t} />)
        ) : (
          <p className="px-2 py-2 text-[11.5px] leading-relaxed text-subtle">No agents in {[...repos].join(", ")} today. Start one with New.</p>
        )}
      </div>
    </aside>
  );
}
