/**
 * Where the agents surface is and what it shows. Agents have no page in the dashboard nav: the
 * header's control opens them as a popup over whatever page is open (in the desktop app's main
 * window, as the separate Agents window instead), and `/dashboard/agents` shows the same surface
 * filling the window. One store for all of it, so the header, the popup and deep links agree.
 */

import { useEffect, useRef, useSyncExternalStore } from "react";
import { openAgentsWindow } from "./desktop";

/**
 * What the surface shows: the canvas alone, the canvas with one conversation or one machine's or
 * project's settings in the side panel (`target` is its canvas key, `m:…` or `p:…`), or the
 * launcher over the canvas: preset to a project or a machine, and opened where something was
 * dropped (`at`, in the surface's coordinates) when it was dragged out of a card. Or the owner's
 * automations in the sheet beside the canvas, a stack (lib/automations-nav.ts): the list (`list`:
 * the automations, every run, or the templates), one automation (`open`: its id) on its overview,
 * its builder (`tab` `edit` or `settings`; `open: "new"` from a `template` is always the builder),
 * its runs or one run (`run`), and a conversation that run started (`thread`).
 */
export type AgentsView =
  | { kind: "canvas" }
  | { kind: "thread"; thread: string }
  | { kind: "settings"; target: string }
  | { kind: "new"; project?: string; machine?: string; at?: { x: number; y: number } }
  | AutomationsView;

export interface AutomationsView {
  kind: "automations";
  list?: "runs" | "templates" | "notices";
  open?: string;
  tab?: "edit" | "runs" | "settings";
  run?: string;
  template?: string;
  thread?: string;
}

/** The conversation a view shows: its own, or one opened over a run in the automations sheet. */
export function threadOfView(view: AgentsView): string | null {
  return view.kind === "thread" || view.kind === "automations" ? (view.thread ?? null) : null;
}

export interface AgentsSurface {
  /** The popup is showing (the full-window surface ignores this). */
  open: boolean;
  view: AgentsView;
}

let state: AgentsSurface = { open: false, view: { kind: "canvas" } };
const listeners = new Set<() => void>();
/** Unsaved work in what's shown (`useLeaveGuard`), newest last. */
const holds: ((resume: () => void) => void)[] = [];

function set(next: AgentsSurface) {
  if (next.open === state.open && sameView(next.view, state.view)) return;
  // Leaving what's shown (another view, or the popup closing) with unsaved work in it: the work
  // says so and stays, as its panel's × does; Save or Discard, then go.
  const hold = holds.at(-1);
  if (hold && (!next.open || !sameView(next.view, state.view))) return hold(() => set(next));
  state = next;
  for (const l of listeners) l();
}

/**
 * While `dirty`, nothing leaves the view that shows it (a card clicked on the canvas, a row in the
 * sidebar, ⌘K, a link): `hold` runs instead, to say why and how to go on. `resume` goes where the
 * user was going, once the work is saved or discarded (it's held again while still dirty).
 */
export function useLeaveGuard(dirty: boolean, hold: (resume: () => void) => void) {
  const latest = useRef(hold);
  useEffect(() => {
    latest.current = hold;
  });
  useEffect(() => {
    if (!dirty) return;
    const h = (resume: () => void) => latest.current(resume);
    holds.push(h);
    return () => {
      const i = holds.lastIndexOf(h);
      if (i >= 0) holds.splice(i, 1);
    };
  }, [dirty]);
}

export const sameView = (a: AgentsView, b: AgentsView) => {
  if (a.kind !== b.kind) return false;
  if (a.kind === "thread") return a.thread === (b as typeof a).thread;
  if (a.kind === "settings") return a.target === (b as typeof a).target;
  if (a.kind === "new") {
    const c = b as typeof a;
    return a.project === c.project && a.machine === c.machine && a.at?.x === c.at?.x && a.at?.y === c.at?.y;
  }
  if (a.kind === "automations") {
    const c = b as AutomationsView;
    return a.list === c.list && a.open === c.open && a.tab === c.tab && a.run === c.run && a.template === c.template && a.thread === c.thread;
  }
  return true;
};

export const agentsSurface = {
  subscribe(fn: () => void) {
    listeners.add(fn);
    return () => void listeners.delete(fn);
  },
  get: () => state,
};

/** Open the agents (on `view`, else where they were). In the desktop app's main window this opens the Agents window. */
export function showAgents(view?: AgentsView) {
  const v = view ?? state.view;
  if (openAgentsWindow(v.kind === "thread" ? { kind: "thread", thread: v.thread } : v.kind === "settings" || v.kind === "automations" ? { kind: "canvas" } : { kind: v.kind })) return;
  set({ open: true, view: v });
}

