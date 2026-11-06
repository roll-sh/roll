/**
 * The threads as a collection (docs/code/agents.md, "Working at scale"): the rows the list and the
 * board show, how they're grouped (Linear's grouping and sub-grouping), how they're sorted, and the
 * board's columns. The canvas shows the same threads (`deriveGraph`'s rules: archived only when
 * asked for, settled and snoozed ones on the shelves, a finished delegated agent gone), narrowed by
 * the same view filter and focus, so a filter applies the same way in every layout. Pure.
 */

import { isBusy, type MachineId, type Run, type Thread, type ThreadId } from "./agents-api";
import { shelved } from "./agents-graph";
import { providerLabel } from "./agents-replay";
import type { AgentsData } from "./agents-store";
import { GROUP_BYS, SORT_KEYS, threadMatches, type GroupBy, type Sort, type SortKey, type ViewFilter } from "./agents-views";

export { GROUP_BYS, SORT_KEYS, type GroupBy, type Sort, type SortKey };

// ── what a thread is doing, in one word (the board's columns) ──

/** Needs you · Running · Queued · Failed · Done (ready-to-land changes lead Done). */
export type Bucket = "needs" | "running" | "queued" | "failed" | "done";
export const BUCKETS: readonly Bucket[] = ["needs", "running", "queued", "failed", "done"];
export const bucketLabel: Record<Bucket, string> = { needs: "Needs you", running: "Running", queued: "Queued", failed: "Failed", done: "Done" };

export function bucketOf(t: Thread): Bucket {
  if (t.pending_requests > 0 || t.status === "waiting" || t.plan_ready) return "needs";
  if (isBusy(t.status)) return "running";
  if (t.queued.length > 0) return "queued";
  if (t.status === "error" || t.error) return "failed";
  return "done";
}

/** Published and landable: it leads Done ("Ready to land"). */
export const readyToLand = (t: Thread) => t.change?.status === "ready";

// ── rows ──

export interface ThreadRow {
  id: ThreadId;
  thread: Thread;
  machine: MachineId;
  bucket: Bucket;
  /** The newest run this page has (its diff stat is the card's "+12 −3"). */
  last: Run | undefined;
  /** When anything last happened on it. */
  activity: number;
  tokens: number;
}

export interface RowsOptions {
  now: number;
  showArchived: boolean;
  filter: ViewFilter;
  /** "Focus on this project only". */
  focused: string | null;
}

/** The newest run of every thread, in one pass (per thread it was a scan of every run). */
export function lastRuns(data: AgentsData): Map<ThreadId, Run> {
  const out = new Map<ThreadId, Run>();
  for (const r of data.runs.values()) {
    const had = out.get(r.value.thread);
    if (!had || r.value.ordinal > had.ordinal) out.set(r.value.thread, r.value);
  }
  return out;
}

/** Whether the canvas shows the thread at all (before any view): `deriveGraph`'s rules. */
export function listed(t: Thread, showArchived: boolean): boolean {
  if (!showArchived && t.archived_at !== null) return false;
  if (t.lineage?.relation === "subagent" && !isBusy(t.status) && t.status !== "error") return false;
  return !shelved(t);
}

/** The rows a view picks, in no particular order. */
export function rowsOf(data: AgentsData, opts: RowsOptions, last = lastRuns(data)): ThreadRow[] {
  const out: ThreadRow[] = [];
  for (const r of data.threads.values()) {
    const t = r.value;
    if (!listed(t, opts.showArchived)) continue;
    if (opts.focused && t.project !== opts.focused) continue;
    if (!threadMatches(data, t, r.machine, opts.filter, opts.now)) continue;
    out.push({ id: t.id, thread: t, machine: r.machine, bucket: bucketOf(t), last: last.get(t.id), activity: t.updated_at, tokens: t.usage.input_tokens + t.usage.output_tokens });
  }
  return out;
}

// ── sorting ──

export const sortLabel: Record<SortKey, string> = {
  activity: "Last activity",
  created: "Created",
  title: "Title",
  status: "Status",
  project: "Project",
  machine: "Machine",
  agent: "Agent",
  changes: "Changes",
  cost: "Cost",
};

export const DEFAULT_SORT: Sort = { key: "activity", dir: "desc" };

/** Which way a column sorts first when picked: names A→Z, numbers and times biggest first. */
export const firstDir = (key: SortKey): Sort["dir"] => (key === "title" || key === "project" || key === "machine" || key === "agent" || key === "status" ? "asc" : "desc");

