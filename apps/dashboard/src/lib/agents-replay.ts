/**
 * Replay of a shared thread (docs/code/agents.md, "Shares"): a share is a frozen copy of a thread's
 * records, and each record says when it happened (a thread's `created_at`, a run's
 * `requested_at`/`started_at`/`completed_at`, a node's `started_at`/`completed_at`, a request's
 * `created_at`/`resolved_at`, a checkpoint's `created_at`). `frameAt` rebuilds what the canvas
 * showed at any moment from those times alone: records appear when they happened, running nodes
 * hold the part of their text that had streamed (in proportion to how far through they were),
 * runs and threads are busy (so the canvas pulses) until they finish, and requests wait until
 * they were answered. At the end the frame is the share itself.
 *
 * Long pauses (a person thinking before approving, an hour between two runs) are shortened to
 * `MAX_GAP` on the replay clock, so a replay runs about as long as there was something to see.
 * Pure; `components/agents/share-view.tsx` plays it.
 */

import {
  ZERO_USAGE,
  addUsage,
  isRunTerminal,
  type Entity,
  type Machine,
  type Mirrored,
  type Node,
  type Request,
  type Run,
  type Share,
  type Thread,
  type ThreadStatus,
} from "./agents-api";
import { applySnapshot, emptyData, type AgentsData } from "./agents-store";

/** The shared machine's id on the replay canvas (shares don't carry the real one). */
export const SHARE_MACHINE = "mch_share";

/** The longest a pause lasts on the replay clock (ms). */
export const MAX_GAP = 1500;

export interface Timeline {
  /** Real times (ms since the epoch) at which something happened, ascending, each once. */
  real: number[];
  /** Where each of them is on the replay clock (ms from the start). */
  at: number[];
  /** Length of the replay (ms at 1×). */
  duration: number;
}

const tsOf = (e: Entity): number[] => {
  switch (e.kind) {
    case "thread":
      return [e.created_at];
    case "run":
      return [e.requested_at, e.started_at, e.completed_at].filter((t): t is number => t != null);
    case "node":
      return [e.started_at, e.completed_at].filter((t): t is number => t != null);
    case "request":
      return [e.created_at, e.resolved_at].filter((t): t is number => t != null);
    case "checkpoint":
      return [e.created_at];
    default:
      return [];
  }
};

export function timeline(share: Share): Timeline {
  const all = new Set<number>();
  for (const e of share.entities) for (const t of tsOf(e)) if (t > 0) all.add(t);
  const real = [...all].sort((a, b) => a - b);
  if (real.length === 0) return { real: [share.created_at], at: [0], duration: 0 };
  const at = [0];
  for (let i = 1; i < real.length; i++) at.push(at[i - 1] + Math.min(real[i] - real[i - 1], MAX_GAP));
  return { real, at, duration: at[at.length - 1] };
}

/** Binary search: the last index with `xs[i] <= v` (or -1). */
function floor(xs: number[], v: number): number {
  let lo = 0;
  let hi = xs.length - 1;
  let ans = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (xs[mid] <= v) {
      ans = mid;
      lo = mid + 1;
    } else hi = mid - 1;
  }
  return ans;
}

/** Map between the clocks: linear inside each (possibly shortened) gap. */
function convert(from: number[], to: number[], v: number): number {
  if (v <= from[0]) return to[0];
  const last = from.length - 1;
  if (v >= from[last]) return to[last];
  const i = floor(from, v);
  const span = from[i + 1] - from[i];
  return to[i] + (span ? ((v - from[i]) / span) * (to[i + 1] - to[i]) : 0);
}

/** The real time at `ms` on the replay clock. */
export const realAt = (tl: Timeline, ms: number) => convert(tl.at, tl.real, ms);
/** Where a real moment is on the replay clock. */
export const replayAt = (tl: Timeline, real: number) => convert(tl.real, tl.at, real);

/** The next (or previous) moment something happened, on the replay clock: for stepping. */
export function step(tl: Timeline, ms: number, dir: 1 | -1): number {
  if (dir > 0) return tl.at.find((a) => a > ms + 0.5) ?? tl.duration;
  for (let i = tl.at.length - 1; i >= 0; i--) if (tl.at[i] < ms - 0.5) return tl.at[i];
  return 0;
}

/** The machine a share's records hang off on the canvas. */
export function shareMachine(share: Share): Machine {
  return {
    id: SHARE_MACHINE,
    name: share.machine.name,
    online: false,
    info: { hostname: share.machine.name, os: share.machine.os ?? "", arch: "", version: "", home: "~", providers: [] },
    sandbox: null,
    created_at: share.created_at,
    last_seen: null,
  };
}

export interface FrameOptions {
  /** Running text grows as it streamed (off with reduced motion: text appears whole). */
  stream?: boolean;
}

/** `s` as far as it had streamed: a fraction `f` of it, ending at a whole character. */
function partial(s: string, f: number): string {
  if (f >= 1) return s;
  const chars = Array.from(s);
  return chars.slice(0, Math.max(0, Math.floor(chars.length * f))).join("");
}

function nodeAt(n: Node, t: number, pending: ReadonlySet<string>, stream: boolean): Node | null {
  if (n.started_at > t) return null;
  if (n.completed_at == null || n.completed_at <= t) return n;
  const f = stream ? (t - n.started_at) / Math.max(1, n.completed_at - n.started_at) : 1;
  const b = n.body;
  const waiting = b.type === "request" ? pending.has(b.request) : false;
  const status = waiting || n.status === "waiting" ? "waiting" : "running";
  const base = { ...n, status, completed_at: null } as Node;
  switch (b.type) {
    case "message":
    case "reasoning":
    case "plan":
      return { ...base, body: { ...b, text: partial(b.text, f) } };
    case "tool":
      return { ...base, body: { ...b, output: b.output == null ? null : partial(b.output, f), exit_code: null } };
    case "subagent":
      return { ...base, body: { ...b, result: null } };
    default:
      return base;
  }
}

