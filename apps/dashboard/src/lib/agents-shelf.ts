/**
 * The shelf (docs/code/agents.md, "Settling"): every settled, snoozed and archived conversation,
 * which have left the canvas, in one popover off the canvas toolbar, grouped by project. Linear
 * keeps its archive behind one control rather than on each team; T3 has its sidebar's Settled shelf
 * (`Sidebar.tsx`, `olderSettledShelf`) and its archive in settings (`ArchivedThreadsPanel.tsx`).
 *
 * Whether it's open, and which project it's narrowed to, is page state shared by the toolbar's
 * control, a project's ⋯ menu, the sidebar and ⌘K, which all open it. What it lists is pure
 * (`shelfList`), from `agents-inbox.ts`.
 */

import { useSyncExternalStore } from "react";
import type { Project } from "./agents-api";
import { inbox, type InboxRow, type ProjectInbox, type Shelf } from "./agents-inbox";
import type { AgentsData } from "./agents-store";

export type Parked = Exclude<Shelf, "active">;
export const PARKED: readonly Parked[] = ["settled", "snoozed", "archived"];
export const PARKED_LABEL: Record<Parked, string> = { settled: "Settled", snoozed: "Snoozed", archived: "Archived" };

export interface ShelfState {
  open: boolean;
  /** Narrowed to one project (opened from it), or every project. */
  project: string | null;
  /** The tab it opens on; `null` picks the first with something on it. */
  tab: Parked | null;
}

const CLOSED: ShelfState = { open: false, project: null, tab: null };
let state: ShelfState = CLOSED;
const subs = new Set<() => void>();

function set(next: ShelfState) {
  state = next;
  for (const f of subs) f();
}

const subscribe = (f: () => void) => {
  subs.add(f);
  return () => {
    subs.delete(f);
  };
};

export function useShelf(): ShelfState {
  return useSyncExternalStore(subscribe, () => state, () => CLOSED);
}

/** Open it, on every project or narrowed to one (and on a tab). */
export function openShelf(project: string | null = null, tab: Parked | null = null) {
  set({ open: true, project, tab });
}

export function closeShelf() {
  if (state.open) set({ ...state, open: false });
}

/** Narrow it to a project, or widen it to all of them, while it's open. */
export function narrowShelf(project: string | null) {
  set({ ...state, project });
}

/** Tests start closed. */
export function resetShelves() {
  state = CLOSED;
}

/** How many conversations are on a project's shelves (or every project's). */
export function shelfCount(groups: readonly ProjectInbox[], project: string | null = null): number {
  let n = 0;
  for (const g of groups) if (project === null || g.project.id === project) n += g.settled.length + g.snoozed.length + g.archived.length;
  return n;
}

export interface ShelfSection {
  project: Project;
  rows: InboxRow[];
}

export interface ShelfList {
  /** Per tab, across the projects in scope (before the search): the tab strip's numbers. */
  counts: Record<Parked, number>;
  /** The tab shown: the asked one, else the first with anything on it. */
  tab: Parked;
  /** The tab's rows that match the search, by project (projects by name, rows in the shelf's order). */
  sections: ShelfSection[];
  /** Every row shown, in order: what the arrow keys walk. */
  rows: InboxRow[];
}

/**
 * What the shelf lists: one tab's conversations, narrowed to a project and to a search (every word
 * in the title or the project's name, any case), grouped by project.
 */
export function shelfList(data: AgentsData, now: number, opts: { project?: string | null; tab?: Parked | null; query?: string } = {}): ShelfList {
  const groups = inbox(data, now).filter((g) => !opts.project || g.project.id === opts.project);
  const counts = { settled: 0, snoozed: 0, archived: 0 } satisfies Record<Parked, number>;
  for (const g of groups) for (const s of PARKED) counts[s] += g[s].length;
  const tab = opts.tab && counts[opts.tab] > 0 ? opts.tab : (PARKED.find((s) => counts[s] > 0) ?? opts.tab ?? "settled");
  const words = (opts.query ?? "").toLowerCase().split(/\s+/).filter(Boolean);
  const matches = (r: InboxRow, project: Project) => {
    if (!words.length) return true;
    const text = `${r.thread.title} ${project.name}`.toLowerCase();
    return words.every((w) => text.includes(w));
  };
  const sections: ShelfSection[] = [];
  for (const g of groups) {
    const rows = g[tab].filter((r) => matches(r, g.project));
    if (rows.length) sections.push({ project: g.project, rows });
  }
  return { counts, tab, sections, rows: sections.flatMap((s) => s.rows) };
}