/** Names the rows need: projects and machines by id. */
export interface Names {
  project: (id: string) => string;
  machine: (id: MachineId) => string;
}

export function namesOf(data: AgentsData): Names {
  return {
    project: (id) => data.projects.get(id)?.value.name ?? id,
    machine: (id) => data.machines.get(id)?.value.name ?? id,
  };
}

const changesOf = (r: ThreadRow) => (r.last?.stat && r.last.status !== "rolled_back" ? r.last.stat.additions + r.last.stat.deletions : 0);
const bucketRank = (r: ThreadRow) => BUCKETS.indexOf(r.bucket) * 2 + (r.bucket === "done" && readyToLand(r.thread) ? 0 : 1);

/** Rows in the order a sort asks for; ties fall back to the newest activity, then the id, so the order never flickers. */
export function sortRows(rows: readonly ThreadRow[], sort: Sort, names: Names): ThreadRow[] {
  const sign = sort.dir === "asc" ? 1 : -1;
  const text = (a: string, b: string) => a.localeCompare(b, undefined, { sensitivity: "base", numeric: true });
  const by: (a: ThreadRow, b: ThreadRow) => number = (() => {
    switch (sort.key) {
      case "activity":
        return (a, b) => a.activity - b.activity;
      case "created":
        return (a, b) => a.thread.created_at - b.thread.created_at;
      case "title":
        return (a, b) => text(a.thread.title, b.thread.title);
      case "status":
        return (a, b) => bucketRank(a) - bucketRank(b);
      case "project":
        return (a, b) => text(names.project(a.thread.project), names.project(b.thread.project));
      case "machine":
        return (a, b) => text(names.machine(a.machine), names.machine(b.machine));
      case "agent":
        return (a, b) => text(agentOf(a.thread), agentOf(b.thread));
      case "changes":
        return (a, b) => changesOf(a) - changesOf(b);
      case "cost":
        return (a, b) => (a.thread.usage.cost_usd ?? 0) - (b.thread.usage.cost_usd ?? 0) || a.tokens - b.tokens;
    }
  })();
  return [...rows].sort((a, b) => sign * by(a, b) || b.activity - a.activity || a.id.localeCompare(b.id));
}

const agentOf = (t: Thread) => `${providerLabel(t.provider)}${t.model ? ` · ${t.model}` : ""}`;

// ── grouping ──

export const groupLabel: Record<GroupBy, string> = { none: "No grouping", status: "Status", project: "Project", machine: "Machine", agent: "Agent", pr: "PR" };

/** A change's state as a group: no PR first, then the states in the order they happen. */
const PR_ORDER = ["none", "checking", "failing", "conflict", "blocked", "ready", "queued", "landed", "closed"] as const;
const prLabel: Record<(typeof PR_ORDER)[number], string> = {
  none: "No PR",
  checking: "Checking",
  failing: "Failing",
  conflict: "Conflict",
  blocked: "Blocked",
  ready: "Ready to land",
  queued: "In the queue",
  landed: "Landed",
  closed: "Closed",
};

export interface Group {
  /** Unique among its siblings; `parent/child` for a sub-group. */
  key: string;
  by: GroupBy;
  label: string;
  /** Threads in it, its sub-groups' included. */
  count: number;
  rows: ThreadRow[];
  /** When sub-grouped. */
  groups?: Group[];
  /** What it's about: a project's or machine's id, a bucket, a provider. */
  value: string;
}

function groupKey(r: ThreadRow, by: Exclude<GroupBy, "none">): string {
  switch (by) {
    case "status":
      return r.bucket;
    case "project":
      return r.thread.project;
    case "machine":
      return r.machine;
    case "agent":
      return r.thread.provider;
    case "pr":
      return r.thread.change?.status ?? "none";
  }
}

function labelOf(by: Exclude<GroupBy, "none">, value: string, names: Names): string {
  switch (by) {
    case "status":
      return bucketLabel[value as Bucket] ?? value;
    case "project":
      return names.project(value);
    case "machine":
      return names.machine(value);
    case "agent":
      return providerLabel(value);
    case "pr":
      return prLabel[value as keyof typeof prLabel] ?? value;
  }
}