function requestAt(r: Request, t: number): Request | null {
  if (r.created_at > t) return null;
  if (r.resolved_at != null && r.resolved_at <= t) return r;
  if (r.status === "pending") return r;
  return { ...r, status: "pending", resolved_at: null, resolved_by: null, response: null };
}

function runAt(r: Run, t: number, waiting: boolean): Run | null {
  if (r.requested_at > t) return null;
  if (r.completed_at != null && r.completed_at <= t) return r;
  if (isRunTerminal(r.status) && r.completed_at == null) return r;
  const status = waiting ? "waiting" : r.started_at != null && r.started_at <= t ? "running" : "starting";
  return { ...r, status, completed_at: null, checkpoint: null, stat: null, usage: { ...ZERO_USAGE }, error: null };
}

function threadAt(th: Thread, runs: Run[], pending: number, activity: string | null, root: boolean, t: number): Thread | null {
  if (!root && th.created_at > t) return null;
  const active = runs.find((r) => !isRunTerminal(r.status));
  const last = [...runs].sort((a, b) => b.ordinal - a.ordinal)[0];
  const status: ThreadStatus = active ? (pending > 0 ? "waiting" : active.status === "starting" ? "starting" : "running") : last?.status === "failed" ? "error" : "idle";
  let usage = { ...ZERO_USAGE };
  for (const r of runs) usage = addUsage(usage, r.usage);
  return {
    ...th,
    status,
    active_run: active?.id ?? null,
    run_count: runs.length,
    pending_requests: pending,
    activity: active ? activity : null,
    usage,
    error: status === "error" ? (last?.error ?? th.error) : null,
    queued: [],
  };
}

/**
 * The canvas at real time `t`. Records the share holds but that hadn't happened yet are left out;
 * at or after the last moment, every record is as the share froze it.
 */
export function frameAt(share: Share, tl: Timeline, t: number, opts: FrameOptions = {}): AgentsData {
  const stream = opts.stream ?? true;
  const end = tl.real[tl.real.length - 1];
  const out: Entity[] = [];
  if (t >= end) {
    out.push(...share.entities);
  } else {
    const requests: Request[] = [];
    const pending = new Set<string>();
    for (const e of share.entities) {
      if (e.kind !== "request") continue;
      const { kind: _k, ...r } = e;
      const at = requestAt(r, t);
      if (!at) continue;
      requests.push(at);
      if (at.status === "pending") pending.add(at.id);
    }
    const waitingRuns = new Set(requests.filter((r) => r.status === "pending").map((r) => r.run));
    const runsByThread = new Map<string, Run[]>();
    const nodes: Node[] = [];
    for (const e of share.entities) {
      if (e.kind === "run") {
        const { kind: _k, ...r } = e;
        const at = runAt(r, t, waitingRuns.has(r.id));
        if (!at) continue;
        out.push({ kind: "run", ...at });
        runsByThread.set(at.thread, [...(runsByThread.get(at.thread) ?? []), at]);
      } else if (e.kind === "node") {
        const { kind: _k, ...n } = e;
        const at = nodeAt(n, t, pending, stream);
        if (!at) continue;
        nodes.push(at);
        out.push({ kind: "node", ...at });
      } else if (e.kind === "checkpoint") {
        if (e.created_at <= t) out.push(e);
      } else if (e.kind === "project") {
        out.push(e);
      }
    }
    for (const r of requests) out.push({ kind: "request", ...r });
    for (const e of share.entities) {
      if (e.kind !== "thread") continue;
      const { kind: _k, ...th } = e;
      const runs = runsByThread.get(th.id) ?? [];
      const running = nodes.filter((n) => n.thread === th.id && n.status === "running").sort((a, b) => b.started_at - a.started_at)[0];
      const activity = running?.body.type === "tool" ? running.body.title : null;
      const pendingHere = requests.filter((r) => r.thread === th.id && r.status === "pending").length;
      const at = threadAt(th, runs, pendingHere, activity, th.id === share.thread, t);
      if (at) out.push({ kind: "thread", ...at });
    }
  }
  const entities: Mirrored[] = out.map((entity, i) => ({ cursor: i + 1, machine: SHARE_MACHINE, entity }));
  const threads = new Set(out.filter((e) => e.kind === "thread").map((e) => e.id));
  const data = applySnapshot(emptyData(), { cursor: entities.length + 1, machines: [shareMachine(share)], entities }, threads);
  // Every thread's detail is here, even before its first node: "no runs yet", not "loading".
  const nodes = new Map(data.nodes);
  for (const t of threads) if (!nodes.has(t)) nodes.set(t, new Map());
  return { ...data, nodes };
}

/** How many things happened in a share: what its replay and link preview count. */
export function shareStats(share: Share) {
  let runs = 0;
  let steps = 0;
  let threads = 0;
  let files = 0;
  const providers = new Set<string>();
  for (const e of share.entities) {
    if (e.kind === "run") runs++;
    else if (e.kind === "node") steps++;
    else if (e.kind === "thread") {
      threads++;
      providers.add(e.provider);
    } else if (e.kind === "checkpoint") files += e.stat.files;
  }
  return { runs, steps, threads, files, providers: [...providers].sort() };
}
