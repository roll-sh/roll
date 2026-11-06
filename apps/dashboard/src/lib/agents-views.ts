/**
 * Canvas views (docs/code/agents.md, "Canvas"): the canvas as a workspace the owner composes.
 * "All" is the whole canvas; every other view is a named filter over it (machines, projects,
 * thread status, providers, a text match) with its own sibling order, its own pinned cards (a
 * conversation, a diff or a terminal shown big on the canvas beside its thread) and its own display
 * options (which way the tree grows). All of it is the owner's canvas document, kept by roll
 * (`AgentsServices.canvas`):
 *
 *     { order, pins?, display?, views? }      views: [{ id, name, filter, order?, pins?, display? }]
 *
 * `order`, `pins` and `display` are All's; a document from before views (`{ order }`) still parses,
 * a document without `views` shows the built-in ones, and one without `display` shows the defaults. Which view is showing and how it's framed
 * are per device (`loadCurrentView`, the canvas's own view memory). Pure apart from the
 * localStorage helpers.
 */

import { isRunning, MAX_CANVAS, type Thread } from "./agents-api";
import { keyOf, type Graph, type GraphNode } from "./agents-graph";
import { DIRECTIONS, parseOrder, withOrder, type Direction, type Order } from "./agents-layout";
import type { AgentsData } from "./agents-store";

export type StatusFilter = "running" | "waiting" | "error" | "recent";
export const STATUS_FILTERS: readonly StatusFilter[] = ["running", "waiting", "error", "recent"];
export const statusFilterLabel: Record<StatusFilter, string> = { running: "Running", waiting: "Waiting on me", error: "Errors", recent: "Recent" };

/**
 * What a view shows. Each field narrows it (they all have to match); within a list any entry
 * matches. Empty or absent: no narrowing.
 */
export interface ViewFilter {
  machines?: string[];
  projects?: string[];
  status?: StatusFilter[];
  providers?: string[];
  /** Matched, case-insensitively, against a thread's title, its project's name and its machine's name. */
  text?: string;
}

export type PinKind = "conversation" | "diff" | "terminal";

/** A card pinned onto a view: a thread's conversation or diff (`id` a thread), or a terminal. */
export interface Pin {
  kind: PinKind;
  id: string;
  w: number;
  h: number;
}

/**
 * How a view is shown (Linear's display options), kept with the view so every device agrees.
 * Absent fields are the defaults.
 */
export interface Display {
  /** On a wide screen; `horizontal` when absent. */
  direction?: Direction;
  /** On a phone, chosen apart (a narrow screen reads top-down); `vertical` when absent. */
  phoneDirection?: Direction;
  /** `compact`: smaller cards (the title, status and what it's doing) closer together; comfortable when absent. */
  density?: "compact";
  /** `project`: projects are the anchors (no machine cards); by machine when absent. */
  group?: "project";
  /** What's left off the canvas: threads with nothing going on, terminals. */
  hide?: Hideable[];
  /**
   * `all`: every project gets a card. When absent a project with nothing on the canvas isn't
   * drawn; its machine lists it ("3 more projects", `collapseUnused`).
   */
  projects?: "all";
  /** Projects pinned onto this view: they keep their card with nothing under it. */
  keep?: string[];
  /** The same threads as Linear's list or board instead of the canvas (`canvas` when absent). */
  layout?: "list" | "board";
  /** List and board: the groups (the board's columns); `status` when absent. */
  groupBy?: GroupBy;
  /** List and board: groups inside each group (the board's swimlanes); none when absent. */
  subGroupBy?: GroupBy;
  /** List and board: the order inside a group; the newest activity first when absent. */
  sort?: Sort;
}

export type Layout = "canvas" | "list" | "board";
export const LAYOUTS: readonly Layout[] = ["canvas", "list", "board"];

/** How the list and the board group threads (`lib/agents-collection.ts`). */
export type GroupBy = "none" | "status" | "project" | "machine" | "agent" | "pr";
export const GROUP_BYS: readonly GroupBy[] = ["status", "project", "machine", "agent", "pr", "none"];

export type SortKey = "activity" | "created" | "title" | "status" | "project" | "machine" | "agent" | "changes" | "cost";
export const SORT_KEYS: readonly SortKey[] = ["activity", "created", "title", "status", "project", "machine", "agent", "changes", "cost"];

export interface Sort {
  key: SortKey;
  dir: "asc" | "desc";
}

export const layoutOf = (d: Display): Layout => d.layout ?? "canvas";

