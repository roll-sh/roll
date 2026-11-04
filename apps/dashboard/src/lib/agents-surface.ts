/**
 * Where the agents surface is and what it shows. Agents have no page in the dashboard nav: the
 * header's control opens them as a popup over whatever page is open (in the desktop app's main
 * window, as the separate Agents window instead), and `/dashboard/agents` shows the same surface
 * filling the window. One store for all of it, so the header, the popup and deep links agree.
 */

import { useSyncExternalStore } from "react";
import { openAgentsWindow } from "./desktop";

/**
 * What the surface shows: the canvas alone, the canvas with one conversation or one machine's or
 * project's settings in the side panel (`target` is its canvas key, `m:…` or `p:…`), or the
 * launcher over the canvas: preset to a project or a machine, and opened where something was
 * dropped (`at`, in the surface's coordinates) when it was dragged out of a card. Or the owner's
 * automations in place of the canvas (`list`: the automations, every run, or the templates), with
 * one open in the side panel (`open`: its id, or `new` from a `template`) on its builder, its runs
 * (`run`: one of them) or its settings.
 */
export type AgentsView =
  | { kind: "canvas" }
  | { kind: "thread"; thread: string }
  | { kind: "settings"; target: string }
  | { kind: "new"; project?: string; machine?: string; at?: { x: number; y: number } }
  | AutomationsView;

export interface AutomationsView {
  kind: "automations";
  list?: "runs" | "templates";
  open?: string;
  tab?: "runs" | "settings";
  run?: string;
  template?: string;
}

export interface AgentsSurface {
  /** The popup is showing (the full-window surface ignores this). */
  open: boolean;
  view: AgentsView;
}

let state: AgentsSurface = { open: false, view: { kind: "canvas" } };
const listeners = new Set<() => void>();

function set(next: AgentsSurface) {
  if (next.open === state.open && sameView(next.view, state.view)) return;
  state = next;
  for (const l of listeners) l();
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
    return a.list === c.list && a.open === c.open && a.tab === c.tab && a.run === c.run && a.template === c.template;
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
 * /templates | /new[/<template>] | /<id>[/runs[/<run>] | /settings]]`: a link that opens the popup on
 * any dashboard page.
 */
export function viewFromHash(hash: string): AgentsView | null {
  const m = /^#agents(?:\/(.+))?$/.exec(hash);
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
  if (p[0] === "runs" || p[0] === "templates") return p.length === 1 ? { ...v, list: p[0] } : v;
  if (p[0] === "new") return p[1] ? { ...v, open: "new", template: p[1] } : { ...v, open: "new" };
  const open = { ...v, open: p[0] };
  if (p[1] === "settings") return { ...open, tab: "settings" };
  if (p[1] === "runs") return p[2] ? { ...open, tab: "runs", run: p[2] } : { ...open, tab: "runs" };
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
      if (view.tab === "settings") return `${base}/settings`;
      if (view.tab === "runs") return view.run ? `${base}/runs/${e(view.run)}` : `${base}/runs`;
      return base;
    }
  }
}

/** ⌘N or ⌘⇧O (Ctrl elsewhere): a new agent, T3's `chat.new` bindings. */
export function isNewAgentKey(e: KeyboardEvent): boolean {
  if (!(e.metaKey || e.ctrlKey) || e.altKey) return false;
  const k = e.key.toLowerCase();
  return (k === "n" && !e.shiftKey) || (k === "o" && e.shiftKey);
}

/** For tests. */
export function resetAgentsSurface() {
  state = { open: false, view: { kind: "canvas" } };
  for (const l of listeners) l();
}
