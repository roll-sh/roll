/**
 * The agents canvas state: the relay's snapshot plus its stream (docs/code/agents.md, "Sequences,
 * sync and resume"), as plain immutable data. Pure functions only; `agents-client.ts` feeds them
 * from the network (or the demo) and React reads the result.
 *
 * - Every record is keyed by kind and id; the newest cursor wins, so replays, overlapping
 *   snapshots and a stream resumed from an older cursor are all idempotent.
 * - A tombstone (`removed`) is remembered with its cursor, so an older upsert can't bring the
 *   record back.
 * - Deltas are ephemeral: they extend a streaming node's text (or a tool's output) until the
 *   durable node catches up, which the daemon re-sends at least every 500 ms.
 * - Detail records (nodes, checkpoints) are kept only for the threads this client follows.
 */

import {
  isBusy,
  type Checkpoint,
  type Delta,
  type Entity,
  type EntityKind,
  type Machine,
  type MachineId,
  type Mirrored,
  type Node,
  type NodeId,
  type Project,
  type Request,
  type Run,
  type State,
  type StreamEvent,
  type Terminal,
  type Thread,
  type ThreadId,
} from "./agents-api";

export interface Rec<T> {
  cursor: number;
  machine: MachineId;
  value: T;
}

/** Text a node has streamed beyond its durable record. */
export interface Live {
  thread: ThreadId;
  field: "text" | "output";
  text: string;
}

export interface AgentsData {
  /** The newest cursor seen (resume the stream after it). */
  cursor: number;
  machines: ReadonlyMap<MachineId, Rec<Machine>>;
  projects: ReadonlyMap<string, Rec<Project>>;
  threads: ReadonlyMap<string, Rec<Thread>>;
  runs: ReadonlyMap<string, Rec<Run>>;
  requests: ReadonlyMap<string, Rec<Request>>;
  terminals: ReadonlyMap<string, Rec<Terminal>>;
  /** Detail, per followed thread. */
  nodes: ReadonlyMap<ThreadId, ReadonlyMap<NodeId, Rec<Node>>>;
  checkpoints: ReadonlyMap<ThreadId, ReadonlyMap<string, Rec<Checkpoint>>>;
  live: ReadonlyMap<NodeId, Live>;
  /** `kind:id` (or `machine:id`) → the cursor it was removed at. */
  tombs: ReadonlyMap<string, number>;
  /** Threads whose detail this data holds. */
  detail: ReadonlySet<ThreadId>;
  /**
   * The newest canvas save the stream announced (`null`: none since the page opened). A canvas
   * showing an older revision loads it again (`AgentsServices.canvas.load`).
   */
  canvas: { revision: number; updated_at: number } | null;
}

export const emptyData = (): AgentsData => ({
  cursor: 0,
  machines: new Map(),
  projects: new Map(),
  threads: new Map(),
  runs: new Map(),
  requests: new Map(),
  terminals: new Map(),
  nodes: new Map(),
  checkpoints: new Map(),
  live: new Map(),
  tombs: new Map(),
  detail: new Set(),
  canvas: null,
});

/** Tombstones kept at most; the oldest go first (only a very late stale upsert could need one). */
const MAX_TOMBS = 20_000;

type ShellKind = "projects" | "threads" | "runs" | "requests" | "terminals";
const shellKey: Record<Exclude<EntityKind, "node" | "checkpoint">, ShellKind> = {
  project: "projects",
  thread: "threads",
  run: "runs",
  request: "requests",
  terminal: "terminals",
};

/** Copy-on-write over one `AgentsData`: each collection is cloned at most once per batch. */
class Draft {
  private cloned = new Set<string>();
  private d: { -readonly [K in keyof AgentsData]: AgentsData[K] };

  constructor(base: AgentsData) {
    this.d = { ...base };
  }

  get data(): AgentsData {
    return this.d;
  }

  private own<K extends "machines" | ShellKind | "live" | "tombs">(k: K): Map<string, never> {
    if (!this.cloned.has(k)) {
      this.d[k] = new Map(this.d[k] as Map<string, never>) as never;
      this.cloned.add(k);
    }
    return this.d[k] as unknown as Map<string, never>;
  }

