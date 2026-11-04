/**
 * The agents' command palette, its logic: items, groups, search and the view stack. Ported from
 * T3 Code's `apps/web/src/components/CommandPalette.logic.ts` (MIT): actions first, recent
 * threads under them; a query searches actions, every submenu's options ("New agent in roll"),
 * threads, projects and machines with T3's ranking (`lib/palette-search.ts`), `>` searches actions
 * only, and a submenu with no match falls back to the root search. roll adds the goal rows: any
 * query can be a goal for agents ("Orchestrate: …", "New agent: …"), first when it reads like one.
 */

import type { ReactNode } from "react";
import {
  foldSearchQuery,
  PALETTE_SECONDARY_LIMIT,
  PALETTE_THREAD_LIMIT,
  rankSearchGroups,
  scoreSearchTerms,
  scoreThreadSearch,
  type SearchMatchHint,
  type ThreadSearchSubject,
} from "@/lib/palette-search";

/** Recent threads shown before typing. */
export const RECENT_THREAD_LIMIT = 12;

interface ItemBase {
  /** Unique across the palette: `action:new`, `thread:thr_…`, `project:prj_…`. */
  readonly value: string;
  readonly title: string;
  /** Keywords besides the title (they count less, and only at word starts). */
  readonly searchTerms?: readonly string[];
  readonly description?: ReactNode;
  readonly icon: ReactNode;
  /** Before the title (a thread's status). */
  readonly leading?: ReactNode;
  /** After the title (a thread's change, its machine). */
  readonly trailing?: ReactNode;
  readonly timestamp?: string;
  /** A key hint, shown at the end of the row. */
  readonly shortcut?: string;
  /** Thread rows rank by this, with use history, instead of by their terms. */
  readonly threadSearch?: ThreadSearchSubject;
  /** Shown in place of the description when the match isn't in the title. */
  readonly matchHint?: SearchMatchHint;
  /** Last in its group, and it doesn't decide where the group sits (the open thread). */
  readonly secondary?: boolean;
  readonly disabled?: boolean;
}

export interface PaletteAction extends ItemBase {
  readonly kind: "action";
  /** Picked; the palette closes unless `keepOpen`. */
  readonly run: () => void | Promise<unknown>;
  readonly keepOpen?: boolean;
}

export interface PaletteSubmenu extends ItemBase {
  readonly kind: "submenu";
  readonly view: PaletteView;
}

export type PaletteItem = PaletteAction | PaletteSubmenu;

export interface PaletteGroup {
  readonly value: string;
  readonly label: string;
  readonly items: readonly PaletteItem[];
}

/**
 * What the palette shows: a list (the root, a submenu), a list made from what's typed (a goal's
 * targets), or a panel of its own (adding a project).
 */
export interface PaletteView {
  /** The breadcrumb ("New agent in"). */
  readonly label: string;
  readonly placeholder: string;
  readonly groups?: readonly PaletteGroup[];
  /** Groups made from the query instead of filtered by it. */
  readonly build?: (query: string) => readonly PaletteGroup[];
  /** A panel in place of the list; `close` closes the palette. */
  readonly panel?: (close: () => void) => ReactNode;
  readonly initialQuery?: string;
  /** What Enter does in the footer ("Open", "Orchestrate"). */
  readonly enterLabel?: string;
}

// ── the view stack (T3's `reduceCommandPaletteSearchState`) ──

export interface PaletteStack {
  readonly query: string;
  readonly views: ReadonlyArray<{ readonly view: PaletteView; readonly parentQuery: string }>;
}

export type StackAction = { type: "query"; query: string } | { type: "push"; view: PaletteView } | { type: "back" } | { type: "reset"; query?: string; view?: PaletteView };

