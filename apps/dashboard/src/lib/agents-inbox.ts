/**
 * The threads sidebar's rules (docs/code/agents.md, "Settling"), ported from T3 Code's
 * `Sidebar.logic.ts`, `threadSettled.ts`, `threadActionMenu.logic.ts` and `Sidebar.tsx`
 * (`planForwardNavigation`): which shelf a thread is on, how each shelf is ordered, what a row
 * says, where "settle and advance" goes, and the snooze presets. Pure; the daemon decides what is
 * settled, this only reads `Thread.inbox`.
 */

import type { MachineId, Project, Run, Thread, ThreadId } from "./agents-api";
import type { AgentsData } from "./agents-store";

export type Shelf = "active" | "snoozed" | "settled";

/** Snoozed first, then settled, else active (T3's section order). */
export function shelfOf(t: Thread, now: number): Shelf {
  if (t.inbox.snooze && t.inbox.snooze.until > now) return "snoozed";
  if (t.inbox.settle?.state === "settled") return "settled";
  return "active";
}

/** What a row says about its thread, most urgent first (T3's row status line). */
export type RowStatus = "working" | "waiting" | "failed" | "woke" | "done";

export const rowStatusLabel: Record<RowStatus, string> = {
  working: "Working",
  waiting: "Needs you",
  failed: "Failed",
  woke: "Woke",
  done: "Done",
};

/** `lastEnded`: when its newest run ended. "Woke" and "Done" last until it's visited after. */
export function rowStatus(t: Thread, lastEnded: number | null): RowStatus | null {
  if (t.status === "running" || t.status === "starting") return "working";
  if (t.status === "waiting" || t.pending_requests > 0) return "waiting";
  if (t.status === "error") return "failed";
  const seen = t.inbox.visited_at ?? 0;
  if (t.inbox.woke_at !== null && seen < t.inbox.woke_at) return "woke";
  if (lastEnded !== null && seen < lastEnded) return "done";
  return null;
}

/** When the thread last came back to you (T3's active order): made, a run asked for or ended, un-settled, woken. */
export function returnedAt(t: Thread, runs: readonly { requested_at: number; completed_at: number | null }[]): number {
  let at = t.created_at;
  for (const r of runs) at = Math.max(at, r.requested_at, r.completed_at ?? 0);
  if (t.inbox.settle?.state === "active") at = Math.max(at, t.inbox.settle.at);
  return Math.max(at, t.inbox.woke_at ?? 0);
}

export interface InboxRow {
  thread: Thread;
  shelf: Shelf;
  status: RowStatus | null;
  /** The time the row shows: when it came back, settled, or wakes. */
  at: number;
}

export interface ProjectInbox {
  project: Project;
  machine: MachineId;
  active: InboxRow[];
  snoozed: InboxRow[];
  settled: InboxRow[];
}

/**
 * Each project's threads on their shelves: active newest first, snoozed soonest to wake first,
 * settled most recently settled first. Archived threads and delegated subagents (their results
 * are in their parent's transcript) aren't listed.
 */
export function inbox(data: AgentsData, now: number): ProjectInbox[] {
  const byProject = new Map<string, ProjectInbox>();
  const projects = [...data.projects.values()].sort((a, b) => a.value.name.localeCompare(b.value.name) || a.value.id.localeCompare(b.value.id));
  for (const r of projects) byProject.set(r.value.id, { project: r.value, machine: r.machine, active: [], snoozed: [], settled: [] });
  const runsBy = new Map<ThreadId, Run[]>();
  for (const r of data.runs.values()) {
    const list = runsBy.get(r.value.thread);
    if (list) list.push(r.value);
    else runsBy.set(r.value.thread, [r.value]);
  }
  for (const r of data.threads.values()) {
    const t = r.value;
    const group = byProject.get(t.project);
    if (!group || t.archived_at !== null || t.lineage?.relation === "subagent") continue;
    const runs = runsBy.get(t.id) ?? [];
    const lastEnded = runs.reduce<number | null>((m, run) => (run.completed_at === null ? m : Math.max(m ?? 0, run.completed_at)), null);
    const shelf = shelfOf(t, now);
    const at = shelf === "snoozed" ? t.inbox.snooze!.until : shelf === "settled" ? t.inbox.settle!.at : returnedAt(t, runs);
    group[shelf].push({ thread: t, shelf, status: rowStatus(t, lastEnded), at });
  }
  const byId = (a: InboxRow, b: InboxRow) => a.thread.id.localeCompare(b.thread.id);
  for (const g of byProject.values()) {
    g.active.sort((a, b) => b.at - a.at || byId(a, b));
    g.snoozed.sort((a, b) => a.at - b.at || byId(a, b));
    g.settled.sort((a, b) => b.at - a.at || byId(a, b));
  }
  return [...byProject.values()];
}

