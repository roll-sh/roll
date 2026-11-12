/**
 * The canvas graph (docs/code/agents.md, "Canvas"), derived from `AgentsData`: machines are
 * anchors, projects hang off machines, threads off projects, forks and delegated subagents off
 * their parent thread; provider-native subagents, pending requests and terminals are satellites of
 * their thread (or project, or machine). Pure; `agents-layout.ts` places it.
 */

import { isBusy, isRunning, type MachineId, type Node, type ProjectId, type ProviderId, type Request, type Run, type Thread, type ThreadChange } from "./agents-api";
import { pendingRequests, runsOf, waitingOnYou, type AgentsData } from "./agents-store";
import { colorToken, machineColor, projectColor } from "./agents-identity";

/**
 * `pin`: a card pinned onto a canvas view (`lib/agents-views.ts` adds them; `deriveGraph` never does).
 * `fold`: a project's quiet threads folded into one card (`foldQuiet`). `group`: a status header
 * inside a project's space (`groupByStatus`).
 */
export type GraphKind = "machine" | "project" | "group" | "thread" | "subagent" | "request" | "terminal" | "pin" | "fold";

export interface GraphNode {
  /** `m:` machine, `p:` project, `t:` thread, `s:` subagent node, `r:` request, `x:` terminal; `pc:`/`pd:`/`px:` a pinned conversation, diff, terminal. */
  key: string;
  kind: GraphKind;
  /** The record's id. */
  id: string;
  machine: MachineId;
  parent: string | null;
  children: string[];
  /** It's working right now (a running thread or subagent, a booting sandbox): it pulses, and so does the edge into it. Nothing else animates. */
  busy: boolean;
  /** Why the edge into it exists (forks draw dashed). */
  relation: "fork" | "subagent" | null;
  /**
   * The colour token of what it belongs to (its project's, else its machine's; docs/code/agents-design.md,
   * "Identity"): the edge into it is drawn in it. Set by `deriveGraph`.
   */
  tint?: string;
}

export interface GraphEdge {
  from: string;
  to: string;
  busy: boolean;
  dashed: boolean;
  /** The colour token it's drawn in when it isn't busy (its child's `tint`). */
  tint?: string;
}

export interface Graph {
  roots: string[];
  nodes: ReadonlyMap<string, GraphNode>;
  edges: GraphEdge[];
}

export interface GraphOptions {
  /** Threads open in place: their requests live inside the session, not beside it. */
  expanded?: ReadonlySet<string>;
  /** The thread whose conversation is open beside the canvas: its terminals are in its drawer, its requests in it. */
  docked?: string | null;
  showArchived?: boolean;
  /** Provider subagents shown per thread at most (the newest). */
  maxSubagents?: number;
}

export const keyOf = {
  machine: (id: string) => `m:${id}`,
  project: (id: string) => `p:${id}`,
  thread: (id: string) => `t:${id}`,
  subagent: (id: string) => `s:${id}`,
  request: (id: string) => `r:${id}`,
  terminal: (id: string) => `x:${id}`,
  /** A container's folded threads (a project's, or a status group's inside it). */
  fold: (container: string) => `f:${container}`,
};