export function reduceStack(state: PaletteStack, action: StackAction): PaletteStack {
  switch (action.type) {
    case "query":
      return { ...state, query: action.query };
    case "push":
      return { query: action.view.initialQuery ?? "", views: [...state.views, { view: action.view, parentQuery: state.query }] };
    case "back": {
      const parent = state.views.at(-1);
      return parent ? { query: parent.parentQuery, views: state.views.slice(0, -1) } : state;
    }
    case "reset":
      return action.view ? { query: action.view.initialQuery ?? "", views: [{ view: action.view, parentQuery: action.query ?? "" }] } : { query: action.query ?? "", views: [] };
  }
}

// ── rows ──

export type PaletteRow = { kind: "label"; key: string; label: string } | { kind: "item"; key: string; item: PaletteItem; index: number | null };

/** Groups as rows, with each enabled item's place in keyboard order. */
export function buildRows(groups: readonly PaletteGroup[]): { rows: PaletteRow[]; values: string[] } {
  const rows: PaletteRow[] = [];
  const values: string[] = [];
  for (const group of groups) {
    if (group.label) rows.push({ kind: "label", key: `group:${group.value}`, label: group.label });
    for (const item of group.items) {
      const index = item.disabled ? null : values.length;
      if (index !== null) values.push(item.value);
      rows.push({ kind: "item", key: `${group.value}:${item.value}`, item, index });
    }
  }
  return { rows, values };
}

export function findItem(groups: readonly PaletteGroup[], value: string | null): PaletteItem | null {
  if (value === null) return null;
  for (const group of groups) for (const item of group.items) if (item.value === value && !item.disabled) return item;
  return null;
}

/** Every option inside a submenu as one row ("New agent in roll", "Project settings: web"). */
export function nestedItems(items: readonly PaletteItem[], query: string): PaletteAction[] {
  const topLevel = new Set(items.map((i) => i.title));
  const folded = foldSearchQuery(query);
  return items.flatMap((parent) => {
    if (parent.kind !== "submenu" || !parent.view.groups) return [];
    const stem = parent.title.replace(/(\.\.\.|…)$/u, "");
    // A query that only names the submenu ("new agent in") wants the submenu row itself.
    if (foldSearchQuery(stem).startsWith(folded)) return [];
    const prefix = stem === parent.title ? `${stem}: ` : `${stem.trimEnd()} `;
    return parent.view.groups.flatMap((group) =>
      group.items.flatMap((child) => {
        if (child.kind !== "action") return [];
        const title = `${prefix}${child.title}`;
        if (topLevel.has(title)) return [];
        // Only the combined title: a child's keywords would list every option.
        return [{ ...child, value: `${parent.value}>${child.value}`, title, searchTerms: [] }];
      }),
    );
  });
}

// ── search ──

type Match = NonNullable<ReturnType<typeof scoreSearchTerms>> | NonNullable<ReturnType<typeof scoreThreadSearch>>;

function scoreItem(item: PaletteItem, query: string, now: number, typos: boolean): Match | null {
  const match = item.threadSearch ? scoreThreadSearch(query, item.threadSearch, now, { typos }) : scoreSearchTerms(query, [item.title, ...(item.searchTerms ?? [])], { typos });
  // A row listed last in its group doesn't decide where the group sits.
  return match && item.secondary ? { ...match, groupQuality: 0 } : match;
}

export interface FilterInput {
  /** The view's groups: the root's actions and recent threads, or a submenu's. */
  groups: readonly PaletteGroup[];
  query: string;
  inSubmenu: boolean;
  /** The root's groups and every submenu option, so a submenu with no match searches everything. */
  root?: { groups: readonly PaletteGroup[]; nested: readonly PaletteAction[] };
  threads?: readonly PaletteItem[];
  projects?: readonly PaletteItem[];
  machines?: readonly PaletteItem[];
  /** Rows for the query as a goal; placed by `goalsFirst`. */
  goals?: readonly PaletteItem[];
  now?: number;
}

export interface Filtered {
  groups: PaletteGroup[];
  /** The best match class of the search (0 when nothing matched), for where goals go. */
  best: number;
}

