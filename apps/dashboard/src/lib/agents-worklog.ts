/**
 * The work log: a run's nodes as T3 Code's timeline shows them (T3's
 * `packages/client-runtime/src/work-log/presentation.ts` `summarizeToolGroup`, and
 * `apps/web/src/components/chat/MessagesTimeline.logic.ts` for grouping and turn folds; MIT).
 *
 * - Consecutive tool calls and thinking join one group; messages, plans, task lists, subagents,
 *   requests and notices stand alone. An empty thought is dropped.
 * - A finished group reads as one sentence ("Ran 3 commands and read 2 files"), a live one by its
 *   latest step.
 * - Once a run has ended with a final message, everything before that message folds into
 *   "Worked for 2m 13s".
 */

import type { Node, Run, ToolKind } from "./agents-api";
import type { NodeTree } from "./agents-store";

export type TimelineItem = { kind: "node"; t: NodeTree } | { kind: "work"; id: string; entries: NodeTree[] };

/** Thinking and tool calls: the steps a group is made of. */
export function isStep(n: Node): boolean {
  return n.body.type === "tool" || n.body.type === "reasoning";
}

/** A run's top-level nodes, grouped. */
export function timelineItems(tree: readonly NodeTree[]): TimelineItem[] {
  const items: TimelineItem[] = [];
  for (const t of tree) {
    const b = t.node.body;
    if (b.type === "reasoning" && !b.text.trim() && t.node.status !== "running") continue;
    if (isStep(t.node)) {
      const last = items.at(-1);
      if (last?.kind === "work") last.entries.push(t);
      else items.push({ kind: "work", id: t.node.id, entries: [t] });
    } else items.push({ kind: "node", t });
  }
  return items;
}

/** A run that has ended (it can't change any more). */
export function ended(run: Pick<Run, "status">): boolean {
  return ["completed", "interrupted", "failed", "cancelled", "rolled_back"].includes(run.status);
}

export interface Turn {
  /** What folds away under "Worked for …" (`null`: nothing folds, show `rest` as it is). */
  fold: TimelineItem[] | null;
  rest: TimelineItem[];
}

/**
 * Fold an ended run's work above its final message. Only work folds (T3 folds work entries): a
 * proposed plan and a compaction stay where they are, around the fold.
 */
export function foldTurn(items: TimelineItem[], run: Pick<Run, "status">): Turn {
  const last = items.at(-1);
  const final = last?.kind === "node" && last.t.node.body.type === "message" && last.t.node.body.text.trim() !== "";
  if (!ended(run) || !final || items.length < 2) return { fold: null, rest: items };
  const stays = (i: TimelineItem) => i.kind === "node" && (i.t.node.body.type === "plan" || i.t.node.body.type === "compaction");
  const before = items.slice(0, -1);
  const fold = before.filter((i) => !stays(i));
  if (fold.length === 0) return { fold: null, rest: items };
  return { fold, rest: [...before.filter(stays), last] };
}

/** T3's fold label: "Worked for 2m 13s", "You stopped after 12s". */
export function foldLabel(run: Pick<Run, "status" | "started_at" | "completed_at">): string {
  const took = run.started_at !== null && run.completed_at !== null ? duration(run.completed_at - run.started_at) : null;
  if (run.status === "interrupted" || run.status === "cancelled") return took ? `You stopped after ${took}` : "You stopped this response";
  return took ? `Worked for ${took}` : "Worked";
}

export function duration(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}

type Action = "command" | "edit" | "read" | "code-search" | "search" | "other";

const actionOf: Record<ToolKind, Action> = { command: "command", edit: "edit", read: "read", search: "code-search", web: "search", mcp: "other", other: "other" };

/** T3's `summaryActionPriority`: what changed things first, then what looked, then the rest. */
const priority: Record<Action, number> = { command: 0, edit: 0, read: 1, "code-search": 1, search: 1, other: 2 };

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

function actionLabel(a: Action, n: number): string {
  switch (a) {
    case "read":
      return `Read ${plural(n, "file", "files")}`;
    case "edit":
      return `Changed ${plural(n, "file", "files")}`;
    case "command":
      return `Ran ${plural(n, "command", "commands")}`;
    case "code-search":
      return `Searched code ${plural(n, "time", "times")}`;
    case "search":
      return `Searched the web ${plural(n, "time", "times")}`;
    case "other":
      return `Used ${plural(n, "tool", "tools")}`;
  }
}

/** A failed step: the tool failed, or its command exited non-zero. */
export function stepFailed(n: Node): boolean {
  return n.status === "failed" || (n.body.type === "tool" && n.body.exit_code !== null && n.body.exit_code !== 0);
}