export function deriveGraph(data: AgentsData, opts: GraphOptions = {}): Graph {
  const expanded = opts.expanded ?? new Set<string>();
  const maxSub = opts.maxSubagents ?? 6;
  const nodes = new Map<string, GraphNode>();
  const add = (n: Omit<GraphNode, "children">) => {
    const g = { ...n, children: [] };
    nodes.set(n.key, g);
    return g;
  };

  const machines = [...data.machines.values()].map((r) => r.value).sort((a, b) => a.name.localeCompare(b.name) || a.id.localeCompare(b.id));
  // A sandbox pulses while it boots or wakes.
  for (const m of machines) {
    const s = m.sandbox?.status;
    add({ key: keyOf.machine(m.id), kind: "machine", id: m.id, machine: m.id, parent: null, busy: s === "creating" || s === "starting", relation: null });
  }

  const projects = [...data.projects.values()].sort((a, b) => a.value.name.localeCompare(b.value.name) || a.value.id.localeCompare(b.value.id));
  for (const r of projects) {
    const parent = keyOf.machine(r.machine);
    if (!nodes.has(parent)) continue;
    add({ key: keyOf.project(r.value.id), kind: "project", id: r.value.id, machine: r.machine, parent, busy: false, relation: null });
  }

  // Threads: newest first under their parent, so fresh work sits at the top.
  // A delegated subagent's thread leaves once it's done (its result is in its parent's transcript).
  // Settled and snoozed threads are on the sidebar's shelves (docs/code/agents.md, "Settling"):
  // new activity brings one back, so a thread with a request is never among them.
  const visible = [...data.threads.values()]
    .filter((r) => opts.showArchived || r.value.archived_at === null)
    .filter((r) => r.value.lineage?.relation !== "subagent" || isBusy(r.value.status) || r.value.status === "error")
    .filter((r) => !shelved(r.value))
    .sort((a, b) => b.value.created_at - a.value.created_at || a.value.id.localeCompare(b.value.id));
  const shown = new Set(visible.map((r) => r.value.id));
  const parentOf = (t: Thread): string | null => {
    const p = t.lineage?.parent;
    if (p && shown.has(p) && p !== t.id) return keyOf.thread(p);
    return nodes.has(keyOf.project(t.project)) ? keyOf.project(t.project) : null;
  };
  // Parents before children so every parent key exists when a child is attached; a lineage cycle
  // (which the daemon never makes) falls back to the project.
  const placed = new Set<string>();
  const place = (r: (typeof visible)[number], depth = 0) => {
    const t = r.value;
    if (placed.has(t.id)) return;
    let parent = parentOf(t);
    if (parent?.startsWith("t:")) {
      const p = data.threads.get(parent.slice(2));
      if (p && depth < 64) place(p, depth + 1);
      if (!placed.has(parent.slice(2))) parent = nodes.has(keyOf.project(t.project)) ? keyOf.project(t.project) : null;
    }
    placed.add(t.id);
    if (!parent) return;
    add({
      key: keyOf.thread(t.id),
      kind: "thread",
      id: t.id,
      machine: r.machine,
      parent,
      busy: isRunning(t.status),
      relation: parent.startsWith("t:") ? (t.lineage?.relation ?? null) : null,
    });
  };
  for (const r of visible) place(r);

  // Provider-native subagents of each thread's current run (delegated roll threads are threads).
  for (const r of visible) {
    const t = r.value;
    const tk = keyOf.thread(t.id);
    if (!nodes.has(tk)) continue;
    const inner = data.nodes.get(t.id);
    if (!inner) continue;
    const runs = runsOf(data, t.id);
    const run = t.active_run ?? runs[runs.length - 1]?.id;
    if (!run) continue;
    const subs: Node[] = [];
    for (const n of inner.values()) {
      const b = n.value.body;
      // Only while they work: a finished subagent has nothing to show beside its thread.
      if (n.value.run !== run || b.type !== "subagent" || n.value.status !== "running") continue;
      if (b.child && shown.has(b.child)) continue;
      subs.push(n.value);
    }
    subs.sort((a, b) => a.ordinal - b.ordinal);
    const keep = new Set(subs.slice(-maxSub).map((n) => n.id));
    for (const n of subs) {
      if (!keep.has(n.id)) continue;
      // Nested subagents hang off the subagent that started them.
      let parent = tk;
      for (let p = n.parent; p; p = inner.get(p)?.value.parent ?? null) {
        if (keep.has(p)) {
          parent = keyOf.subagent(p);
          break;
        }
      }
      add({ key: keyOf.subagent(n.id), kind: "subagent", id: n.id, machine: r.machine, parent, busy: n.status === "running", relation: null });
    }
  }

  // Pending requests beside their thread; inside it when it's open in place or its conversation
  // is open beside the canvas (answered there: a second copy beside the card only crowded it).
  for (const q of pendingRequests(data)) {
    const tk = keyOf.thread(q.thread);
    if (!nodes.has(tk) || expanded.has(q.thread) || q.thread === opts.docked) continue;
    add({ key: keyOf.request(q.id), kind: "request", id: q.id, machine: nodes.get(tk)!.machine, parent: tk, busy: false, relation: null });
  }

  // Terminals: on their thread, else their project, else their machine (provider logins). One on
  // the machine that finished cleanly (a login that went through) has nothing left to show; a
  // failed one stays, with its output.
  const terminals = [...data.terminals.values()].sort((a, b) => a.value.created_at - b.value.created_at);
  for (const r of terminals) {
    const t = r.value;
    if (t.thread && t.thread === opts.docked) continue;
    if (!t.thread && !t.project && t.status === "exited" && t.exit_code === 0) continue;
    const parent = [t.thread && keyOf.thread(t.thread), t.project && keyOf.project(t.project), keyOf.machine(r.machine)].find(
      (k): k is string => !!k && nodes.has(k),
    );
    if (!parent) continue;
    add({ key: keyOf.terminal(t.id), kind: "terminal", id: t.id, machine: r.machine, parent, busy: false, relation: null });
  }

  // Each card's identity: a project's colour for it and everything under it, a machine's for the
  // machine and what hangs off it directly. Parents come first in the map, so one pass does it.
  for (const n of nodes.values()) {
    if (n.kind === "machine") {
      const m = data.machines.get(n.id)?.value;
      if (m) n.tint = colorToken(machineColor(m));
    } else if (n.kind === "project") {
      const p = data.projects.get(n.id)?.value;
      if (p) n.tint = colorToken(projectColor(p));
    } else if (n.kind === "thread") {
      const p = data.projects.get(data.threads.get(n.id)?.value.project ?? "")?.value;
      n.tint = p ? colorToken(projectColor(p)) : n.parent ? nodes.get(n.parent)?.tint : undefined;
    } else if (n.parent) {
      n.tint = nodes.get(n.parent)?.tint;
    }
  }

  // Children in insertion order (already sorted per kind).
  const roots: string[] = [];
  for (const n of nodes.values()) {
    if (n.parent) nodes.get(n.parent)!.children.push(n.key);
    else roots.push(n.key);
  }

  const edges: GraphEdge[] = [];
  for (const n of nodes.values()) {
    if (!n.parent) continue;
    edges.push({ from: n.parent, to: n.key, busy: n.busy, dashed: n.relation === "fork", tint: n.tint });
  }
  return { roots, nodes, edges };
}