  private ownDetail<K extends "nodes" | "checkpoints">(k: K, thread: ThreadId): Map<string, Rec<never>> {
    if (!this.cloned.has(k)) {
      this.d[k] = new Map(this.d[k] as ReadonlyMap<string, unknown>) as never;
      this.cloned.add(k);
    }
    const outer = this.d[k] as Map<ThreadId, Map<string, Rec<never>>>;
    const tag = `${k}:${thread}`;
    if (!this.cloned.has(tag)) {
      outer.set(thread, new Map(outer.get(thread) ?? []));
      this.cloned.add(tag);
    }
    return outer.get(thread)!;
  }

  cursor(c: number) {
    if (c > this.d.cursor) this.d.cursor = c;
  }

  /** Revisions only go up: a late or replayed announcement never takes the canvas back. */
  canvas(revision: number, updated_at: number) {
    if (revision > (this.d.canvas?.revision ?? 0)) this.d.canvas = { revision, updated_at };
  }

  machine(cursor: number, m: Machine) {
    const tomb = this.d.tombs.get(`machine:${m.id}`);
    if (tomb !== undefined && tomb >= cursor) return;
    const cur = this.d.machines.get(m.id);
    if (cur && cur.cursor > cursor) return;
    this.own("machines").set(m.id, { cursor, machine: m.id, value: m } as never);
  }

  machineRemoved(cursor: number, id: MachineId) {
    const cur = this.d.machines.get(id);
    if (cur && cur.cursor > cursor) return;
    this.tomb(`machine:${id}`, cursor);
    this.own("machines").delete(id);
    // Its records go with it: the relay drops a revoked machine's mirror.
    for (const k of Object.values(shellKey)) {
      for (const [rid, r] of this.d[k]) if (r.machine === id) this.own(k).delete(rid);
    }
    for (const k of ["nodes", "checkpoints"] as const) {
      for (const [t, inner] of this.d[k]) {
        for (const [rid, r] of inner) if (r.machine === id) this.ownDetail(k, t).delete(rid);
      }
    }
  }

  private tomb(key: string, cursor: number) {
    const tombs = this.own("tombs") as Map<string, number>;
    tombs.delete(key);
    tombs.set(key, cursor);
    if (tombs.size > MAX_TOMBS) tombs.delete(tombs.keys().next().value!);
  }

  entity(m: Mirrored) {
    const e = m.entity;
    if (e.kind === "removed") return this.removed(m.cursor, m.machine, e);
    const key = `${e.kind}:${e.id}`;
    const tomb = this.d.tombs.get(key);
    if (tomb !== undefined && tomb >= m.cursor) return;
    if (e.kind === "node" || e.kind === "checkpoint") {
      const thread = e.thread;
      if (!this.d.detail.has(thread)) return;
      const k = e.kind === "node" ? "nodes" : "checkpoints";
      const cur = this.d[k].get(thread)?.get(e.id);
      if (cur && cur.cursor > m.cursor) return;
      const { kind: _kind, ...value } = e;
      this.ownDetail(k, thread).set(e.id, { cursor: m.cursor, machine: m.machine, value } as never);
      if (e.kind === "node") this.settleLive(value as Node);
      return;
    }
    const k = shellKey[e.kind];
    const cur = this.d[k].get(e.id);
    if (cur && cur.cursor > m.cursor) return;
    const { kind: _kind, ...value } = e;
    this.own(k).set(e.id, { cursor: m.cursor, machine: m.machine, value } as never);
  }

  private removed(cursor: number, _machine: MachineId, r: Extract<Entity, { kind: "removed" }>) {
    const key = `${r.entity}:${r.id}`;
    this.tomb(key, cursor);
    if (r.entity === "node" || r.entity === "checkpoint") {
      const k = r.entity === "node" ? "nodes" : "checkpoints";
      const threads = r.thread ? [r.thread] : [...this.d[k].keys()];
      for (const t of threads) {
        const cur = this.d[k].get(t)?.get(r.id);
        if (cur && cur.cursor <= cursor) this.ownDetail(k, t).delete(r.id);
      }
      if (r.entity === "node" && this.d.live.has(r.id)) this.own("live").delete(r.id);
      return;
    }
    const k = shellKey[r.entity];
    const cur = this.d[k].get(r.id);
    if (cur && cur.cursor <= cursor) this.own(k).delete(r.id);
    if (r.entity === "thread") this.dropDetail(r.id);
  }