/**
 * One sentence for a group of steps (T3's `summarizeToolGroup`): at most two kinds of action,
 * what changed things first, then "performed N other actions". Edits count files changed.
 */
export function summarize(entries: readonly NodeTree[]): { summary: string; failed: boolean } {
  const tools = entries.filter((t) => t.node.body.type === "tool");
  if (entries.length > 0 && tools.length === 0) return { summary: entries.length === 1 ? "Thought" : `Thought (×${entries.length})`, failed: false };
  const groups = new Map<Action, Node[]>();
  for (const t of tools) {
    const b = t.node.body as Extract<Node["body"], { type: "tool" }>;
    const a = actionOf[b.kind];
    groups.set(a, [...(groups.get(a) ?? []), t.node]);
  }
  const all = [...groups].map(([a, nodes], index) => {
    let count = nodes.length;
    if (a === "edit") {
      const files = new Set<string>();
      let bare = 0;
      for (const n of nodes) {
        const fs = n.body.type === "tool" ? n.body.files : [];
        if (fs.length === 0) bare++;
        for (const f of fs) files.add(f.path);
      }
      count = files.size + bare;
    }
    return { a, index, steps: nodes.length, label: actionLabel(a, count) };
  });
  const picked = [...all]
    .sort((x, y) => priority[x.a] - priority[y.a] || x.index - y.index)
    .slice(0, 2)
    .sort((x, y) => x.index - y.index);
  const labels = picked.map((p) => p.label);
  const rest = tools.length - picked.reduce((n, p) => n + p.steps, 0);
  if (rest > 0) labels.push(`Performed ${plural(rest, "other action", "other actions")}`);
  const words = labels.map((l, i) => (i === 0 ? l : l.charAt(0).toLowerCase() + l.slice(1)));
  const summary = words.length < 3 ? words.join(" and ") : `${words.slice(0, -1).join(", ")}, and ${words.at(-1)}`;
  return { summary, failed: tools.some((t) => stepFailed(t.node)) };
}

/** The first line of a thought (T3's `liveThoughtLine`), without markdown emphasis. */
export function thoughtLine(text: string): string {
  const line = text.split("\n").find((l) => l.trim()) ?? "";
  return line.replace(/^#+\s*/, "").replace(/\*\*|__|`/g, "").trim();
}

/** What a step's row says. A thought is "Thinking" while it runs and "Thought" after (it expands). */
export function stepLabel(n: Node): string {
  const b = n.body;
  if (b.type === "reasoning") return n.status === "running" ? "Thinking" : "Thought";
  if (b.type !== "tool") return "";
  switch (b.kind) {
    case "read":
      return `Read ${b.title}`;
    case "edit":
      return b.files.length > 1 ? `Changed ${b.files.length} files` : b.title;
    case "search":
      return `Searched ${b.title}`;
    case "web":
      return `Searched the web for ${b.title}`;
    default:
      return b.title;
  }
}

/**
 * The steps that led to a node (a request): the last `n` steps before it in the run, depth
 * first (a subagent's steps count), and how many there were in all.
 */
export function stepsBefore(tree: readonly NodeTree[], node: string, n: number): { steps: Node[]; total: number } {
  const before: Node[] = [];
  let found = false;
  const walk = (ts: readonly NodeTree[]) => {
    for (const t of ts) {
      if (found) return;
      if (t.node.id === node) {
        found = true;
        return;
      }
      if (isStep(t.node) && !(t.node.body.type === "reasoning" && !t.node.body.text.trim())) before.push(t.node);
      walk(t.children);
    }
  };
  walk(tree);
  return { steps: before.slice(-n), total: before.length };
}

/** The tasks a delivering run carries, titled and with how they ended as the daemon's wake says it. */
export function delegatedTasks(run: Pick<Run, "prompt" | "delivers">): { id: string; title: string; status: string }[] {
  return (run.delivers ?? []).map((id) => {
    const head = `Delegated task ${id} (`;
    const at = run.prompt.indexOf(head);
    const said = at < 0 ? null : /^(.*?)\) \[(\w+)\]/.exec(run.prompt.slice(at + head.length));
    return { id, title: said?.[1] || "A task", status: said?.[2] ?? "completed" };
  });
}

/** "Delegated task finished · Tests", "2 delegated tasks finished · Build, Docs failed": what a delivering run is called. */
export function deliveredLabel(run: Pick<Run, "prompt" | "delivers">): string {
  const tasks = delegatedTasks(run);
  const head = tasks.length === 1 ? "Delegated task finished" : `${tasks.length} delegated tasks finished`;
  return `${head} · ${tasks.map((t) => (t.status === "completed" ? t.title : `${t.title} ${t.status}`)).join(", ")}`;
}