const sameList = <T>(a: readonly T[], b: readonly T[], eq: (x: T, y: T) => boolean = Object.is) => a.length === b.length && a.every((x, i) => eq(x, b[i]));
const sameNode = (a: GraphNode, b: GraphNode) =>
  a.key === b.key && a.kind === b.kind && a.id === b.id && a.machine === b.machine && a.parent === b.parent && a.busy === b.busy && a.relation === b.relation && a.tint === b.tint && sameList(a.children, b.children);
const sameEdge = (a: GraphEdge, b: GraphEdge) => a.from === b.from && a.to === b.to && a.busy === b.busy && a.dashed === b.dashed && a.tint === b.tint;

/**
 * `prev` when `next` has the same shape (nodes, children, edges, what pulses), else `next` with
 * `prev`'s node for each node that didn't change. Most streamed records change what a card says,
 * not the graph: kept, the canvas's layout and its slots don't render again for them (each card
 * follows its own record), and when one thread starts or stops only its card renders.
 */
export function sameGraph(prev: Graph | null, next: Graph): Graph {
  if (!prev || prev === next) return next;
  let kept = prev.nodes.size === next.nodes.size;
  const nodes = new Map<string, GraphNode>();
  for (const [k, n] of next.nodes) {
    const p = prev.nodes.get(k);
    if (p && sameNode(p, n)) nodes.set(k, p);
    else {
      nodes.set(k, n);
      kept = false;
    }
  }
  if (kept && sameList(prev.roots, next.roots) && sameList(prev.edges, next.edges, sameEdge)) return prev;
  return { ...next, nodes };
}

