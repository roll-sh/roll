/**
 * Where the agents surface is and what it shows. Agents have no page in the dashboard nav: the
 * header's control opens them as a popup over whatever page is open (in the desktop app's main
 * window, as the separate Agents window instead), and `/dashboard/agents` shows the same surface
 * filling the window. One store for all of it, so the header, the popup and deep links agree.
 */

import { useSyncExternalStore } from "react";
import { openAgentsWindow } from "./desktop";

/** What the surface shows: the canvas, one conversation, or a new agent's composer. */
export type AgentsView = { kind: "canvas" } | { kind: "thread"; thread: string } | { kind: "new" };

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

export const sameView = (a: AgentsView, b: AgentsView) => a.kind === b.kind && (a.kind !== "thread" || a.thread === (b as typeof a).thread);

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
  if (openAgentsWindow(v.kind === "thread" ? { kind: "thread", thread: v.thread } : v)) return;
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

/** `#agents`, `#agents/new`, `#agents/thr_…`: a link that opens the popup on any dashboard page. */
export function viewFromHash(hash: string): AgentsView | null {
  const m = /^#agents(?:\/(.+))?$/.exec(hash);
  if (!m) return null;
  const rest = m[1] ? decodeURIComponent(m[1]) : "";
  if (!rest) return { kind: "canvas" };
  if (rest === "new") return { kind: "new" };
  return { kind: "thread", thread: rest };
}

export function hashFor(view: AgentsView): string {
  return view.kind === "canvas" ? "#agents" : view.kind === "new" ? "#agents/new" : `#agents/${encodeURIComponent(view.thread)}`;
}

/** For tests. */
export function resetAgentsSurface() {
  state = { open: false, view: { kind: "canvas" } };
  for (const l of listeners) l();
}
