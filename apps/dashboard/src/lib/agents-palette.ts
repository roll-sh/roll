/**
 * The agents' command palette (⌘K inside the agents surface): whether it's open and what it opened
 * to, one store for the surface's header, its keyboard shortcut and the canvas's prompt bar (T3's
 * `commandPaletteBus`). Also the thread-open history its search ranks with (T3 keeps it on its
 * server, `OrchestrationV2ThreadOpens`; roll keeps it per device).
 */

import { useSyncExternalStore } from "react";
import { recordThreadOpen, type ThreadOpenStats } from "./palette-search";

/** What the palette opens to: a search (with a query), or straight into one of its views. */
export type PaletteIntent =
  | { kind: "search"; query?: string }
  /** "What should the agents get done?" with the goal so far. */
  | { kind: "orchestrate"; goal?: string }
  | { kind: "add-project" }
  | { kind: "new-in" };

export interface PaletteState {
  open: boolean;
  intent: PaletteIntent;
  /** Bumped by every open, so an open palette starts over from the new intent. */
  session: number;
}

let state: PaletteState = { open: false, intent: { kind: "search" }, session: 0 };
const listeners = new Set<() => void>();

function set(next: PaletteState) {
  state = next;
  for (const l of listeners) l();
}

export const agentsPalette = {
  subscribe(fn: () => void) {
    listeners.add(fn);
    return () => void listeners.delete(fn);
  },
  get: () => state,
};

export function openPalette(intent: PaletteIntent = { kind: "search" }) {
  set({ open: true, intent, session: state.session + 1 });
}

export function closePalette() {
  if (state.open) set({ ...state, open: false });
}

/** ⌘K: open on a search, or close. */
export function togglePalette() {
  if (state.open) closePalette();
  else openPalette();
}

export function usePalette(): PaletteState {
  return useSyncExternalStore(agentsPalette.subscribe, agentsPalette.get, agentsPalette.get);
}

// ── thread opens ──

const OPENS = "roll:agents:opens";
/** Threads remembered; the least recently opened go first. */
const OPENS_MAX = 400;

type Opens = Record<string, ThreadOpenStats>;

export function threadOpens(): Opens {
  try {
    const raw = JSON.parse(localStorage.getItem(OPENS) ?? "{}") as unknown;
    return raw && typeof raw === "object" ? (raw as Opens) : {};
  } catch {
    return {};
  }
}

/** A thread was opened, from the palette's search for `query` or any other way. */
export function noteThreadOpen(thread: string, query?: string, at = Date.now()) {
  const opens = threadOpens();
  opens[thread] = recordThreadOpen(opens[thread] ?? null, at, query);
  const kept = Object.entries(opens)
    .sort((a, b) => b[1].openedAt - a[1].openedAt)
    .slice(0, OPENS_MAX);
  try {
    localStorage.setItem(OPENS, JSON.stringify(Object.fromEntries(kept)));
  } catch {
    // Not remembered.
  }
}

/** For tests. */
export function resetPalette() {
  // Sessions keep counting, so a closing palette from before never closes a new one.
  state = { open: false, intent: { kind: "search" }, session: state.session };
  for (const l of listeners) l();
}