/**
 * Forks, delegated subagents and a provider's own subagents shown inside the thread they came
 * from (a row on its card), not as cards of their own: as cards beside or under it they broke a
 * project's grid and read as more of its threads. What hung off them (a request waiting on a
 * subagent, its terminal) goes on the host's card instead, so it still waits in plain sight.
 */
export interface Absorbed {
  graph: Graph;
  /** Per host thread's key, the children shown inside its card (oldest first). */
  inside: ReadonlyMap<string, readonly string[]>;
  /** Per absorbed key, the card it shows in. */
  host: ReadonlyMap<string, string>;
}

const NOTHING_INSIDE: ReadonlyMap<string, readonly string[]> = new Map();

export function absorbChildren(graph: Graph): Absorbed {
  const into = (n: GraphNode) => (n.kind === "subagent" || (n.kind === "thread" && graph.nodes.get(n.parent ?? "")?.kind === "thread") || (n.kind === "thread" && graph.nodes.get(n.parent ?? "")?.kind === "subagent"));
  const host = new Map<string, string>();
  const hostOf = (n: GraphNode): string => {
    const known = host.get(n.key);
    if (known) return known;
    let p = graph.nodes.get(n.parent ?? "");
    for (let i = 0; p && into(p) && i < 64; i++) p = graph.nodes.get(p.parent ?? "");
    return p?.key ?? n.key;
  };
  for (const n of graph.nodes.values()) if (into(n)) host.set(n.key, hostOf(n));
  if (!host.size) return { graph, inside: NOTHING_INSIDE, host };
  const inside = new Map<string, string[]>();
  for (const [k, h] of host) inside.set(h, [...(inside.get(h) ?? []), k]);
  const nodes = new Map<string, GraphNode>();
  for (const n of graph.nodes.values()) {
    if (host.has(n.key)) continue;
    const parent = n.parent && host.has(n.parent) ? host.get(n.parent)! : n.parent;
    const mine = inside.get(n.key);
    const busy = n.busy || !!mine?.some((k) => graph.nodes.get(k)?.busy);
    nodes.set(n.key, parent === n.parent && busy === n.busy ? { ...n, children: [] } : { ...n, parent, busy, relation: parent === n.parent ? n.relation : null, children: [] });
  }
  for (const n of nodes.values()) if (n.parent) nodes.get(n.parent)?.children.push(n.key);
  const edges: GraphEdge[] = [];
  for (const n of nodes.values()) if (n.parent && nodes.has(n.parent)) edges.push({ from: n.parent, to: n.key, busy: n.busy, dashed: false, tint: n.tint });
  return { graph: { roots: graph.roots.filter((r) => nodes.has(r)), nodes, edges }, inside, host };
}

/** Settled, or snoozed (the daemon wakes it): off the canvas, on the sidebar's shelves. */
export function shelved(t: Thread): boolean {
  return t.inbox.settle?.state === "settled" || t.inbox.snooze !== null;
}

export interface Collapsed {
  graph: Graph;
  /** Per machine id, the projects it lists instead of drawing them (name order). */
  unused: ReadonlyMap<MachineId, readonly ProjectId[]>;
}

/**
 * The canvas without the projects that have nothing on it (docs/code/agents.md, "Canvas"): a
 * project card under a machine with no card under it (no thread, terminal or pin in this view,
 * after the display's filters) isn't drawn; its machine lists it instead. `keep` says which stay
 * anyway (pinned, focused, named by the view's filter). Projects as the anchors (grouped by
 * project) aren't under a machine, so they always stay.
 */
