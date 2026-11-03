/**
 * The canvas graph (docs/code/agents.md, "Canvas"), derived from `AgentsData`: machines are
 * anchors, projects hang off machines, threads off projects, forks and delegated subagents off
 * their parent thread; provider-native subagents, pending requests and terminals are satellites of
 * their thread (or project, or machine). Pure; `agents-layout.ts` places it.
 */

import { isBusy, type MachineId, type Node, type Request, type Thread } from "./agents-api";
import { pendingRequests, runsOf, type AgentsData } from "./agents-store";

export type GraphKind = "machine" | "project" | "thread" | "subagent" | "request" | "terminal";

export interface GraphNode {
  /** `m:` machine, `p:` project, `t:` thread, `s:` subagent node, `r:` request, `x:` terminal. */
  key: string;
  kind: GraphKind;
  /** The record's id. */
  id: string;
  machine: MachineId;
  parent: string | null;
  children: string[];
  /** Work is flowing through it: it pulses, and so does the edge into it. */
  busy: boolean;
  /** Why the edge into it exists (forks draw dashed). */
  relation: "fork" | "subagent" | null;
}

export interface GraphEdge {
  from: string;
  to: string;
  busy: boolean;
  dashed: boolean;
}

export interface Graph {
  roots: string[];
  nodes: ReadonlyMap<string, GraphNode>;
  edges: GraphEdge[];
}

export interface GraphOptions {
  /** Threads open in place: their requests live inside the session, not beside it. */
  expanded?: ReadonlySet<string>;
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
  // A sandbox pulses while it boots.
  for (const m of machines) add({ key: keyOf.machine(m.id), kind: "machine", id: m.id, machine: m.id, parent: null, busy: m.sandbox?.status === "creating", relation: null });

  const projects = [...data.projects.values()].sort((a, b) => a.value.name.localeCompare(b.value.name) || a.value.id.localeCompare(b.value.id));
  for (const r of projects) {
    const parent = keyOf.machine(r.machine);
    if (!nodes.has(parent)) continue;
    add({ key: keyOf.project(r.value.id), kind: "project", id: r.value.id, machine: r.machine, parent, busy: false, relation: null });
  }

  // Threads: newest first under their parent, so fresh work sits at the top.
  const visible = [...data.threads.values()]
    .filter((r) => opts.showArchived || r.value.archived_at === null)
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
      busy: isBusy(t.status),
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
      if (n.value.run !== run || b.type !== "subagent") continue;
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
    add({ key: keyOf.request(q.id), kind: "request", id: q.id, machine: nodes.get(tk)!.machine, parent: tk, busy: true, relation: null });
  }

  // Terminals: on their thread, else their project, else their machine (provider logins).
  const terminals = [...data.terminals.values()].sort((a, b) => a.value.created_at - b.value.created_at);
  for (const r of terminals) {
    const t = r.value;
    const parent = [t.thread && keyOf.thread(t.thread), t.project && keyOf.project(t.project), keyOf.machine(r.machine)].find(
      (k): k is string => !!k && nodes.has(k),
    );
    if (!parent) continue;
    add({ key: keyOf.terminal(t.id), kind: "terminal", id: t.id, machine: r.machine, parent, busy: false, relation: null });
  }

  // Children in insertion order (already sorted per kind); busy flows up to the anchors.
  const roots: string[] = [];
  for (const n of nodes.values()) {
    if (n.parent) nodes.get(n.parent)!.children.push(n.key);
    else roots.push(n.key);
  }
  const rollUp = (key: string): boolean => {
    const n = nodes.get(key)!;
    let any = n.busy;
    for (const c of n.children) any = rollUp(c) || any;
    if (n.kind === "machine" || n.kind === "project") n.busy = any;
    return any;
  };
  roots.forEach(rollUp);

  const edges: GraphEdge[] = [];
  for (const n of nodes.values()) {
    if (!n.parent) continue;
    edges.push({ from: n.parent, to: n.key, busy: n.busy, dashed: n.relation === "fork" });
  }
  return { roots, nodes, edges };
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
