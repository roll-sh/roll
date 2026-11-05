/**
 * The owner's automations and their recent runs, live: loaded from `/v1/code/agents/{owner}/
 * automations…` and kept current by the owner's event stream (`automation`, `automation_removed`,
 * `automation_run`; the same stream the canvas follows, through `AgentsClient.onAutomation`).
 * A record from the stream and one from a load are the same record at different cursors: the
 * newer cursor wins, so a slow load never puts back something older. Also: which thread each run
 * started (the canvas tags those cards) and the `notice` events (toasts).
 */

import { useEffect, useSyncExternalStore } from "react";
import type { StreamEvent, ThreadId } from "./agents-api";
import type { AgentsClient } from "./agents-client";
import type { Automation, AutomationId, AutomationRun, AutomationRunId, AutomationsApi, Catalog, Notice } from "./automations-api";

export type AutomationEvent = Extract<StreamEvent, { type: "automation" | "automation_removed" | "automation_run" | "notice" }>;

export const isAutomationEvent = (ev: StreamEvent): ev is AutomationEvent =>
  ev.type === "automation" || ev.type === "automation_removed" || ev.type === "automation_run" || ev.type === "notice";

/** A thread an automation started. */
export interface ThreadOrigin {
  automation: AutomationId;
  name: string;
  run: AutomationRunId;
  step: string;
}

export interface AutomationsState {
  /** The first load answered (or failed). */
  loaded: boolean;
  error: string | null;
  automations: ReadonlyMap<AutomationId, Automation>;
  /** Recent runs (the last load's page plus everything the stream brought since). */
  runs: ReadonlyMap<AutomationRunId, AutomationRun>;
  threads: ReadonlyMap<ThreadId, ThreadOrigin>;
  catalog: Catalog | null;
}

/** Runs kept in memory; the oldest finished ones go first. */
const MAX_RUNS = 600;
/** Runs the first load asks for (the server's page limit). */
const FIRST_PAGE = 200;

export class AutomationsStore {
  private state: AutomationsState = { loaded: false, error: null, automations: new Map(), runs: new Map(), threads: new Map(), catalog: null };
  private listeners = new Set<() => void>();
  private notices = new Set<(n: Notice) => void>();
  private loading: Promise<void> | null = null;
  /** Ids removed, with the cursor they were removed at (a late load doesn't bring them back). */
  private removed = new Map<AutomationId, number>();

  constructor(
    readonly owner: string,
    readonly api: AutomationsApi,
  ) {}

  subscribe = (fn: () => void) => {
    this.listeners.add(fn);
    return () => void this.listeners.delete(fn);
  };

  getSnapshot = () => this.state;

  private set(patch: Partial<AutomationsState>) {
    this.state = { ...this.state, ...patch };
    for (const l of this.listeners) l();
  }

  /** Load once (again after `refresh`); the stream keeps it current after. */
  ensure(): Promise<void> {
    this.loading ??= this.load();
    return this.loading;
  }

  refresh(): Promise<void> {
    this.loading = this.load();
    return this.loading;
  }

  private async load() {
    try {
      const [list, runs, catalog] = await Promise.all([
        this.api.list(this.owner),
        this.api.runs(this.owner, { limit: FIRST_PAGE }),
        this.api.catalog(this.owner).catch(() => null),
      ]);
      const automations = new Map(this.state.automations);
      for (const a of list) this.mergeAutomation(automations, a);
      // Whatever the list doesn't have any more was deleted while nobody listened.
      const listed = new Set(list.map((a) => a.id));
      for (const [id, a] of automations) if (!listed.has(id) && a.cursor <= Math.max(0, ...list.map((x) => x.cursor))) automations.delete(id);
      const all = new Map(this.state.runs);
      for (const r of runs) this.mergeRun(all, r);
      this.set({ loaded: true, error: null, automations, catalog: catalog ?? this.state.catalog, ...this.withRuns(all) });
    } catch (e) {
      this.loading = null;
      this.set({ loaded: true, error: e instanceof Error ? e.message : String(e) });
    }
  }

  private mergeAutomation(into: Map<AutomationId, Automation>, a: Automation) {
    const gone = this.removed.get(a.id);
    if (gone !== undefined && gone >= a.cursor) return;
    const have = into.get(a.id);
    if (!have || have.cursor <= a.cursor) into.set(a.id, a);
  }