export function collapseUnused(graph: Graph, keep: (project: ProjectId) => boolean): Collapsed {
  const drop = new Set<string>();
  const unused = new Map<MachineId, ProjectId[]>();
  for (const n of graph.nodes.values()) {
    if (n.kind !== "project" || n.children.length > 0 || keep(n.id)) continue;
    const parent = n.parent ? graph.nodes.get(n.parent) : undefined;
    if (parent?.kind !== "machine") continue;
    drop.add(n.key);
    const list = unused.get(parent.id);
    if (list) list.push(n.id);
    else unused.set(parent.id, [n.id]);
  }
  if (!drop.size) return { graph, unused };
  const nodes = new Map<string, GraphNode>();
  for (const n of graph.nodes.values()) {
    if (drop.has(n.key)) continue;
    nodes.set(n.key, n.children.some((c) => drop.has(c)) ? { ...n, children: n.children.filter((c) => !drop.has(c)) } : n);
  }
  return { graph: { roots: graph.roots, nodes, edges: graph.edges.filter((e) => !drop.has(e.to)) }, unused };
}

/** How many of a project's threads the overview shows before it folds the quiet ones (`foldQuiet`). */
export const SHOWN_THREADS = 4;
/** Inside a project, how many done or quiet threads a status group shows before it folds the rest. */
export const SHOWN_IN_GROUP = 12;
/** Folding fewer than this many would trade a card or two for a card: they show. */
const FOLD_AT_LEAST = 3;

export interface Fold {
  /** The threads that aren't drawn (ids, most recently active first). */
  threads: readonly string[];
  /** Shown anyway (the container was expanded): the fold card, last, folds them back. */
  expanded: boolean;
}

export interface Folds {
  graph: Graph;
  /**
   * Per container key (a project's in the overview, a status group's inside a project), what it
   * folded. Each has a `fold` card (`f:<container>`) last among its children: "22 more threads".
   */
  folded: ReadonlyMap<string, Fold>;
}

export interface FoldOptions {
  /** How many threads a node shows before its quiet ones fold; `null`: it never folds. */
  shown: (node: GraphNode) => number | null;
  /** A thread that always keeps its card: it's working, waiting on you or failed, open, or asked for. */
  keep: (thread: string) => boolean;
  /** When a thread was last active (newest first among the quiet ones). */
  activity: (thread: string) => number;
  /** Containers whose threads are all shown. */
  expanded?: ReadonlySet<string>;
}

/**
 * A busy project stays readable (docs/code/agents.md, "Canvas"): per container, every thread that
 * matters now keeps its card (working, waiting on you, failed, open beside the canvas, pinned, or
 * with any of that under it), the most recently active of the rest fill up to `shown` cards, and
 * the others fold into one card after them. 2 threads or 200, a project is a few cards, the way
 * unused projects fold into their machine's "3 more projects" (`collapseUnused`).
 */