  /** The durable node caught up with (or finished) what deltas streamed: show the record again. */
  private settleLive(n: Node) {
    const live = this.d.live.get(n.id);
    if (!live) return;
    const durable = durableText(n, live.field);
    const streaming = n.status === "running" || n.status === "waiting";
    if (!streaming || durable === null || durable.length >= live.text.length) this.own("live").delete(n.id);
  }

  delta(d: Delta) {
    if (d.type === "terminal") return;
    if (!this.d.detail.has(d.thread)) return;
    const field = d.type === "text" ? "text" : "output";
    const live = this.d.live.get(d.node);
    let base = live?.field === field ? live.text : null;
    if (base === null) {
      const node = this.d.nodes.get(d.thread)?.get(d.node)?.value;
      if (node && node.status !== "running" && node.status !== "waiting") return;
      base = (node && durableText(node, field)) ?? "";
    }
    this.own("live").set(d.node, { thread: d.thread, field, text: base + d.text } as never);
  }

  follow(threads: Iterable<ThreadId>) {
    const next = new Set(threads);
    for (const t of this.d.detail) if (!next.has(t)) this.dropDetail(t);
    this.d.detail = next;
  }

  dropDetail(thread: ThreadId) {
    for (const k of ["nodes", "checkpoints"] as const) {
      if (this.d[k].has(thread)) {
        if (!this.cloned.has(k)) {
          this.d[k] = new Map(this.d[k] as ReadonlyMap<string, unknown>) as never;
          this.cloned.add(k);
        }
        (this.d[k] as Map<ThreadId, unknown>).delete(thread);
        this.cloned.delete(`${k}:${thread}`);
      }
    }
    for (const [id, l] of this.d.live) if (l.thread === thread) this.own("live").delete(id);
  }

  /**
   * Merge a snapshot taken at `state.cursor`. Records it has win by cursor; records it lacks that
   * are no newer than it were removed before it was taken.
   */
  snapshot(state: State, detail: ReadonlySet<ThreadId>) {
    const seen = new Set<string>();
    for (const m of state.machines) {
      seen.add(`machine:${m.id}`);
      this.machine(state.cursor, m);
    }
    for (const m of state.entities) {
      if (m.entity.kind !== "removed") seen.add(`${m.entity.kind}:${m.entity.id}`);
      this.entity(m);
    }
    for (const [id, r] of this.d.machines) {
      if (!seen.has(`machine:${id}`) && r.cursor <= state.cursor) this.own("machines").delete(id);
    }
    for (const [kind, k] of Object.entries(shellKey) as [EntityKind, ShellKind][]) {
      for (const [id, r] of this.d[k]) if (!seen.has(`${kind}:${id}`) && r.cursor <= state.cursor) this.own(k).delete(id);
    }
    for (const [kind, k] of [
      ["node", "nodes"],
      ["checkpoint", "checkpoints"],
    ] as const) {
      for (const [t, inner] of this.d[k]) {
        if (!detail.has(t)) continue;
        for (const [id, r] of inner) if (!seen.has(`${kind}:${id}`) && r.cursor <= state.cursor) this.ownDetail(k, t).delete(id);
      }
    }
    this.cursor(state.cursor);
  }
}

function durableText(n: Node, field: Live["field"]): string | null {
  const b = n.body;
  if (field === "output") return b.type === "tool" ? (b.output ?? "") : null;
  return b.type === "message" || b.type === "reasoning" || b.type === "plan" ? b.text : null;
}

