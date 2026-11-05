/**
 * The canvas graph (docs/code/agents.md, "Canvas"), derived from `AgentsData`: machines are
 * anchors, projects hang off machines, threads off projects, forks and delegated subagents off
 * their parent thread; provider-native subagents, pending requests and terminals are satellites of
 * their thread (or project, or machine). Pure; `agents-layout.ts` places it.
 */

import { isBusy, isRunning, type MachineId, type Node, type ProjectId, type ProviderId, type Request, type Thread, type ThreadChange } from "./agents-api";
import { pendingRequests, runsOf, type AgentsData } from "./agents-store";
import { colorToken, machineColor, projectColor } from "./agents-identity";

/** `pin`: a card pinned onto a canvas view (`lib/agents-views.ts` adds them; `deriveGraph` never does). */
export type GraphKind = "machine" | "project" | "thread" | "subagent" | "request" | "terminal" | "pin";

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
  /** The thread whose conversation is open beside the canvas: its terminals are in its drawer. */
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

  // Pending requests beside their thread (inside it when it's open).
  for (const q of pendingRequests(data)) {
    const tk = keyOf.thread(q.thread);
    if (!nodes.has(tk) || expanded.has(q.thread)) continue;
    add({ key: keyOf.request(q.id), kind: "request", id: q.id, machine: nodes.get(tk)!.machine, parent: tk, busy: false, relation: null });
  }

  // Terminals: on their thread, else their project, else their machine (provider logins).
  const terminals = [...data.terminals.values()].sort((a, b) => a.value.created_at - b.value.created_at);
  for (const r of terminals) {
    const t = r.value;
    if (t.thread && t.thread === opts.docked) continue;
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
  /** The roll code changes (PRs) its threads were published as, newest thread first. */
  changes: ThreadChange[];
  /** The agents working in it (by provider, with the models used), most threads first. */
  agents: { provider: ProviderId; models: (string | null)[]; threads: number }[];
  /** Spent across its threads, when the providers say (Claude does). */
  cost_usd: number | null;
}

export function projectStats(data: AgentsData, project: ProjectId): ProjectStats {
  const out: ProjectStats = { threads: 0, running: 0, waiting: 0, changes: [], agents: [], cost_usd: null };
  const agents = new Map<string, ProjectStats["agents"][number]>();
  const threads = [...data.threads.values()].map((r) => r.value).filter((t) => t.project === project && t.archived_at === null);
  threads.sort((a, b) => b.updated_at - a.updated_at);
  for (const t of threads) {
    out.threads++;
    if (isRunning(t.status)) out.running++;
    else if (t.status === "waiting") out.waiting++;
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
