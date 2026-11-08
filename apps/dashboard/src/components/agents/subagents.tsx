"use client";

/**
 * Subagents in the conversation, as T3 Code shows them (`V2LifecycleRow.tsx`'s
 * `SubagentTimelineLink`, `TimelineV2EventRow.tsx`, `ProviderSubagentBar.tsx`; MIT). A subagent is
 * a row: its avatar with a status dot, what it was asked, what it's doing now (or its answer, or
 * how it ended), a live timer, and a chevron; several in a row sit under "N subagents". Clicking
 * one opens its own transcript: a delegated roll thread opens as the thread it is; a provider's
 * subagent (Claude's Agent tool, Codex's collab agents) opens in place of the conversation, its
 * work as a transcript of its own, with T3's bar instead of the composer (what it runs, how long,
 * "Runs on its own", Open parent).
 */

import { ArrowUpLeft, Bot, ChevronRight } from "lucide-react";
import type { Node, NodeId, SubagentBody } from "@/lib/agents-api";
import type { NodeTree } from "@/lib/agents-store";
import { elapsed } from "./shared";

type Status = Node["status"];

/** T3's subagent status labels. */
export const subagentStatus: Record<Status, string> = {
  running: "Running",
  waiting: "Waiting",
  completed: "Completed",
  failed: "Failed",
  interrupted: "Stopped",
};

const dot: Record<Status, string> = {
  running: "bg-accent pulse-dot",
  waiting: "bg-accent",
  completed: "bg-ok",
  failed: "bg-danger",
  interrupted: "bg-subtle",
};

const live = (s: Status) => s === "running" || s === "waiting";

/** What it was asked, on one line (T3 titles a subagent by its clipped prompt). */
export function subagentTitle(b: SubagentBody): string {
  const line = b.prompt.trim().split("\n")[0] ?? "";
  return line ? (line.length > 90 ? `${line.slice(0, 89)}…` : line) : b.agent;
}

/** The line under the title: what it's doing now, its answer, or how it ended. */
export function subagentDetail(n: Node, b: SubagentBody): string {
  if (live(n.status)) return b.progress?.trim() || subagentStatus[n.status];
  const result = b.result?.trim().split("\n").find((l) => l.trim());
  return result ?? subagentStatus[n.status];
}

/** How long it ran (or has been running). */
export function subagentTime(n: Node, now: number): string {
  return elapsed((n.completed_at ?? now) - n.started_at);
}

export function SubagentRow({ node, now, onOpen }: { node: Node; now: number; onOpen?: () => void }) {
  if (node.body.type !== "subagent") return null;
  const b = node.body;
  const title = subagentTitle(b);
  const running = live(node.status);
  return (
    <button
      type="button"
      onClick={onOpen}
      disabled={!onOpen}
      aria-label={`Open ${title}`}
      className="group/sub flex w-full min-w-0 items-center gap-2.5 rounded-lg border border-line bg-raised/60 px-2.5 py-1.5 text-left transition-colors enabled:hover:bg-fg/[0.04]"
    >
      <span className="relative grid size-6 shrink-0 place-items-center rounded-md bg-fg/[0.05] text-muted">
        <Bot className="size-3.5" strokeWidth={1.5} />
        <span className={`absolute -right-0.5 -bottom-0.5 size-2 rounded-full ring-2 ring-raised ${dot[node.status]}`} aria-hidden />
      </span>
      <span className="min-w-0 flex-1">
        <span className="flex min-w-0 items-center gap-1.5">
          <span className="truncate text-[13px] font-medium">{title}</span>
          <span className="shrink-0 font-mono text-[10.5px] text-subtle">{b.child ? "thread" : b.agent}</span>
        </span>
        <span className={`block truncate text-[12px] ${node.status === "failed" ? "text-danger/80" : "text-muted"}`}>
          <span className={running && b.progress ? "shimmer" : ""}>{subagentDetail(node, b)}</span>
        </span>
      </span>
      <span className="shrink-0 font-mono text-[11px] text-subtle tabular-nums" title={subagentStatus[node.status]}>
        {subagentTime(node, now)}
      </span>
      {onOpen && <ChevronRight className="size-3.5 shrink-0 text-subtle transition-transform duration-200 group-hover/sub:translate-x-0.5 motion-reduce:transition-none" aria-hidden />}
    </button>
  );
}

/** Several subagents started together (T3's "{n} subagents" row). */
export function SubagentGroup({ nodes, now, open }: { nodes: Node[]; now: number; open: (n: Node) => (() => void) | undefined }) {
  const running = nodes.filter((n) => n.status === "running").length;
  const waiting = nodes.filter((n) => n.status === "waiting").length;
  return (
    <section aria-label={`${nodes.length} subagents`} className="space-y-1">
      <p className="flex min-h-6 items-center gap-1.5 px-0.5 text-[13px] text-muted">
        <span className="grid w-6 place-items-center">
          <Bot className={`size-4 ${running || waiting ? "text-accent" : "opacity-70"}`} strokeWidth={1.5} />
        </span>
        {nodes.length} subagents
        {running > 0 && <span className="text-subtle">· {running} running</span>}
        {waiting > 0 && <span className="text-subtle">· {waiting} waiting</span>}
      </p>
      <div className="ms-7 space-y-1">
        {nodes.map((n) => (
          <SubagentRow key={n.id} node={n} now={now} onOpen={open(n)} />
        ))}
      </div>
    </section>
  );
}

/** The subagent node with this id anywhere in a run's tree. */
export function findTree(trees: readonly NodeTree[], id: NodeId): NodeTree | null {
  for (const t of trees) {
    if (t.node.id === id) return t;
    const inner = findTree(t.children, id);
    if (inner) return inner;
  }
  return null;
}

/** In place of the composer while a provider's subagent is open (T3's `ProviderSubagentBar`). */
export function SubagentBar({ node, now, onParent }: { node: Node; now: number; onParent: () => void }) {
  if (node.body.type !== "subagent") return null;
  const b = node.body;
  const running = live(node.status);
  const status = running ? `Working ${subagentTime(node, now)}` : `${subagentStatus[node.status]} in ${subagentTime(node, now)}`;
  return (
    <div className="flex min-w-0 items-center gap-2.5 rounded-2xl border border-outline bg-raised px-3 py-2.5 text-[12.5px] shadow-float inset-shadow-highlight" role="status">
      <span className="relative grid size-6 shrink-0 place-items-center rounded-md bg-fg/[0.05] text-muted">
        <Bot className="size-3.5" strokeWidth={1.5} />
        <span className={`absolute -right-0.5 -bottom-0.5 size-2 rounded-full ring-2 ring-raised ${dot[node.status]}`} aria-hidden />
      </span>
      <span className="min-w-0 flex-1">
        <span className="flex min-w-0 items-center gap-1.5">
          <span className="truncate font-medium">{b.agent}</span>
          {b.model && <span className="truncate font-mono text-[11px] text-subtle">{b.model}</span>}
        </span>
        <span className="flex min-w-0 items-center gap-1.5 text-muted tabular-nums">
          <span className={running ? "shimmer" : ""}>{status}</span>
          <span className="text-faint">·</span>
          <span className="truncate">Runs on its own</span>
        </span>
      </span>
      <button
        type="button"
        onClick={onParent}
        className="inline-flex h-7 shrink-0 items-center gap-1.5 rounded-full px-3 text-[12.5px] text-muted transition hover:bg-fg/[0.06] hover:text-fg active:scale-[0.97] motion-reduce:transition-none"
      >
        <ArrowUpLeft className="size-3.5" /> Open parent
      </button>
    </div>
  );
}