  private mergeRun(into: Map<AutomationRunId, AutomationRun>, r: AutomationRun) {
    const have = into.get(r.id);
    if (!have || have.cursor <= r.cursor) into.set(r.id, r);
  }

  private withRuns(runs: Map<AutomationRunId, AutomationRun>): Pick<AutomationsState, "runs" | "threads"> {
    if (runs.size > MAX_RUNS) {
      const old = [...runs.values()].filter((r) => r.finished_at !== null).sort((a, b) => a.created_at - b.created_at);
      for (const r of old.slice(0, runs.size - MAX_RUNS)) runs.delete(r.id);
    }
    const threads = new Map<ThreadId, ThreadOrigin>();
    const names = this.state.automations;
    for (const r of [...runs.values()].sort((a, b) => a.created_at - b.created_at))
      for (const t of r.threads) threads.set(t.thread, { automation: r.automation, name: names.get(r.automation)?.name ?? r.name, run: r.id, step: t.step });
    return { runs, threads };
  }

  /** One event off the owner's stream. */
  apply(ev: AutomationEvent) {
    switch (ev.type) {
      case "automation": {
        const automations = new Map(this.state.automations);
        if (ev.cursor > (this.removed.get(ev.automation.id) ?? -1)) this.removed.delete(ev.automation.id);
        this.mergeAutomation(automations, ev.automation);
        this.set({ automations });
        break;
      }
      case "automation_removed": {
        this.removed.set(ev.automation, ev.cursor);
        const automations = new Map(this.state.automations);
        automations.delete(ev.automation);
        this.set({ automations });
        break;
      }
      case "automation_run": {
        const runs = new Map(this.state.runs);
        this.mergeRun(runs, ev.run);
        this.set(this.withRuns(runs));
        break;
      }
      case "notice":
        for (const fn of this.notices) fn(ev.notice);
        break;
    }
  }

  /** A record a call answered with (a save, a run, a cancel): shown at once, before the stream says so. */
  put(a: Automation) {
    this.apply({ type: "automation", cursor: a.cursor, automation: a });
  }

  putRun(r: AutomationRun) {
    this.apply({ type: "automation_run", cursor: r.cursor, run: r });
  }

  forget(id: AutomationId) {
    const a = this.state.automations.get(id);
    this.apply({ type: "automation_removed", cursor: a?.cursor ?? 0, automation: id });
  }

  /** Older runs of one automation (or all), for paging the history. */
  async more(automation: AutomationId | null, before: AutomationRunId | undefined): Promise<AutomationRun[]> {
    const page = automation ? await this.api.runsOf(this.owner, automation, { before, limit: 50 }) : await this.api.runs(this.owner, { before, limit: 50 });
    const runs = new Map(this.state.runs);
    for (const r of page) this.mergeRun(runs, r);
    this.set(this.withRuns(runs));
    return page;
  }

  onNotice(fn: (n: Notice) => void) {
    this.notices.add(fn);
    return () => void this.notices.delete(fn);
  }

  /** A notice the dashboard raises itself (not a `notify` step's), shown with theirs. */
  local(n: Pick<Notice, "title" | "level"> & Partial<Pick<Notice, "body" | "link">>) {
    const notice: Notice = { id: `local-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`, link: null, automation: null, run: null, at: Date.now(), ...n };
    for (const fn of this.notices) fn(notice);
  }
}

const stores = new WeakMap<AgentsClient, AutomationsStore>();

/** The one store of `client`'s owner, fed by its stream. */
export function automationsFor(client: AgentsClient, api: AutomationsApi): AutomationsStore {
  let s = stores.get(client);
  if (!s || s.api !== api) {
    const store = new AutomationsStore(client.owner, api);
    client.onAutomation((ev) => store.apply(ev));
    stores.set(client, (s = store));
  }
  return s;
}

const EMPTY: AutomationsState = { loaded: false, error: null, automations: new Map(), runs: new Map(), threads: new Map(), catalog: null };
const none = () => () => {};
const empty = () => EMPTY;

/** The store's state, loading it the first time (`null` store: nothing, as in a share's replay). */
export function useAutomationsState(store: AutomationsStore | null): AutomationsState {
  useEffect(() => void store?.ensure(), [store]);
  return useSyncExternalStore(store ? store.subscribe : none, store ? store.getSnapshot : empty, store ? store.getSnapshot : empty);
}