export function foldQuiet(graph: Graph, opts: FoldOptions): Folds {
  const folded = new Map<string, Fold>();
  // Whether anything in a thread's subtree has to show (a request, a pin, a working subagent or fork).
  const matters = (key: string): boolean => {
    const n = graph.nodes.get(key);
    if (!n) return false;
    if (n.kind === "request" || n.kind === "pin" || n.busy) return true;
    if (n.kind === "thread" && opts.keep(n.id)) return true;
    return n.children.some(matters);
  };
  const drop = new Set<string>();
  const add: GraphNode[] = [];
  for (const p of graph.nodes.values()) {
    const limit = p.kind === "project" || p.kind === "group" ? opts.shown(p) : null;
    if (limit === null) continue;
    const threads = p.children.filter((c) => graph.nodes.get(c)?.kind === "thread");
    if (threads.length < limit + FOLD_AT_LEAST) continue;
    const quiet = threads.filter((k) => !matters(k));
    const room = Math.max(0, limit - (threads.length - quiet.length));
    const last = new Map(quiet.map((k) => [k, opts.activity(graph.nodes.get(k)!.id)]));
    const rest = [...quiet].sort((a, b) => last.get(b)! - last.get(a)! || a.localeCompare(b)).slice(room);
    if (rest.length < FOLD_AT_LEAST) continue;
    const expanded = !!opts.expanded?.has(p.key);
    folded.set(p.key, { threads: rest.map((k) => graph.nodes.get(k)!.id), expanded });
    if (!expanded) for (const k of rest) dropTree(graph, k, drop);
    add.push({ key: keyOf.fold(p.key), kind: "fold", id: p.key, machine: p.machine, parent: p.key, children: [], busy: false, relation: null, tint: p.tint });
  }
  if (!add.length) return { graph, folded };
  const nodes = new Map<string, GraphNode>();
  for (const n of graph.nodes.values()) {
    if (drop.has(n.key)) continue;
    const fold = folded.has(n.key) ? keyOf.fold(n.key) : null;
    nodes.set(n.key, fold || n.children.some((c) => drop.has(c)) ? { ...n, children: [...n.children.filter((c) => !drop.has(c)), ...(fold ? [fold] : [])] } : n);
  }
  for (const f of add) nodes.set(f.key, f);
  const edges = graph.edges.filter((e) => !drop.has(e.to));
  for (const f of add) edges.push({ from: f.parent!, to: f.key, busy: false, dashed: false, tint: f.tint });
  return { graph: { roots: graph.roots, nodes, edges }, folded };
}

/** Where a thread stands, for grouping a project's threads (in this order). */
export type StatusGroup = "needs" | "running" | "failed" | "done" | "quiet";
export const STATUS_GROUPS: readonly StatusGroup[] = ["needs", "running", "failed", "done", "quiet"];
export const statusGroupLabel: Record<StatusGroup, string> = { needs: "Needs you", running: "Running", failed: "Failed", done: "Done", quiet: "Quiet" };

/**
 * A project's space (drilled into it): its threads under one header per status that has any,
 * Needs you · Running · Failed · Done · Quiet, like Linear's board columns. A thread's own subtree
 * goes with it; what isn't a thread (terminals, pins) stays on the project. Group nodes are
 * `g:<project>:<status>`, with the status as their id.
 */
export function groupByStatus(graph: Graph, project: string, groupOf: (thread: string) => StatusGroup): Graph {
  const p = graph.nodes.get(project);
  if (p?.kind !== "project") return graph;
  const byGroup = new Map<StatusGroup, string[]>();
  const rest: string[] = [];
  for (const c of p.children) {
    const n = graph.nodes.get(c);
    if (n?.kind !== "thread") {
      rest.push(c);
      continue;
    }
    const g = groupOf(n.id);
    byGroup.set(g, [...(byGroup.get(g) ?? []), c]);
  }
  if (!byGroup.size) return graph;
  const nodes = new Map(graph.nodes);
  const moved = new Set([...byGroup.values()].flat());
  const edges = graph.edges.filter((e) => !(e.from === project && moved.has(e.to)));
  const groups: string[] = [];
  for (const g of STATUS_GROUPS) {
    const kids = byGroup.get(g);
    if (!kids) continue;
    const key = `g:${p.id}:${g}`;
    groups.push(key);
    nodes.set(key, { key, kind: "group", id: g, machine: p.machine, parent: project, children: kids, busy: g === "running", relation: null, tint: p.tint });
    for (const k of kids) {
      nodes.set(k, { ...graph.nodes.get(k)!, parent: key });
      const old = graph.edges.find((e) => e.from === project && e.to === k);
      edges.push({ ...(old ?? { busy: false, dashed: false }), from: key, to: k, tint: p.tint });
    }
    edges.push({ from: project, to: key, busy: false, dashed: false, tint: p.tint });
  }
  nodes.set(project, { ...p, children: [...groups, ...rest] });
  return { roots: graph.roots, nodes, edges };
}