export function hideAgents() {
  set({ ...state, open: false });
}

/** Show `view` without opening or closing anything. */
export function setAgentsView(view: AgentsView) {
  set({ ...state, view });
}

export function useAgentsSurface(): AgentsSurface {
  return useSyncExternalStore(agentsSurface.subscribe, agentsSurface.get, agentsSurface.get);
}

/**
 * `#agents`, `#agents/new`, `#agents/thr_…`, `#agents/settings/m:…`, `#agents/automations[/runs |
 * /templates | /new[/<template>] | /<id>[/edit | /settings | /runs[/<run>[/<thread>]]]]`: a link that
 * opens the popup on any dashboard page.
 */
export function viewFromHash(hash: string): AgentsView | null {
  // A query after the view (`#agents/settings/owner?linear=connected`: a redirect that appended
  // one to a path with a hash) is the page's, not the view's.
  const m = /^#agents(?:\/([^?&]+))?(?:[?&].*)?$/.exec(hash);
  if (!m) return null;
  const rest = m[1] ? decodeURIComponent(m[1]) : "";
  if (!rest) return { kind: "canvas" };
  if (rest === "new") return { kind: "new" };
  if (rest.startsWith("settings/")) return { kind: "settings", target: rest.slice(9) };
  if (rest === "automations" || rest.startsWith("automations/")) return automationsFromPath(rest.split("/").slice(1).filter(Boolean));
  return { kind: "thread", thread: rest };
}

function automationsFromPath(p: string[]): AutomationsView {
  const v: AutomationsView = { kind: "automations" };
  if (!p.length) return v;
  if (p[0] === "runs" || p[0] === "templates" || p[0] === "notices") return p.length === 1 ? { ...v, list: p[0] } : v;
  if (p[0] === "new") return p[1] ? { ...v, open: "new", template: p[1] } : { ...v, open: "new" };
  const open = { ...v, open: p[0] };
  if (p[1] === "edit" || p[1] === "settings") return { ...open, tab: p[1] };
  if (p[1] === "runs") return p[2] ? { ...open, tab: "runs", run: p[2], ...(p[3] ? { thread: p[3] } : {}) } : { ...open, tab: "runs" };
  return open;
}

export function hashFor(view: AgentsView): string {
  switch (view.kind) {
    case "canvas":
      return "#agents";
    case "new":
      return "#agents/new";
    case "settings":
      return `#agents/settings/${encodeURIComponent(view.target)}`;
    case "thread":
      return `#agents/${encodeURIComponent(view.thread)}`;
    case "automations": {
      const e = encodeURIComponent;
      if (!view.open) return view.list ? `#agents/automations/${view.list}` : "#agents/automations";
      if (view.open === "new") return view.template ? `#agents/automations/new/${e(view.template)}` : "#agents/automations/new";
      const base = `#agents/automations/${e(view.open)}`;
      if (view.tab === "edit" || view.tab === "settings") return `${base}/${view.tab}`;
      if (view.tab === "runs") return view.run ? `${base}/runs/${e(view.run)}${view.thread ? `/${e(view.thread)}` : ""}` : `${base}/runs`;
      return base;
    }
  }
}

/**
 * Write what's open into the URL, with a history entry for each level opened (the popup over the
 * page, then a panel over the canvas): Back closes the newest level, as a phone's back gesture
 * should, instead of leaving the dashboard. Closing a level from the UI goes back over the entry it
 * pushed (the hash change that follows sets the view it already has). Same level: replaced.
 */
export function writeAgentsUrl(url: string, depth: number, wasDepth: number) {
  const h = window.history;
  const state = (h.state ?? {}) as Record<string, unknown>;
  if (depth > wasDepth) h.pushState({ ...state, agentsDepth: depth }, "", url);
  else if (depth < wasDepth && state.agentsDepth === wasDepth) h.back();
  else h.replaceState(h.state, "", url);
}

/** ⌘N or ⌘⇧O (Ctrl elsewhere): a new agent, T3's `chat.new` bindings. */
export function isNewAgentKey(e: KeyboardEvent): boolean {
  if (!(e.metaKey || e.ctrlKey) || e.altKey) return false;
  const k = e.key.toLowerCase();
  return (k === "n" && !e.shiftKey) || (k === "o" && e.shiftKey);
}

/** The new-agent shortcut to show: ⌘N on a Mac; elsewhere Ctrl+N is the browser's new window, so Ctrl+Shift+O. */
export function newAgentHint(platform: string): string {
  return /Mac|iPhone|iPad/.test(platform) ? "⌘N" : "Ctrl+Shift+O";
}

/** For tests. */
export function resetAgentsSurface() {
  state = { open: false, view: { kind: "canvas" } };
  for (const l of listeners) l();
}
