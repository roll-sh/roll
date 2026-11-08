/**
 * The automations list's logic (docs/code/automations-ui.md, "The list"): what a row says about
 * its runs, and the search and filters (status, trigger, project), kept out of the component so
 * they're tested on their own.
 */

import type { Automation, AutomationRun } from "./automations-api";
import { lastRunOf, triggerSource } from "./automations-ui";

export type StatusFilter = "on" | "paused" | "failing" | "running";

/** At most one value per kind, Linear's quick filters. */
export interface ListFilter {
  status?: StatusFilter;
  /** A trigger source: `schedule`, `code`, `linear`, … */
  trigger?: string;
  /** A project id the automation works in. */
  project?: string;
}

const going = (r: AutomationRun) => r.status === "running" || r.status === "waiting" || r.status === "queued";

/** Its newest run going on now, if any. */
export function liveRunOf(id: string, runs: Iterable<AutomationRun>): AutomationRun | null {
  let best: AutomationRun | null = null;
  for (const r of runs) if (r.automation === id && going(r) && (!best || r.created_at > best.created_at)) best = r;
  return best;
}

/** Its last `n` runs, oldest first: the row's run history bars. */
export function recentRunsOf(id: string, runs: Iterable<AutomationRun>, n = 12): AutomationRun[] {
  return [...runs]
    .filter((r) => r.automation === id)
    .sort((a, b) => b.created_at - a.created_at)
    .slice(0, n)
    .reverse();
}

export function matchesStatus(a: Automation, status: StatusFilter, runs: Iterable<AutomationRun>): boolean {
  switch (status) {
    case "on":
      return a.enabled;
    case "paused":
      return !a.enabled;
    case "failing":
      return lastRunOf(a, runs)?.status === "failed";
    case "running":
      return liveRunOf(a.id, runs) !== null;
  }
}

export interface ListContext {
  runs: readonly AutomationRun[];
  /** The trigger in words, for search. */
  summary: (a: Automation) => string;
  /** The projects it works in (ids), for the project filter. */
  projectsOf: (a: Automation) => readonly string[];
}

/** The automations a search and the filters leave, in their order. */
export function filterAutomations(all: readonly Automation[], query: string, filter: ListFilter, ctx: ListContext): Automation[] {
  const runs = [...ctx.runs];
  const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  return all.filter((a) => {
    if (filter.status && !matchesStatus(a, filter.status, runs)) return false;
    if (filter.trigger && triggerSource(a.trigger) !== filter.trigger) return false;
    if (filter.project && !ctx.projectsOf(a).includes(filter.project)) return false;
    if (!words.length) return true;
    const text = `${a.name} ${a.description ?? ""} ${ctx.summary(a)} ${a.steps.map((s) => `${s.name ?? ""} ${s.kind}`).join(" ")}`.toLowerCase();
    return words.every((w) => text.includes(w));
  });
}

/** What each filter can be set to, with how many automations it leaves (only values that occur). */
export function filterOptions(all: readonly Automation[], ctx: Pick<ListContext, "runs" | "projectsOf">) {
  const runs = [...ctx.runs];
  const count = <K extends string>(keys: (a: Automation) => readonly K[]) => {
    const m = new Map<K, number>();
    for (const a of all) for (const k of new Set(keys(a))) m.set(k, (m.get(k) ?? 0) + 1);
    return [...m].map(([value, n]) => ({ value, count: n }));
  };
  return {
    status: (["on", "paused", "failing", "running"] as const).map((value) => ({ value, count: all.filter((a) => matchesStatus(a, value, runs)).length })),
    trigger: count((a) => [triggerSource(a.trigger)]),
    project: count((a) => ctx.projectsOf(a)),
  };
}