/** Groups in their natural order: status and PR state as they happen, the rest by name. */
function orderGroups(by: Exclude<GroupBy, "none">, groups: Group[]): Group[] {
  if (by === "status") return groups.sort((a, b) => BUCKETS.indexOf(a.value as Bucket) - BUCKETS.indexOf(b.value as Bucket));
  if (by === "pr") return groups.sort((a, b) => PR_ORDER.indexOf(a.value as (typeof PR_ORDER)[number]) - PR_ORDER.indexOf(b.value as (typeof PR_ORDER)[number]));
  return groups.sort((a, b) => a.label.localeCompare(b.label, undefined, { sensitivity: "base", numeric: true }) || a.value.localeCompare(b.value));
}

/**
 * Sorted rows split into groups (and each group into sub-groups), keeping the sort inside each.
 * By status, every column is there even when empty (a board's columns don't come and go); other
 * groupings show only groups with threads. `none` is one group holding everything.
 */
export function groupRows(rows: readonly ThreadRow[], by: GroupBy, sub: GroupBy, names: Names, parent = ""): Group[] {
  if (by === "none") {
    const all: Group = { key: `${parent}all`, by, label: "All threads", value: "", count: rows.length, rows: [...rows] };
    if (sub !== "none") all.groups = groupRows(rows, sub, "none", names, `${parent}all/`).filter((s) => s.count > 0);
    return [all];
  }
  const map = new Map<string, ThreadRow[]>();
  if (by === "status") for (const b of BUCKETS) map.set(b, []);
  for (const r of rows) {
    const k = groupKey(r, by);
    const list = map.get(k);
    if (list) list.push(r);
    else map.set(k, [r]);
  }
  const groups = orderGroups(
    by,
    [...map].map(([value, list]) => ({ key: `${parent}${by}:${value}`, by, value, label: labelOf(by, value, names), count: list.length, rows: list })),
  );
  if (sub !== "none" && sub !== by) for (const g of groups) g.groups = groupRows(g.rows, sub, "none", names, `${g.key}/`).filter((s) => s.count > 0);
  return groups;
}

// ── the list, flattened (what the virtual list draws) ──

export type Item = { kind: "group"; group: Group; depth: number; collapsed: boolean } | { kind: "row"; row: ThreadRow; depth: number; group: string };

/**
 * One line per group header and per row, in reading order; a collapsed group keeps its header and
 * hides what's in it. The one group of `none` draws no header. Empty groups (a status no thread
 * has) are left out of the list, unlike the board's columns.
 */
export function flatten(groups: readonly Group[], collapsed: ReadonlySet<string>): Item[] {
  const out: Item[] = [];
  const walk = (list: readonly Group[], depth: number) => {
    for (const g of list) {
      if (g.count === 0) continue;
      const header = g.by !== "none";
      const shut = header && collapsed.has(g.key);
      if (header) out.push({ kind: "group", group: g, depth, collapsed: shut });
      if (shut) continue;
      const inner = header ? depth + 1 : depth;
      if (g.groups?.length) walk(g.groups, inner);
      else for (const row of g.rows) out.push({ kind: "row", row, depth: inner, group: g.key });
    }
  };
  walk(groups, 0);
  return out;
}

/** The threads in the order they're drawn (Shift-click ranges, j/k). */
export const orderOf = (items: readonly Item[]): ThreadId[] => items.flatMap((i) => (i.kind === "row" ? [i.row.id] : []));

/** Every thread in a group, its sub-groups' too (a group's "select all"). */
export const idsOf = (g: Group): ThreadId[] => g.rows.map((r) => r.id);

// ── "next that needs me" ──

/**
 * The threads waiting on you, the longest waiting first (the oldest pending request, else when it
 * last changed): the order "Next that needs me" walks in every layout.
 */
export function needsMe(data: AgentsData, rows: readonly ThreadRow[]): ThreadId[] {
  const since = new Map<string, number>();
  for (const r of data.requests.values()) {
    if (r.value.status !== "pending") continue;
    const at = since.get(r.value.thread);
    if (at === undefined || r.value.created_at < at) since.set(r.value.thread, r.value.created_at);
  }
  return rows
    .filter((r) => r.bucket === "needs")
    .map((r) => [r.id, since.get(r.id) ?? r.thread.updated_at] as const)
    .sort((a, b) => a[1] - b[1] || a[0].localeCompare(b[0]))
    .map(([id]) => id);
}

/** The one after `current` in `order`, round to the start; the first when `current` isn't in it. */
export function nextOf(order: readonly ThreadId[], current: ThreadId | null): ThreadId | null {
  if (!order.length) return null;
  const i = current ? order.indexOf(current) : -1;
  return order[(i + 1) % order.length];
}