/** Every listed thread in the sidebar's order: per project, active, then snoozed, then settled. */
export function inboxOrder(groups: readonly ProjectInbox[]): InboxRow[] {
  return groups.flatMap((g) => [...g.active, ...g.snoozed, ...g.settled]);
}

/**
 * Where "settle and advance" goes after `current` (T3's `planForwardNavigation`): the next row
 * below it in the sidebar's order, wrapping to the top, skipping settled and snoozed rows. `null`:
 * none is left (back to the canvas).
 */
export function nextAfter(groups: readonly ProjectInbox[], current: ThreadId): ThreadId | null {
  const order = inboxOrder(groups);
  const i = order.findIndex((r) => r.thread.id === current);
  const rotated = i < 0 ? order : [...order.slice(i + 1), ...order.slice(0, i)];
  return rotated.find((r) => r.shelf === "active" && r.thread.id !== current)?.thread.id ?? null;
}

/** The settled shelf shows this many rows first, then this many more per "Show more" (T3's). */
export const SETTLED_FIRST = 10;
export const SETTLED_PAGE = 25;

// ── snooze ──

export interface SnoozePreset {
  /** "In 1 hour", "This evening", "Tomorrow", "Next week". */
  label: string;
  /** When it wakes, short ("15:42", "Thu 09:00"). */
  when: string;
  until: number;
}

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

/** At `h:00` on the day `days` after `now`'s, by the calendar (so a DST change doesn't shift it). */
function at(now: Date, days: number, h: number): Date {
  return new Date(now.getFullYear(), now.getMonth(), now.getDate() + days, h, 0, 0, 0);
}

function whenLabel(d: Date, now: Date): string {
  const time = d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
  const sameDay = d.toDateString() === now.toDateString();
  return sameDay ? time : `${d.toLocaleDateString(undefined, { weekday: "short" })} ${time}`;
}

/**
 * T3's default snooze presets: in 1 hour, in 3 hours, today at 18:00 (hidden once it's past),
 * tomorrow at 09:00, next Monday at 09:00; soonest first, one per minute.
 */
export function snoozePresets(nowMs: number): SnoozePreset[] {
  const now = new Date(nowMs);
  const evening = at(now, 0, 18);
  const monday = at(now, ((8 - now.getDay()) % 7 || 7), 9);
  const out: SnoozePreset[] = [];
  const push = (label: string, d: Date) => {
    if (d.getTime() <= nowMs) return;
    const minute = Math.floor(d.getTime() / 60_000);
    if (out.some((p) => Math.floor(p.until / 60_000) === minute)) return;
    out.push({ label, when: whenLabel(d, now), until: d.getTime() });
  };
  push("In 1 hour", new Date(nowMs + HOUR));
  push("In 3 hours", new Date(nowMs + 3 * HOUR));
  push("This evening", evening);
  push("Tomorrow", at(now, 1, 9));
  push("Next week", monday);
  return out.sort((a, b) => a.until - b.until);
}

/** A duration from the custom snooze ("2 hours"). */
export type SnoozeUnit = "minutes" | "hours" | "days";
const UNIT: Record<SnoozeUnit, number> = { minutes: 60_000, hours: HOUR, days: DAY };

export function snoozeFor(amount: number, unit: SnoozeUnit, nowMs: number): number | null {
  if (!Number.isFinite(amount) || amount <= 0) return null;
  return nowMs + Math.round(amount * UNIT[unit]);
}

/** Time left on a snooze: `Nm` under an hour, `Nh` under a day, else `Nd` (rounded up; T3's). */
export function snoozeLeft(until: number, now: number): string {
  const ms = Math.max(0, until - now);
  if (ms < HOUR) return `${Math.max(1, Math.ceil(ms / 60_000))}m`;
  if (ms < DAY) return `${Math.ceil(ms / HOUR)}h`;
  return `${Math.ceil(ms / DAY)}d`;
}

/** How long ago, short: "now", "5m", "3h", "2d", "3w". */
export function shortAgo(at: number, now: number): string {
  const s = Math.max(0, Math.round((now - at) / 1000));
  if (s < 60) return "now";
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  const d = Math.floor(h / 24);
  return d < 14 ? `${d}d` : `${Math.floor(d / 7)}w`;
}

/** Why a thread can't be settled or snoozed now (the daemon's checks), or `null`. */
export function cantPark(t: Thread, what: "settle" | "snooze"): string | null {
  if (t.archived_at !== null) return "It's archived";
  if (t.pending_requests > 0) return "It's waiting for your answer";
  if (t.queued.length > 0) return "It has a queued message";
  if (what === "settle" && (t.status === "running" || t.status === "starting" || t.status === "waiting")) return "It's still running";
  return null;
}
