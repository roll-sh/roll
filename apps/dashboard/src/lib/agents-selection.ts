/**
 * The threads picked for a bulk action, shared by every place a thread shows: the list, the board,
 * canvas cards and sidebar rows (docs/code/agents.md, "Working at scale"). Ported from T3 Code's
 * `threadSelectionStore.ts` (MIT): ⌘/Ctrl-click toggles one and makes it the anchor, Shift-click
 * selects from the anchor to it in the order the clicked place shows, Escape clears. One store per
 * page; ids are thread ids. Pure apart from the subscription.
 */

import { useSyncExternalStore } from "react";

export interface Selection {
  ids: ReadonlySet<string>;
  /** Where a Shift-click range starts: the last thread toggled on. */
  anchor: string | null;
}

const EMPTY: Selection = { ids: new Set(), anchor: null };

// ── the pure model (tested) ──

export function toggle(s: Selection, id: string): Selection {
  const ids = new Set(s.ids);
  if (ids.has(id)) ids.delete(id);
  else ids.add(id);
  return { ids, anchor: ids.has(id) ? id : s.anchor === id ? null : s.anchor };
}

/**
 * Everything from the anchor to `id` in `order` added (what's selected stays). Without an anchor,
 * or with one this order doesn't have (it was picked in another view), just `id`.
 */
export function rangeTo(s: Selection, id: string, order: readonly string[]): Selection {
  const from = s.anchor === null ? -1 : order.indexOf(s.anchor);
  const to = order.indexOf(id);
  const ids = new Set(s.ids);
  if (from === -1 || to === -1) {
    ids.add(id);
    return { ids, anchor: id };
  }
  for (let i = Math.min(from, to); i <= Math.max(from, to); i++) ids.add(order[i]);
  return { ids, anchor: s.anchor };
}

/** Every id in `ids` selected, or (when they all already are) none of them: a group's checkbox. */
export function toggleAll(s: Selection, ids: readonly string[]): Selection {
  const all = ids.length > 0 && ids.every((id) => s.ids.has(id));
  const next = new Set(s.ids);
  for (const id of ids) {
    if (all) next.delete(id);
    else next.add(id);
  }
  return { ids: next, anchor: all ? (s.anchor && next.has(s.anchor) ? s.anchor : null) : (ids.at(-1) ?? s.anchor) };
}

/** Threads that are gone (deleted, or no longer loaded) leave the selection. */
export function prune(s: Selection, exists: (id: string) => boolean): Selection {
  let gone = false;
  for (const id of s.ids) if (!exists(id)) gone = true;
  if (!gone) return s;
  const ids = new Set([...s.ids].filter(exists));
  return { ids, anchor: s.anchor && ids.has(s.anchor) ? s.anchor : null };
}

/** What a click with these keys does: ⌘/Ctrl toggles, Shift selects a range, a plain click opens. */
export function clickIntent(e: { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean; altKey?: boolean }, mac: boolean): "toggle" | "range" | null {
  if (e.altKey) return null;
  if (e.shiftKey) return "range";
  if (mac ? e.metaKey : e.ctrlKey) return "toggle";
  return null;
}

// ── the page's store ──

let state: Selection = EMPTY;
const listeners = new Set<() => void>();

function set(next: Selection) {
  if (next === state) return;
  state = next;
  for (const l of listeners) l();
}

const subscribe = (l: () => void) => {
  listeners.add(l);
  return () => void listeners.delete(l);
};

export const selection = {
  get: () => state,
  toggle: (id: string) => set(toggle(state, id)),
  rangeTo: (id: string, order: readonly string[]) => set(rangeTo(state, id, order)),
  toggleAll: (ids: readonly string[]) => set(toggleAll(state, ids)),
  /** Exactly these selected (a bulk action's failures, say). */
  replace: (ids: Iterable<string>) => {
    const s = new Set(ids);
    set(s.size ? { ids: s, anchor: null } : EMPTY);
  },
  prune: (exists: (id: string) => boolean) => set(prune(state, exists)),
  clear: () => set(state.ids.size === 0 && state.anchor === null ? state : EMPTY),
  subscribe,
};

/** The whole selection: for the bulk bar and anything that counts it. */
export function useSelection(): Selection {
  return useSyncExternalStore(subscribe, selection.get, () => EMPTY);
}

/** Whether one thread is selected: a row re-renders only when its own answer changes. */
export function useSelected(id: string): boolean {
  return useSyncExternalStore(
    subscribe,
    () => state.ids.has(id),
    () => false,
  );
}

/**
 * A click on a thread somewhere that lists threads in `order`: with ⌘/Ctrl or Shift it selects
 * (and returns true, so the caller doesn't open the thread); a plain click returns false.
 */
export function selectFromClick(e: { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean; altKey?: boolean; preventDefault?: () => void }, id: string, order: () => readonly string[]): boolean {
  const intent = clickIntent(e, isMac());
  if (!intent) return false;
  e.preventDefault?.();
  if (intent === "toggle") selection.toggle(id);
  else selection.rangeTo(id, order());
  return true;
}

export const isMac = () => typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

/** Tests start clean. */
export function resetSelection() {
  set(EMPTY);
}