/** The display showing the view as the canvas, a list or a board (everything else kept). */
export function withLayout(d: Display, layout: Layout): Display {
  const { layout: _, ...rest } = d;
  return layout === "canvas" ? rest : { ...rest, layout };
}
export const groupByOf = (d: Display): GroupBy => d.groupBy ?? "status";
export const subGroupByOf = (d: Display): GroupBy => (d.subGroupBy && d.subGroupBy !== groupByOf(d) ? d.subGroupBy : "none");
export const sortOf = (d: Display): Sort => d.sort ?? { key: "activity", dir: "desc" };

/** How many projects a view can pin. */
export const MAX_KEPT = 64;

/** What the display can leave off: `idle` threads (not working, not waiting, nothing unseen), `terminals`. */
export type Hideable = "idle" | "terminals";
export const HIDEABLE: readonly Hideable[] = ["idle", "terminals"];

export interface SavedView {
  /** `v_…`. */
  id: string;
  name: string;
  filter: ViewFilter;
  order: Order;
  pins: Pin[];
  display: Display;
}

export interface CanvasDoc {
  /** All's order. */
  order: Order;
  /** All's pins. */
  pins: Pin[];
  /** All's display. */
  display: Display;
  views: SavedView[];
}

/** The view everything else is a narrowing of; its id in `CanvasDoc` terms. */
export const ALL = "all";

/** A view as the canvas shows it (All included). */
export interface ViewLike {
  id: string;
  name: string;
  filter: ViewFilter;
  order: Order;
  pins: readonly Pin[];
  display: Display;
}

// ── bounds (the document stays well under the relay's 64 KiB) ──

export const MAX_VIEWS = 16;
export const MAX_PINS = 12;
export const MAX_NAME = 40;
export const MAX_TEXT = 120;
const MAX_FILTER_IDS = 16;
/** Parents whose order a view keeps (All keeps `withOrder`'s). */
const MAX_VIEW_PARENTS = 120;
/** What `encodeDoc` aims at: the relay's limit with room to spare. */
export const DOC_BUDGET = MAX_CANVAS - 4 * 1024;

export const PIN_MIN: Readonly<Record<PinKind, { w: number; h: number }>> = {
  conversation: { w: 380, h: 320 },
  diff: { w: 420, h: 280 },
  terminal: { w: 420, h: 240 },
};
export const PIN_MAX = { w: 1600, h: 1200 };
export const PIN_DEFAULT: Readonly<Record<PinKind, { w: number; h: number }>> = {
  conversation: { w: 520, h: 600 },
  diff: { w: 680, h: 520 },
  terminal: { w: 680, h: 420 },
};

export const clampPinSize = (kind: PinKind, w: number, h: number) => ({
  w: Math.round(Math.min(PIN_MAX.w, Math.max(PIN_MIN[kind].w, Number.isFinite(w) ? w : PIN_DEFAULT[kind].w))),
  h: Math.round(Math.min(PIN_MAX.h, Math.max(PIN_MIN[kind].h, Number.isFinite(h) ? h : PIN_DEFAULT[kind].h))),
});

/** The built-in views a canvas starts with (until the owner changes the list). Stable ids, so every device agrees before the first save. */
export const DEFAULT_VIEWS: readonly SavedView[] = [
  { id: "v_waiting", name: "Waiting on me", filter: { status: ["waiting"] }, order: {}, pins: [], display: {} },
  { id: "v_running", name: "Running", filter: { status: ["running"] }, order: {}, pins: [], display: {} },
];

export const emptyDoc = (): CanvasDoc => ({ order: {}, pins: [], display: {}, views: DEFAULT_VIEWS.map(copyView) });

function copyView(v: SavedView): SavedView {
  return { ...v, filter: copyFilter(v.filter), order: { ...v.order }, pins: v.pins.map((p) => ({ ...p })), display: { ...v.display } };
}

function copyFilter(f: ViewFilter): ViewFilter {
  const out: ViewFilter = {};
  for (const k of ["machines", "projects", "status", "providers"] as const) if (f[k]?.length) (out as Record<string, unknown>)[k] = [...f[k]!];
  if (f.text?.trim()) out.text = f.text;
  return out;
}

// ── parsing (anything malformed is dropped, never fatal) ──

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const strings = (v: unknown, max: number, len = 200) =>
  Array.isArray(v) ? [...new Set(v.filter((x): x is string => typeof x === "string" && x.length > 0 && x.length <= len))].slice(0, max) : [];