function dropTree(graph: Graph, key: string, into: Set<string>) {
  into.add(key);
  for (const c of graph.nodes.get(key)?.children ?? []) dropTree(graph, c, into);
}

/** A finished run nobody has opened yet (thread-status.tsx's `threadDone`, without its import). */
function unseenDone(t: Thread, run: Run | undefined): boolean {
  return t.status === "idle" && !t.error && run?.status === "completed" && t.archived_at === null && (t.inbox?.visited_at == null || (run.completed_at ?? 0) > t.inbox.visited_at);
}

/** Each thread's newest run, in one pass over the runs. */
export function newestRuns(data: AgentsData): Map<string, Run> {
  const out = new Map<string, Run>();
  for (const r of data.runs.values()) {
    const had = out.get(r.value.thread);
    if (!had || r.value.ordinal > had.ordinal) out.set(r.value.thread, r.value);
  }
  return out;
}

/** Everything the "rolling" popover lists: busy threads and the requests waiting on someone. */
export interface Rolling {
  busy: Thread[];
  requests: Request[];
}

export function rolling(data: AgentsData): Rolling {
  const busy: Thread[] = [];
  for (const r of data.threads.values()) if (isBusy(r.value.status) && r.value.archived_at === null) busy.push(r.value);
  busy.sort((a, b) => b.updated_at - a.updated_at);
  return { busy, requests: pendingRequests(data) };
}

/** What a project's card says about it (like T3's project stats): its work, its PRs, its agents. */
export interface ProjectStats {
  threads: number;
  running: number;
  waiting: number;
  /** Failed (the thread's status is `error`). */
  failed: number;
  /** A finished run nobody has opened yet (T3's completed tone). */
  done: number;
  /** The roll code changes (PRs) its threads were published as, newest thread first. */
  changes: ThreadChange[];
  /** The agents working in it (by provider, with the models used), most threads first. */
  agents: { provider: ProviderId; models: (string | null)[]; threads: number }[];
  /** Spent across its threads, when the providers say (Claude does). */
  cost_usd: number | null;
}

export function projectStats(data: AgentsData, project: ProjectId): ProjectStats {
  const out: ProjectStats = { threads: 0, running: 0, waiting: 0, failed: 0, done: 0, changes: [], agents: [], cost_usd: null };
  const newest = newestRuns(data);
  const agents = new Map<string, ProjectStats["agents"][number]>();
  // What the canvas has of it: not what's on the shelves, nor a delegated subagent that's done (it left).
  const threads = [...data.threads.values()]
    .map((r) => r.value)
    .filter((t) => t.project === project && t.archived_at === null && !shelved(t) && !(t.lineage?.relation === "subagent" && !isBusy(t.status) && t.status !== "error"));
  threads.sort((a, b) => b.updated_at - a.updated_at);
  const waiting = waitingOnYou(data);
  for (const t of threads) {
    out.threads++;
    if (waiting.has(t.id)) out.waiting++;
    else if (isBusy(t.status)) out.running++;
    else if (t.status === "error") out.failed++;
    else if (unseenDone(t, newest.get(t.id))) out.done++;
    if (t.change) out.changes.push(t.change);
    const a = agents.get(t.provider) ?? { provider: t.provider, models: [], threads: 0 };
    a.threads++;
    if (!a.models.includes(t.model)) a.models.push(t.model);
    agents.set(t.provider, a);
    if (t.usage.cost_usd != null) out.cost_usd = (out.cost_usd ?? 0) + t.usage.cost_usd;
  }
  out.agents = [...agents.values()].sort((a, b) => b.threads - a.threads);
  return out;
}
