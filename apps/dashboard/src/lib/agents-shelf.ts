/**
 * Which projects have their shelf open on the canvas (docs/code/agents.md, "Settling"): the
 * project card's list of its settled, snoozed and archived conversations. Per device, remembered,
 * shared by the card, focusing the project (F) and ⌘K, which open it.
 */

import { useSyncExternalStore } from "react";

const KEY = "roll:agents:shelves";

let open: ReadonlySet<string> | null = null;
const subs = new Set<() => void>();

function load(): ReadonlySet<string> {
  if (open) return open;
  try {
    const raw = typeof localStorage === "undefined" ? null : localStorage.getItem(KEY);
    const ids = raw ? (JSON.parse(raw) as unknown) : [];
    open = new Set(Array.isArray(ids) ? ids.filter((x): x is string => typeof x === "string") : []);
  } catch {
    open = new Set();
  }
  return open;
}

function set(next: ReadonlySet<string>) {
  open = next;
  try {
    localStorage.setItem(KEY, JSON.stringify([...next]));
  } catch {
    // Private mode: it lasts as long as the page.
  }
  for (const f of subs) f();
}

const subscribe = (f: () => void) => {
  subs.add(f);
  return () => {
    subs.delete(f);
  };
};

const EMPTY: ReadonlySet<string> = new Set();

export function useShelfOpen(project: string): boolean {
  return useSyncExternalStore(subscribe, () => load(), () => EMPTY).has(project);
}

export function setShelfOpen(project: string, on: boolean) {
  const cur = load();
  if (cur.has(project) === on) return;
  const next = new Set(cur);
  if (on) next.add(project);
  else next.delete(project);
  set(next);
}

/** Tests start from nothing remembered. */
export function resetShelves() {
  open = null;
  try {
    localStorage.removeItem(KEY);
  } catch {
    // Nothing to forget.
  }
}