export function parseFilter(v: unknown): ViewFilter {
  if (!isObject(v)) return {};
  const out: ViewFilter = {};
  for (const k of ["machines", "projects", "providers"] as const) {
    const list = strings(v[k], MAX_FILTER_IDS);
    if (list.length) out[k] = list;
  }
  const status = strings(v.status, STATUS_FILTERS.length).filter((s): s is StatusFilter => (STATUS_FILTERS as readonly string[]).includes(s));
  if (status.length) out.status = status;
  if (typeof v.text === "string" && v.text.trim()) out.text = v.text.slice(0, MAX_TEXT);
  return out;
}

export function parsePins(v: unknown): Pin[] {
  if (!Array.isArray(v)) return [];
  const out: Pin[] = [];
  const seen = new Set<string>();
  for (const p of v) {
    if (!isObject(p) || typeof p.id !== "string" || !p.id || p.id.length > 200) continue;
    const kind = p.kind;
    if (kind !== "conversation" && kind !== "diff" && kind !== "terminal") continue;
    const key = `${kind}:${p.id}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({ kind, id: p.id, ...clampPinSize(kind, Number(p.w), Number(p.h)) });
    if (out.length === MAX_PINS) break;
  }
  return out;
}

// ── display ──

const isDirection = (v: unknown): v is Direction => (DIRECTIONS as readonly unknown[]).includes(v);

/** Display options from a document: unknown values are dropped (they show the default). */
export function parseDisplay(v: unknown): Display {
  if (!isObject(v)) return {};
  const out: Display = {};
  if (isDirection(v.direction)) out.direction = v.direction;
  if (isDirection(v.phoneDirection)) out.phoneDirection = v.phoneDirection;
  if (v.density === "compact") out.density = "compact";
  if (v.group === "project") out.group = "project";
  const hide = strings(v.hide, HIDEABLE.length).filter((h): h is Hideable => (HIDEABLE as readonly string[]).includes(h));
  if (hide.length) out.hide = hide;
  if (v.projects === "all") out.projects = "all";
  const keep = strings(v.keep, MAX_KEPT, 64);
  if (keep.length) out.keep = keep;
  if (v.layout === "list" || v.layout === "board") out.layout = v.layout;
  const isGroup = (g: unknown): g is GroupBy => (GROUP_BYS as readonly unknown[]).includes(g);
  if (isGroup(v.groupBy) && v.groupBy !== "status") out.groupBy = v.groupBy;
  if (isGroup(v.subGroupBy) && v.subGroupBy !== "none" && v.subGroupBy !== (out.groupBy ?? "status")) out.subGroupBy = v.subGroupBy;
  if (isObject(v.sort) && (SORT_KEYS as readonly unknown[]).includes(v.sort.key) && (v.sort.dir === "asc" || v.sort.dir === "desc") && !(v.sort.key === "activity" && v.sort.dir === "desc"))
    out.sort = { key: v.sort.key as SortKey, dir: v.sort.dir };
  return out;
}

/** Whether a display is all defaults on every screen (pinned projects and the layout aren't display choices). */
export const isDefaultDisplay = (d: Display) => Object.keys(parseDisplay(d)).every((k) => k === "keep" || k === "layout");

/** The display back to its defaults, keeping the projects pinned onto the view and its layout. */
export const defaultDisplay = (d: Display): Display => ({ ...(d.keep?.length ? { keep: d.keep } : {}), ...(d.layout ? { layout: d.layout } : {}) });

/** Pin a project onto the view (it keeps its card with no threads), or unpin it. */
export function keepProject(d: Display, project: string, on: boolean): Display {
  const rest = (d.keep ?? []).filter((p) => p !== project);
  const keep = on ? [...rest, project].slice(-MAX_KEPT) : rest;
  const { keep: _, ...out } = d;
  return keep.length ? { ...out, keep } : out;
}

/**
 * The graph as a display shows it: grouped by project (machine cards gone, projects the roots in
 * name order, a machine's own terminals with it), without terminals, without idle threads. An
 * idle thread stays while anything under it isn't (a running fork) or a card of it is pinned.
 */
export function applyDisplay(graph: Graph, display: Display, opts: { idle: (thread: string) => boolean; name?: (key: string) => string }): Graph {
  const hide = new Set(display.hide ?? []);
  if (!hide.size && display.group !== "project") return graph;
  const drop = new Set<string>();
  const dropTree = (k: string) => {
    drop.add(k);
    for (const c of graph.nodes.get(k)?.children ?? []) dropTree(c);
  };
  if (hide.has("terminals")) for (const n of graph.nodes.values()) if (n.kind === "terminal") drop.add(n.key);
  if (hide.has("idle")) {
    // Whether a thread (or anything under it) should stay.
    const stays = (k: string): boolean => {
      const n = graph.nodes.get(k)!;
      const kids = n.children.map((c) => graph.nodes.get(c)!).filter(Boolean);
      const own = n.kind !== "thread" || !opts.idle(n.id) || kids.some((c) => c.kind === "pin" || c.kind === "request");
      // Every child is visited, so every idle thread below is decided too.
      const below = kids.filter((c) => c.kind === "thread").map((c) => stays(c.key));
      const keep = own || below.some(Boolean);
      if (!keep) dropTree(k);
      return keep;
    };
    for (const n of graph.nodes.values()) if (n.kind === "thread" && graph.nodes.get(n.parent ?? "")?.kind !== "thread") stays(n.key);
  }
  let roots = graph.roots;
  if (display.group === "project") {
    roots = [];
    for (const r of graph.roots) {
      drop.add(r);
      for (const c of graph.nodes.get(r)?.children ?? []) if (graph.nodes.get(c)?.kind === "project") roots.push(c);
        else dropTree(c);
    }
    const name = opts.name ?? ((k: string) => k);
    roots = roots.map((k, i) => [k, i] as const).sort((a, b) => name(a[0]).localeCompare(name(b[0])) || a[1] - b[1]).map(([k]) => k);
  }
  const nodes = new Map<string, GraphNode>();
  for (const n of graph.nodes.values()) {
    if (drop.has(n.key)) continue;
    const parent = n.parent && !drop.has(n.parent) ? n.parent : null;
    nodes.set(n.key, { ...n, parent, children: n.children.filter((c) => !drop.has(c)) });
  }
  return { roots: roots.filter((r) => nodes.has(r)), nodes, edges: graph.edges.filter((e) => nodes.has(e.from) && nodes.has(e.to)) };
}

/** Which way a view's tree grows on this screen: the owner's choice for it, else across on a wide screen and down on a phone. */
export function directionOf(display: Display, phone: boolean): Direction {
  return phone ? (display.phoneDirection ?? "vertical") : (display.direction ?? "horizontal");
}

/** The display with this screen's direction set (a phone's is kept apart from a wide screen's). */
export function withDirection(display: Display, phone: boolean, direction: Direction): Display {
  return phone ? { ...display, phoneDirection: direction } : { ...display, direction };
}

const cleanName = (s: string) => s.replace(/\s+/g, " ").trim().slice(0, MAX_NAME);

/** The canvas document from roll (or this device). An old `{ order }` document parses; without `views`, the built-in views show. */
export function parseDoc(v: unknown): CanvasDoc {
  if (!isObject(v)) return emptyDoc();
  const views: SavedView[] = [];
  if (Array.isArray(v.views)) {
    const ids = new Set<string>();
    for (const x of v.views) {
      if (!isObject(x) || typeof x.id !== "string" || !/^v_[A-Za-z0-9_-]{1,32}$/.test(x.id) || ids.has(x.id)) continue;
      const name = typeof x.name === "string" ? cleanName(x.name) : "";
      if (!name) continue;
      ids.add(x.id);
      views.push({ id: x.id, name, filter: parseFilter(x.filter), order: parseOrder(x.order), pins: parsePins(x.pins), display: parseDisplay(x.display) });
      if (views.length === MAX_VIEWS) break;
    }
  }
  return {
    order: parseOrder(v.order),
    pins: parsePins(v.pins),
    display: parseDisplay(v.display),
    views: Array.isArray(v.views) ? views : DEFAULT_VIEWS.map(copyView),
  };
}

const byteLength = (s: string) => new TextEncoder().encode(s).length;

/**
 * The document as it's saved: empty parts left out, and bounded to `budget` bytes of JSON. Over
 * it, the arrangements least recently made go first (views' before All's); views and pins are
 * already bounded in number, so dropping order always gets there.
 */
export function encodeDoc(doc: CanvasDoc, budget = DOC_BUDGET): Record<string, unknown> {
  const order = { ...doc.order } as Record<string, readonly string[]>;
  const views = doc.views.slice(0, MAX_VIEWS).map((v) => ({ ...v, order: { ...v.order } as Record<string, readonly string[]> }));
  const build = () => {
    const out: Record<string, unknown> = { order };
    if (doc.pins.length) out.pins = doc.pins.slice(0, MAX_PINS);
    const display = parseDisplay(doc.display);
    if (Object.keys(display).length) out.display = display;
    out.views = views.map((v) => {
      const o: Record<string, unknown> = { id: v.id, name: v.name, filter: copyFilter(v.filter) };
      if (Object.keys(v.order).length) o.order = v.order;
      if (v.pins.length) o.pins = v.pins.slice(0, MAX_PINS);
      const display = parseDisplay(v.display);
      if (Object.keys(display).length) o.display = display;
      return o;
    });
    return out;
  };
  let out = build();
  let size = byteLength(JSON.stringify(out));
  if (size <= budget) return out;
  // Oldest first: every view's arrangements (each in the order they were made), then All's.
  const victims: { from: Record<string, readonly string[]>; key: string }[] = [];
  for (const v of views) for (const key of Object.keys(v.order)) victims.push({ from: v.order, key });
  for (const key of Object.keys(order)) victims.push({ from: order, key });
  for (const { from, key } of victims) {
    if (size <= budget) break;
    size -= byteLength(JSON.stringify(key)) + byteLength(JSON.stringify(from[key])) + 2;
    delete from[key];
  }
  out = build();
  // Names and filters are bounded, so this only trims views in a pathological document.
  while (byteLength(JSON.stringify(out)) > budget && views.length) {
    views.pop();
    out = build();
  }
  return out;
}

// ── views ──

export function allView(doc: CanvasDoc): ViewLike {
  return { id: ALL, name: "All", filter: {}, order: doc.order, pins: doc.pins, display: doc.display };
}

/** Every view in the bar's order, All first. */
export function viewsOf(doc: CanvasDoc): ViewLike[] {
  return [allView(doc), ...doc.views];
}

export function viewById(doc: CanvasDoc, id: string): ViewLike {
  return (id !== ALL && doc.views.find((v) => v.id === id)) || allView(doc);
}

export function newViewId(rand: () => number = Math.random): string {
  let s = "";
  for (let i = 0; i < 10; i++) s += "abcdefghijklmnopqrstuvwxyz0123456789"[Math.floor(rand() * 36)];
  return `v_${s}`;
}

/**
 * The doc with a new view at the end (none past `MAX_VIEWS`: the doc comes back as it was). It
 * keeps the display it was made from, so saving what's on screen keeps it looking the same.
 */
export function addView(doc: CanvasDoc, name: string, filter: ViewFilter, id = newViewId(), display: Display = {}): CanvasDoc {
  const n = cleanName(name);
  if (!n || doc.views.length >= MAX_VIEWS || doc.views.some((v) => v.id === id)) return doc;
  return { ...doc, views: [...doc.views, { id, name: n, filter: copyFilter(parseFilter(filter)), order: {}, pins: [], display: parseDisplay(display) }] };
}

/** A view's (or All's) display set to `display`. */
export function setViewDisplay(doc: CanvasDoc, id: string, display: Display): CanvasDoc {
  const d = parseDisplay(display);
  if (id === ALL) return { ...doc, display: d };
  return { ...doc, views: doc.views.map((v) => (v.id === id ? { ...v, display: d } : v)) };
}

export function renameView(doc: CanvasDoc, id: string, name: string): CanvasDoc {
  const n = cleanName(name);
  if (!n) return doc;
  return { ...doc, views: doc.views.map((v) => (v.id === id ? { ...v, name: n } : v)) };
}

export function setViewFilter(doc: CanvasDoc, id: string, filter: ViewFilter): CanvasDoc {
  return { ...doc, views: doc.views.map((v) => (v.id === id ? { ...v, filter: copyFilter(parseFilter(filter)) } : v)) };
}

export function deleteView(doc: CanvasDoc, id: string): CanvasDoc {
  return { ...doc, views: doc.views.filter((v) => v.id !== id) };
}

/** The view moved to `index` among the saved views (All always comes first). */
export function moveView(doc: CanvasDoc, id: string, index: number): CanvasDoc {
  const v = doc.views.find((x) => x.id === id);
  if (!v) return doc;
  const rest = doc.views.filter((x) => x.id !== id);
  rest.splice(Math.max(0, Math.min(rest.length, index)), 0, v);
  return { ...doc, views: rest };
}

/** A view's (or All's) order set to `order`. */
export function setViewOrder(doc: CanvasDoc, id: string, order: Order): CanvasDoc {
  if (id === ALL) return { ...doc, order };
  return {
    ...doc,
    views: doc.views.map((v) => {
      if (v.id !== id) return v;
      // Bounded like All's: the least recently arranged go first.
      let o: Order = order;
      const keys = Object.keys(o);
      if (keys.length > MAX_VIEW_PARENTS) {
        const last = keys[keys.length - 1];
        o = withOrder(Object.fromEntries(keys.slice(keys.length - MAX_VIEW_PARENTS).map((k) => [k, o[k]])), last, o[last]);
      }
      return { ...v, order: o };
    }),
  };
}

function setPins(doc: CanvasDoc, id: string, fn: (pins: readonly Pin[]) => Pin[]): CanvasDoc {
  if (id === ALL) return { ...doc, pins: fn(doc.pins) };
  return { ...doc, views: doc.views.map((v) => (v.id === id ? { ...v, pins: fn(v.pins) } : v)) };
}

const samePin = (a: Pick<Pin, "kind" | "id">, b: Pick<Pin, "kind" | "id">) => a.kind === b.kind && a.id === b.id;

export function isPinned(view: ViewLike, kind: PinKind, id: string): boolean {
  return view.pins.some((p) => samePin(p, { kind, id }));
}

/**
 * The doc with a card pinned onto a view. `exists` drops pins whose thread or terminal is gone; at
 * `MAX_PINS` the oldest pin makes room.
 */
export function pin(doc: CanvasDoc, view: string, kind: PinKind, id: string, exists: (p: Pin) => boolean = () => true): CanvasDoc {
  return setPins(doc, view, (pins) => {
    if (pins.some((p) => samePin(p, { kind, id }))) return [...pins];
    const kept = pins.filter(exists);
    return [...kept, { kind, id, ...PIN_DEFAULT[kind] }].slice(-MAX_PINS);
  });
}

export function unpin(doc: CanvasDoc, view: string, kind: PinKind, id: string): CanvasDoc {
  return setPins(doc, view, (pins) => pins.filter((p) => !samePin(p, { kind, id })));
}

export function resizePin(doc: CanvasDoc, view: string, kind: PinKind, id: string, w: number, h: number): CanvasDoc {
  return setPins(doc, view, (pins) => pins.map((p) => (samePin(p, { kind, id }) ? { ...p, ...clampPinSize(kind, w, h) } : p)));
}

// ── what a view shows ──

export const isEmptyFilter = (f: ViewFilter) => !f.machines?.length && !f.projects?.length && !threadFiltered(f);
/** The filter picks threads (not just machines and projects). */
const threadFiltered = (f: ViewFilter) => !!(f.status?.length || f.providers?.length || f.text?.trim());

const DAY = 24 * 60 * 60 * 1000;

function statusMatches(t: Thread, s: StatusFilter, now: number): boolean {
  switch (s) {
    case "running":
      return isRunning(t.status);
    case "waiting":
      return t.status === "waiting" || t.pending_requests > 0;
    case "error":
      return t.status === "error";
    case "recent":
      return now - t.updated_at < DAY;
  }
}

/** Whether a thread is one the filter picks (machines and projects included). */
export function threadMatches(data: AgentsData, t: Thread, machine: string, f: ViewFilter, now: number): boolean {
  if (f.machines?.length && !f.machines.includes(machine)) return false;
  if (f.projects?.length && !f.projects.includes(t.project)) return false;
  if (f.status?.length && !f.status.some((s) => statusMatches(t, s, now))) return false;
  if (f.providers?.length && !f.providers.includes(t.provider)) return false;
  const q = f.text?.trim().toLowerCase();
  if (q) {
    const hay = [t.title, data.projects.get(t.project)?.value.name, data.machines.get(machine)?.value.name];
    if (!hay.some((s) => s?.toLowerCase().includes(q))) return false;
  }
  return true;
}

/** Threads a view counts (the bar's badge): the ones it picks, archived ones aside. */
export function countView(data: AgentsData, f: ViewFilter, now: number): number {
  let n = 0;
  for (const r of data.threads.values()) if (r.value.archived_at === null && threadMatches(data, r.value, r.machine, f, now)) n++;
  return n;
}

/** Threads whose conversation is pinned on the view: their requests are answered inside the card. */
export function pinnedConversations(view: Pick<ViewLike, "pins">): Set<string> {
  return new Set(view.pins.filter((p) => p.kind === "conversation").map((p) => p.id));
}

/** A pinned card's canvas key: `pc:` conversation, `pd:` diff, `px:` terminal. */
export const pinKey = (p: Pick<Pin, "kind" | "id">) => `${p.kind === "conversation" ? "pc" : p.kind === "diff" ? "pd" : "px"}:${p.id}`;

/** The pin a `pin` node stands for. */
export function pinOf(view: Pick<ViewLike, "pins">, key: string): Pin | undefined {
  return view.pins.find((p) => pinKey(p) === key);
}

/**
 * The graph a view shows. A filter that picks threads keeps those threads, what they hang off
 * (their project, machine and parent threads, for context) and their satellites; a filter on
 * machines or projects alone keeps those whole. Pinned cards always show (with what they hang
 * off): a pinned conversation or diff as the first child of its thread, a pinned terminal in its
 * terminal card's place. Cards are only ever children in the tree, so the layout keeps them
 * apart.
 */
export function applyView(graph: Graph, data: AgentsData, view: Pick<ViewLike, "filter" | "pins">, now: number): Graph {
  const f = view.filter;
  const pins = view.pins.filter((p) => graph.nodes.has(p.kind === "terminal" ? keyOf.terminal(p.id) : keyOf.thread(p.id)));
  if (isEmptyFilter(f) && pins.length === 0) return graph;

  const keep = new Set<string>();
  const withAncestors = (key: string) => {
    for (let k: string | null = key; k && !keep.has(k); k = graph.nodes.get(k)?.parent ?? null) keep.add(k);
  };
  if (isEmptyFilter(f)) for (const k of graph.nodes.keys()) keep.add(k);
  else {
    const byThread = threadFiltered(f);
    for (const n of graph.nodes.values()) {
      if (n.kind === "thread") {
        const r = data.threads.get(n.id);
        if (r && threadMatches(data, r.value, n.machine, f, now)) withAncestors(n.key);
      } else if (!byThread && (n.kind === "project" ? (!f.machines?.length || f.machines.includes(n.machine)) && (!f.projects?.length || f.projects.includes(n.id)) : n.kind === "machine" && !f.projects?.length && (!f.machines?.length || f.machines.includes(n.id))))
        withAncestors(n.key);
    }
    // Satellites of what's kept (a thread's subagents, requests and terminals; a project's or
    // machine's terminals when the view isn't about threads).
    for (const n of graph.nodes.values()) {
      if (keep.has(n.key) || !n.parent || !keep.has(n.parent)) continue;
      const parent = graph.nodes.get(n.parent)!;
      if (n.kind === "subagent" || n.kind === "request" || (n.kind === "terminal" && (parent.kind === "thread" || !byThread))) keep.add(n.key);
    }
    // Nested subagents hang off kept subagents.
    for (const n of graph.nodes.values()) if (n.kind === "subagent" && n.parent && keep.has(n.parent)) keep.add(n.key);
  }
  for (const p of pins) withAncestors(p.kind === "terminal" ? keyOf.terminal(p.id) : keyOf.thread(p.id));

  const nodes = new Map<string, GraphNode>();
  for (const n of graph.nodes.values()) if (keep.has(n.key)) nodes.set(n.key, { ...n, children: n.children.filter((c) => keep.has(c)) });

  // Pins: a conversation or diff first under its thread (the conversation before the diff), a terminal in its own card's place.
  const sorted = [...pins].sort((a, b) => (a.kind === "conversation" ? 0 : 1) - (b.kind === "conversation" ? 0 : 1));
  const firsts = new Map<string, string[]>();
  for (const p of sorted) {
    const key = pinKey(p);
    if (p.kind === "terminal") {
      const t = nodes.get(keyOf.terminal(p.id))!;
      nodes.delete(t.key);
      nodes.set(key, { ...t, key, kind: "pin", children: [] });
      if (t.parent) {
        const parent = nodes.get(t.parent)!;
        parent.children = parent.children.map((c) => (c === t.key ? key : c));
      }
      continue;
    }
    const tk = keyOf.thread(p.id);
    const t = nodes.get(tk)!;
    nodes.set(key, { key, kind: "pin", id: p.id, machine: t.machine, parent: tk, children: [], busy: false, relation: null, tint: t.tint });
    firsts.set(tk, [...(firsts.get(tk) ?? []), key]);
  }
  for (const [tk, list] of firsts) {
    const t = nodes.get(tk)!;
    t.children = [...list, ...t.children];
  }

  const roots = graph.roots.filter((r) => nodes.has(r));
  const edges = [];
  for (const n of nodes.values()) if (n.parent && nodes.has(n.parent)) edges.push({ from: n.parent, to: n.key, busy: n.busy, dashed: n.relation === "fork", tint: n.tint });
  return { roots, nodes, edges };
}

/**
 * The graph narrowed to one card, everything under it and what it hangs off: "focus a project"
 * (the project, its machine above it, its threads and their satellites and pins). A key that
 * isn't on the graph leaves it whole.
 */
export function focusOn(graph: Graph, key: string): Graph {
  if (!graph.nodes.has(key)) return graph;
  const keep = new Set<string>();
  for (let k: string | null = graph.nodes.get(key)!.parent; k && !keep.has(k); k = graph.nodes.get(k)?.parent ?? null) keep.add(k);
  const down = [key];
  while (down.length) {
    const k = down.pop()!;
    if (keep.has(k) && k !== key) continue;
    keep.add(k);
    down.push(...(graph.nodes.get(k)?.children ?? []));
  }
  const nodes = new Map<string, GraphNode>();
  for (const n of graph.nodes.values()) if (keep.has(n.key)) nodes.set(n.key, { ...n, children: n.children.filter((c) => keep.has(c)) });
  return { roots: graph.roots.filter((r) => keep.has(r)), nodes, edges: graph.edges.filter((e) => keep.has(e.from) && keep.has(e.to)) };
}

/** The project a card belongs to (`p:…`): itself, or the project above it. */
export function projectOf(graph: Graph, key: string): string | null {
  for (let k: string | null = key; k; k = graph.nodes.get(k)?.parent ?? null) if (graph.nodes.get(k)?.kind === "project") return k;
  return null;
}

// ── remembered on this device ──

const DOC_KEY = (owner: string) => `roll:agents:canvas:${owner}`;
const OLD_ORDER_KEY = (owner: string) => `roll:agents:order:${owner}`;
const CURRENT_KEY = (owner: string) => `roll:agents:current-view:${owner}`;

function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function write(key: string, v: string | null) {
  try {
    if (v === null) localStorage.removeItem(key);
    else localStorage.setItem(key, v);
  } catch {
    // Private mode or full: it just isn't remembered.
  }
}

/** This device's copy of the canvas document (the order from before views counts as one). */
export function loadDoc(owner: string): CanvasDoc {
  // Free positions (from before cards only moved among their siblings) made overlaps: forget them.
  write(`roll:agents:positions:${owner}`, null);
  const raw = read(DOC_KEY(owner));
  if (raw) {
    try {
      return parseDoc(JSON.parse(raw));
    } catch {
      // Fall through to what's there from before.
    }
  }
  try {
    const old = read(OLD_ORDER_KEY(owner));
    return parseDoc({ order: old ? JSON.parse(old) : {} });
  } catch {
    return emptyDoc();
  }
}

export function saveDoc(owner: string, doc: CanvasDoc) {
  write(DOC_KEY(owner), JSON.stringify(encodeDoc(doc)));
  write(OLD_ORDER_KEY(owner), null);
}

/** Which view this device shows (each device keeps its own). */
export function loadCurrentView(owner: string): string {
  return read(CURRENT_KEY(owner)) || ALL;
}

export function saveCurrentView(owner: string, id: string) {
  write(CURRENT_KEY(owner), id === ALL ? null : id);
}

const ONLY_KEY = (owner: string) => `roll:agents:only:${owner}`;

/** The project each view is focused on ("Focus on this project only"), on this device. */
export function loadFocused(owner: string): Record<string, string> {
  try {
    const v = JSON.parse(read(ONLY_KEY(owner)) ?? "{}") as unknown;
    if (!isObject(v)) return {};
    const out: Record<string, string> = {};
    for (const [view, project] of Object.entries(v).slice(0, MAX_VIEWS + 1)) if (typeof project === "string" && project.length <= 64) out[view] = project;
    return out;
  } catch {
    return {};
  }
}

/** Focus a view on a project, or (`null`) on every project again; returns the new map. */
export function setFocused(owner: string, all: Record<string, string>, view: string, project: string | null): Record<string, string> {
  const next = { ...all };
  if (project) next[view] = project;
  else delete next[view];
  write(ONLY_KEY(owner), Object.keys(next).length ? JSON.stringify(next) : null);
  return next;
}

/** Where a view's frame (pan and zoom) is remembered on this device: All keeps the canvas's own. */
export const viewStorageKey = (owner: string, id: string) => (id === ALL ? owner : `${owner}#${id}`);