/**
 * T3's `filterCommandPaletteGroups`: equal match classes put actions before threads, so "new"
 * runs "New agent"; threads come before projects and machines.
 */
export function filterGroups(input: FilterInput): PaletteGroup[] {
  const { groups, best } = searchGroups(input);
  const goals = input.goals ?? [];
  if (goals.length === 0 || input.inSubmenu || input.query.startsWith(">")) return groups;
  const group: PaletteGroup = { value: "goal", label: "Ask agents", items: goals };
  return goalsFirst(input.query, best) ? [group, ...groups] : [...groups, group];
}

/**
 * Whether the goal rows lead: when nothing else matched, or when the query reads like a sentence
 * (three words or more) and nothing is named exactly by it.
 */
export function goalsFirst(query: string, best: number): boolean {
  const words = foldSearchQuery(query).split(" ").filter(Boolean).length;
  return best === 0 || (words >= 3 && best < 3);
}

function searchGroups(input: FilterInput): Filtered {
  if (input.root && input.inSubmenu) {
    const scoped = searchGroups({ ...input, root: undefined });
    if (scoped.groups.length > 0 || input.query.trim().length === 0) return scoped;
    return searchGroups({ ...input, groups: input.root.groups, inSubmenu: false });
  }
  const actionsOnly = input.query.startsWith(">");
  const query = foldSearchQuery(actionsOnly ? input.query.slice(1) : input.query);
  if (query.length === 0) return { groups: actionsOnly ? input.groups.filter((g) => g.value === "actions") : [...input.groups], best: 0 };

  let base = [...input.groups];
  if (actionsOnly) base = base.filter((g) => g.value === "actions");
  else if (!input.inSubmenu) base = base.filter((g) => g.value !== "recent");
  const everything = !input.inSubmenu && !actionsOnly;
  if (everything) {
    if (input.root?.nested.length) base.push({ value: "nested", label: "Suggestions", items: input.root.nested });
    if (input.threads?.length) base.push({ value: "threads", label: "Threads", items: input.threads });
    if (input.projects?.length) base.push({ value: "projects", label: "Projects", items: input.projects });
    if (input.machines?.length) base.push({ value: "machines", label: "Machines", items: input.machines });
  }
  const now = input.now ?? Date.now();
  const ranked = rankSearchGroups(
    base.map((group) => ({ group, items: group.items })),
    (item, { typos }) => scoreItem(item, query, now, typos),
  );
  const threadsMatch = ranked.some(({ group }) => group.value === "threads");
  let best = 0;
  const groups = ranked.map(({ group, ranked: items }): PaletteGroup => {
    const limit = group.value === "threads" ? PALETTE_THREAD_LIMIT : group.value === "nested" || (everything && threadsMatch) ? PALETTE_SECONDARY_LIMIT : items.length;
    for (const { match } of items) best = Math.max(best, matchClassOf(match));
    return {
      value: group.value,
      label: group.label,
      items: items
        .toSorted((a, b) => Number(a.item.secondary ?? false) - Number(b.item.secondary ?? false))
        .slice(0, limit)
        .map(({ item, match }) => {
          // Hints are built only for the rows shown.
          if (match.ranges.length > 0 || !("explain" in match) || !match.explain) return item;
          const hint = match.explain();
          return hint ? { ...item, matchHint: hint } : item;
        }),
    };
  });
  return { groups, best };
}

// The same classes as `rankSearchGroups`: 3 a whole name, 2 a word start, 1 a typo, 0 inside a word.
function matchClassOf(match: Match): number {
  const q = match.quality;
  return q >= 1.5 ? 3 : q >= 0.7 ? 2 : q >= 0.4 ? 1 : 0;
}

/** The search box's hint. */
export function placeholderFor(view: PaletteView | null): string {
  return view?.placeholder ?? "Search agents, projects and actions, or say what to do…";
}