/** Apply stream events in order. Returns `data` itself when nothing changed. */
export function applyEvents(data: AgentsData, events: readonly StreamEvent[]): AgentsData {
  if (!events.length) return data;
  const d = new Draft(data);
  for (const ev of events) {
    switch (ev.type) {
      case "entity":
        d.entity(ev);
        d.cursor(ev.cursor);
        break;
      case "machine":
        d.machine(ev.cursor, ev.machine);
        d.cursor(ev.cursor);
        break;
      case "machine_removed":
        d.machineRemoved(ev.cursor, ev.machine);
        d.cursor(ev.cursor);
        break;
      case "delta":
        d.delta(ev.delta);
        break;
      case "canvas":
        d.canvas(ev.revision, ev.updated_at);
        d.cursor(ev.cursor);
        break;
    }
  }
  return d.data;
}

/**
 * Merge a snapshot that holds the detail of `detail` (the threads followed when it was fetched).
 * The followed set becomes `detail`; detail of threads no longer followed is dropped.
 */
export function applySnapshot(data: AgentsData, state: State, detail: ReadonlySet<ThreadId> = data.detail): AgentsData {
  const d = new Draft(data);
  d.follow(detail);
  d.snapshot(state, detail);
  return d.data;
}

/** Follow a different set of threads (drops detail of the ones left). */
export function follow(data: AgentsData, threads: Iterable<ThreadId>): AgentsData {
  const d = new Draft(data);
  d.follow(threads);
  return d.data;
}

// ── reading ──

/** A node as the UI shows it: with any text streamed past its durable record. */
export function liveNode(data: AgentsData, n: Node): Node {
  const live = data.live.get(n.id);
  if (!live) return n;
  const b = n.body;
  if (live.field === "output" && b.type === "tool") return { ...n, body: { ...b, output: live.text } };
  if (live.field === "text" && (b.type === "message" || b.type === "reasoning" || b.type === "plan"))
    return { ...n, body: { ...b, text: live.text } };
  return n;
}

/** Is this node's text arriving right now (render a caret)? */
export const isStreaming = (data: AgentsData, n: Node) => n.status === "running" && (data.live.has(n.id) || n.body.type === "message");

export const machineOf = (data: AgentsData, thread: ThreadId): MachineId | undefined => data.threads.get(thread)?.machine;

/** A thread's runs, oldest first. */
export function runsOf(data: AgentsData, thread: ThreadId): Run[] {
  const out: Run[] = [];
  for (const r of data.runs.values()) if (r.value.thread === thread) out.push(r.value);
  return out.sort((a, b) => a.ordinal - b.ordinal);
}

/** A run's nodes in order, each with its children (subagent work), streamed text applied. */
export interface NodeTree {
  node: Node;
  children: NodeTree[];
}

export function nodeTree(data: AgentsData, thread: ThreadId, run: string): NodeTree[] {
  const inner = data.nodes.get(thread);
  if (!inner) return [];
  const nodes = [...inner.values()].map((r) => liveNode(data, r.value)).filter((n) => n.run === run);
  nodes.sort((a, b) => a.ordinal - b.ordinal);
  const byId = new Map<string, NodeTree>(nodes.map((n) => [n.id, { node: n, children: [] }]));
  const roots: NodeTree[] = [];
  for (const n of nodes) {
    const t = byId.get(n.id)!;
    const parent = n.parent ? byId.get(n.parent) : undefined;
    (parent ? parent.children : roots).push(t);
  }
  return roots;
}

export function checkpointsOf(data: AgentsData, thread: ThreadId): Checkpoint[] {
  return [...(data.checkpoints.get(thread)?.values() ?? [])].map((r) => r.value).sort((a, b) => a.ordinal - b.ordinal);
}

export function pendingRequests(data: AgentsData, thread?: ThreadId): Request[] {
  const out: Request[] = [];
  for (const r of data.requests.values()) {
    if (r.value.status === "pending" && (!thread || r.value.thread === thread)) out.push(r.value);
  }
  return out.sort((a, b) => a.created_at - b.created_at);
}

export function busyThreads(data: AgentsData): Thread[] {
  const out: Thread[] = [];
  for (const r of data.threads.values()) if (isBusy(r.value.status) && r.value.archived_at === null) out.push(r.value);
  return out.sort((a, b) => b.updated_at - a.updated_at);
}

/** The providers a thread's machine offers. */
export function providersFor(data: AgentsData, machine: MachineId | undefined) {
  return (machine && data.machines.get(machine)?.value.info?.providers) || [];
}
