/**
 * Placing the canvas graph: a tidy tree that grows to the right (machines on the left, then
 * projects, threads, and their satellites) or down (machines on top), with nodes of any size; the
 * order the user dragged cards into among their siblings; moving between cards from the keyboard;
 * and the view math (pan, zoom about a point, zoom to fit). Pure apart from the localStorage
 * helpers.
 */

export interface Point {
  x: number;
  y: number;
}

export interface Size {
  w: number;
  h: number;
}

export interface Rect extends Point, Size {}

/** Screen = world × k + (x, y). */
export interface View {
  x: number;
  y: number;
  k: number;
}

/**
 * Which way the tree grows: `horizontal` puts the machines on the left and work to their right
 * (depths are columns, siblings stack down); `vertical` puts the machines on top and work below
 * (depths are rows, siblings sit side by side).
 */
export type Direction = "horizontal" | "vertical";

export const DIRECTIONS: readonly Direction[] = ["horizontal", "vertical"];

export interface TreeLike {
  roots: readonly string[];
  nodes: ReadonlyMap<string, { children: readonly string[] }>;
}

export interface LayoutOptions {
  direction?: Direction;
  /**
   * Growing down: whether a node spreads its children side by side (a machine's projects) rather
   * than listing them down its column, indented (a project's threads, a thread's requests). The
   * roots always spread. Absent: everything spreads (a plain top-down tree).
   */
  spreads?: (key: string) => boolean;
  /** Between a node and its children (along the direction). */
  depthGap?: number;
  /** Between siblings. */
  siblingGap?: number;
  /** Between root trees. */
  rootGap?: number;
  /** Growing down: how far a listed child is indented from its parent. */
  indent?: number;
  /** Growing down: between a parent and the first child it lists, and between listed children. */
  listGap?: number;
}

export type Gaps = Required<Omit<LayoutOptions, "direction" | "spreads">>;

/** The gaps a direction lays out with by default (the canvas's comfortable density). */
export const GAPS: Readonly<Record<Direction, Gaps>> = {
  horizontal: { depthGap: 72, siblingGap: 20, rootGap: 64, indent: 0, listGap: 0 },
  vertical: { depthGap: 56, siblingGap: 32, rootGap: 72, indent: 28, listGap: 14 },
};

/** The compact density's gaps: about half, so more of the canvas fits on a screen. */
export const COMPACT_GAPS: Readonly<Record<Direction, Gaps>> = {
  horizontal: { depthGap: 44, siblingGap: 10, rootGap: 36, indent: 0, listGap: 0 },
  vertical: { depthGap: 32, siblingGap: 16, rootGap: 40, indent: 20, listGap: 8 },
};

/**
 * Top-left corners for every node reachable from the roots, so that no two cards ever overlap.
 *
 * Growing right, it's a tidy tree: each subtree takes the height of its children stacked (or its
 * own, if taller) and its root sits centred on them; depths are columns as wide as their widest
 * card, so cards line up.
 *
 * Growing down, it's a board: what spreads (the roots, a machine's projects) sits side by side in
 * rows as tall as their tallest card, its parent centred over them; everything else lists its
 * children down its column, indented, each child's own subtree before the next (a project's
 * threads, each with its requests and subagents under it), so a narrow screen reads it top to
 * bottom like Linear's board, a column per project.
 */
export function tidyLayout(tree: TreeLike, size: (key: string) => Size, opts: LayoutOptions = {}): Map<string, Point> {
  return opts.direction === "vertical" ? boardLayout(tree, size, opts) : treeLayout(tree, size, opts);
}

function treeLayout(tree: TreeLike, size: (key: string) => Size, opts: LayoutOptions): Map<string, Point> {
  const depthGap = opts.depthGap ?? GAPS.horizontal.depthGap;
  const siblingGap = opts.siblingGap ?? GAPS.horizontal.siblingGap;
  const rootGap = opts.rootGap ?? GAPS.horizontal.rootGap;
  const span = new Map<string, number>();
  const childBlock = new Map<string, number>();
  const depth = new Map<string, number>();
  const colW: number[] = [];

  const measure = (key: string, d: number): number => {
    const known = span.get(key);
    if (known !== undefined) return known;
    if (depth.has(key)) return 0;
    depth.set(key, d);
    const { w, h } = size(key);
    colW[d] = Math.max(colW[d] ?? 0, w);
    const kids = (tree.nodes.get(key)?.children ?? []).filter((c) => !depth.has(c));
    let block = 0;
    kids.forEach((c, i) => (block += measure(c, d + 1) + (i ? siblingGap : 0)));
    const s = Math.max(h, block);
    childBlock.set(key, block);
    span.set(key, s);
    return s;
  };
  for (const r of tree.roots) measure(r, 0);
  const colX = [0];
  for (let d = 1; d < colW.length; d++) colX[d] = colX[d - 1] + (colW[d - 1] ?? 0) + depthGap;

  const out = new Map<string, Point>();
  const place = (key: string, top: number) => {
    if (out.has(key)) return;
    const s = span.get(key)!;
    const d = depth.get(key)!;
    out.set(key, { x: colX[d], y: top + (s - size(key).h) / 2 });
    let y = top + (s - (childBlock.get(key) ?? 0)) / 2;
    for (const c of tree.nodes.get(key)?.children ?? []) {
      if (depth.get(c) !== d + 1 || out.has(c)) continue;
      place(c, y);
      y += span.get(c)! + siblingGap;
    }
  };

  let top = 0;
  for (const r of tree.roots) {
    if (out.has(r)) continue;
    place(r, top);
    top += span.get(r)! + rootGap;
  }
  return out;
}

function boardLayout(tree: TreeLike, size: (key: string) => Size, opts: LayoutOptions): Map<string, Point> {
  const g = GAPS.vertical;
  const depthGap = opts.depthGap ?? g.depthGap;
  const siblingGap = opts.siblingGap ?? g.siblingGap;
  const rootGap = opts.rootGap ?? g.rootGap;
  const indent = opts.indent ?? g.indent;
  const listGap = opts.listGap ?? g.listGap;
  const spreads = opts.spreads ?? (() => true);

  // Each node once, children after their parent (a key reached twice keeps its first place).
  const kids = new Map<string, string[]>();
  const depth = new Map<string, number>();
  // On a row: a root, or a child of something on a row that spreads. Rows line up: each is as
  // tall as its tallest card, and what hangs off a row starts below all of it.
  const onRow = new Set<string>();
  const rowH: number[] = [];
  const visit = (key: string, d: number, row: boolean) => {
    depth.set(key, d);
    if (row) {
      onRow.add(key);
      rowH[d] = Math.max(rowH[d] ?? 0, size(key).h);
    }
    const mine: string[] = [];
    for (const c of tree.nodes.get(key)?.children ?? []) if (!depth.has(c)) mine.push(c), depth.set(c, d + 1);
    kids.set(key, mine);
    const spread = spreads(key);
    for (const c of mine) visit(c, d + 1, row && spread);
  };
  const roots: string[] = [];
  for (const r of tree.roots) if (!depth.has(r)) roots.push(r), visit(r, 0, true);
  const head = (key: string) => (onRow.has(key) ? rowH[depth.get(key)!] : size(key).h);

  // The room each subtree takes.
  const box = new Map<string, Size>();
  const measure = (key: string): Size => {
    const s = size(key);
    const mine = kids.get(key)!;
    const child = mine.map(measure);
    let b: Size;
    if (!mine.length) b = { w: s.w, h: head(key) };
    else if (spreads(key)) {
      const row = child.reduce((sum, c, i) => sum + c.w + (i ? siblingGap : 0), 0);
      b = { w: Math.max(s.w, row), h: head(key) + depthGap + Math.max(...child.map((c) => c.h)) };
    } else {
      const col = child.reduce((sum, c, i) => sum + c.h + (i ? listGap : 0), 0);
      b = { w: Math.max(s.w, indent + Math.max(...child.map((c) => c.w))), h: head(key) + listGap + col };
    }
    box.set(key, b);
    return b;
  };
  roots.forEach(measure);

  const out = new Map<string, Point>();
  const place = (key: string, x: number, y: number) => {
    const s = size(key);
    const b = box.get(key)!;
    const mine = kids.get(key)!;
    if (spreads(key)) {
      out.set(key, { x: x + (b.w - s.w) / 2, y });
      const row = mine.reduce((sum, c, i) => sum + box.get(c)!.w + (i ? siblingGap : 0), 0);
      let cx = x + (b.w - row) / 2;
      for (const c of mine) {
        place(c, cx, y + head(key) + depthGap);
        cx += box.get(c)!.w + siblingGap;
      }
    } else {
      out.set(key, { x, y });
      let cy = y + head(key) + listGap;
      for (const c of mine) {
        place(c, x + indent, cy);
        cy += box.get(c)!.h + listGap;
      }
    }
  };
  let x = 0;
  for (const r of roots) {
    place(r, x, 0);
    x += box.get(r)!.w + rootGap;
  }
  return out;
}

/**
 * Whether `parent`'s children sit side by side (left to right) on the canvas rather than one under
 * another: growing down, the roots' (`ROOTS`) and those of whatever spreads.
 */
export function sideBySide(direction: Direction, parent: string, spreads: (key: string) => boolean = () => true): boolean {
  return direction === "vertical" && (parent === ROOTS || spreads(parent));
}

/** Where a card is among its siblings: its centre on the axis they're laid out along. */
export function centreAcross(p: Point, s: Size, across: boolean): number {
  return across ? p.x + s.w / 2 : p.y + s.h / 2;
}

// ── the user's arrangement ──

/** Where the roots' order is kept in an `Order`. */
export const ROOTS = "";

/**
 * How the user arranged the canvas: per parent key (`ROOTS` for the machines), its children in the
 * order they were put in. Cards only ever move among their siblings, so the layout stays tidy.
 */
export type Order = Readonly<Record<string, readonly string[]>>;

/**
 * `defaults` in the user's order: the children they placed keep their order, in the slots those
 * children take by default, and the ones they never placed (new work) keep their default place.
 */
export function mergeOrder(defaults: readonly string[], saved: readonly string[] | undefined): string[] {
  if (!saved?.length) return [...defaults];
  const here = new Set(defaults);
  const placed = saved.filter((k) => here.has(k));
  const isPlaced = new Set(placed);
  let i = 0;
  return defaults.map((k) => (isPlaced.has(k) ? placed[i++] : k));
}

/** The tree with every node's children (and the roots) in the user's order. */
export function arrange<T extends { children: readonly string[] }>(
  tree: { roots: readonly string[]; nodes: ReadonlyMap<string, T> },
  order: Order,
): TreeLike {
  const nodes = new Map<string, { children: readonly string[] }>();
  for (const [k, n] of tree.nodes) nodes.set(k, { children: order[k] ? mergeOrder(n.children, order[k]) : n.children });
  return { roots: mergeOrder(tree.roots, order[ROOTS]), nodes };
}

/** `list` with `key` moved to `index` (counted without `key`). */
export function moveTo(list: readonly string[], key: string, index: number): string[] {
  const rest = list.filter((k) => k !== key);
  rest.splice(Math.max(0, Math.min(rest.length, index)), 0, key);
  return rest;
}

/**
 * Where a sibling dragged with its centre at `at` (on the axis siblings are laid out along) drops:
 * after every other sibling whose centre comes before it.
 */
export function dropIndex(siblings: readonly string[], key: string, at: number, centre: (key: string) => number): number {
  let i = 0;
  for (const s of siblings) if (s !== key && centre(s) < at) i++;
  return i;
}

export type Arrow = "left" | "right" | "up" | "down";

/**
 * Where an arrow key moves from the card `from` (keyboard navigation between cards): the nearest
 * card that way. Cards in line with it (overlapping it across the arrow) come first, nearest
 * edge first, and of those as near, the first in reading order; with none in line, the nearest
 * by edge and offset. It follows the tree in either direction: growing right, ← is the parent, → a child,
 * ↑ / ↓ the cards above and below in the column; in the top-down board ↑ / ↓ walk a project's
 * column and ← / → go to the next column.
 */
export function nextCard(rects: ReadonlyMap<string, Rect>, from: string, arrow: Arrow): string | null {
  const a = rects.get(from);
  if (!a) return null;
  const sideways = arrow === "left" || arrow === "right";
  const sign = arrow === "right" || arrow === "down" ? 1 : -1;
  // Along the arrow and across it, as [start, end] ranges.
  const along = (r: Rect): [number, number] => (sideways ? [r.x, r.x + r.w] : [r.y, r.y + r.h]);
  const across = (r: Rect): [number, number] => (sideways ? [r.y, r.y + r.h] : [r.x, r.x + r.w]);
  const [a0, a1] = along(a);
  const [c0, c1] = across(a);
  let best: string | null = null;
  let bestScore = Infinity;
  let bestStart = Infinity;
  for (const [k, b] of rects) {
    if (k === from) continue;
    const [b0, b1] = along(b);
    // Its centre has to be further that way than this card's.
    if (sign * ((b0 + b1) / 2 - (a0 + a1) / 2) <= 0) continue;
    const gap = Math.max(0, sign > 0 ? b0 - a1 : a0 - b1);
    const [d0, d1] = across(b);
    const off = Math.max(0, d0 - c1, c0 - d1);
    const score = (off > 0 ? 1e6 + off * 2 : 0) + gap;
    // As near: the first in reading order (a parent's first child).
    if (score < bestScore - 0.5 || (Math.abs(score - bestScore) <= 0.5 && d0 < bestStart)) {
      bestScore = score;
      bestStart = d0;
      best = k;
    }
  }
  return best;
}

export function bounds(pos: ReadonlyMap<string, Point>, size: (key: string) => Size, only?: Iterable<string>): Rect | null {
  let x0 = Infinity;
  let y0 = Infinity;
  let x1 = -Infinity;
  let y1 = -Infinity;
  for (const key of only ?? pos.keys()) {
    const p = pos.get(key);
    if (!p) continue;
    const s = size(key);
    x0 = Math.min(x0, p.x);
    y0 = Math.min(y0, p.y);
    x1 = Math.max(x1, p.x + s.w);
    y1 = Math.max(y1, p.y + s.h);
  }
  return x0 === Infinity ? null : { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
}

export const MIN_ZOOM = 0.15;
export const MAX_ZOOM = 2;

export const clampZoom = (k: number) => Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, k));

/**
 * The view that shows `r` whole, at most `maxK`. With `minK` (a phone, where everything at once
 * would be unreadable) it never zooms out past that; what doesn't fit then starts at the top left,
 * or, given an `anchor` (the first machine) that the top left wouldn't show, centred on it.
 */
export function fitView(r: Rect, vw: number, vh: number, pad = 48, maxK = 1, minK = 0, anchor?: Rect): View {
  const k = clampZoom(Math.max(minK, Math.min(maxK, (vw - pad * 2) / Math.max(1, r.w), (vh - pad * 2) / Math.max(1, r.h))));
  // On one axis: centred if it fits; else from the start, moved on just enough to show the anchor.
  const axis = (start: number, extent: number, view: number, a?: { start: number; extent: number }) => {
    if (extent * k <= view - pad * 2) return view / 2 - (start + extent / 2) * k;
    const first = pad - start * k;
    if (!a) return first;
    const last = view - pad - (start + extent) * k;
    // From the start if that shows the anchor whole; else the anchor centred (its start, if it's
    // bigger than the view), never past either end.
    if ((a.start + a.extent) * k + first <= view - pad) return first;
    const centred = a.extent * k > view - pad * 2 ? pad - a.start * k : view / 2 - (a.start + a.extent / 2) * k;
    return Math.max(last, Math.min(first, centred));
  };
  return {
    k,
    x: axis(r.x, r.w, vw, anchor && { start: anchor.x, extent: anchor.w }),
    y: axis(r.y, r.h, vh, anchor && { start: anchor.y, extent: anchor.h }),
  };
}

/** Zoom to `k` keeping the world point under screen point (sx, sy) where it is. */
export function zoomAt(v: View, k: number, sx: number, sy: number): View {
  const nk = clampZoom(k);
  const f = nk / v.k;
  return { k: nk, x: sx - (sx - v.x) * f, y: sy - (sy - v.y) * f };
}

export const toWorld = (v: View, sx: number, sy: number): Point => ({ x: (sx - v.x) / v.k, y: (sy - v.y) / v.k });

// ── remembered on this device ──

const ORDER_KEY = (owner: string) => `roll:agents:order:${owner}`;
const VIEW_KEY = (owner: string) => `roll:agents:view:${owner}`;
/** Parents whose order is kept, at most (the least recently arranged go first). */
const MAX_PARENTS = 500;

/** The order as a document: what's kept here and synced to roll. Anything malformed is dropped. */
export function parseOrder(v: unknown): Record<string, string[]> {
  const out: Record<string, string[]> = {};
  if (!v || typeof v !== "object" || Array.isArray(v)) return out;
  for (const [k, list] of Object.entries(v as Record<string, unknown>)) {
    if (Array.isArray(list)) out[k] = list.filter((x): x is string => typeof x === "string").slice(0, 1000);
  }
  return out;
}

/** `order` with `parent`'s children set to `list`, most recent last, bounded. */
export function withOrder(order: Order, parent: string, list: readonly string[]): Record<string, string[]> {
  const next: Record<string, string[]> = {};
  for (const [k, v] of Object.entries(order)) if (k !== parent) next[k] = [...v];
  next[parent] = [...list];
  const keys = Object.keys(next);
  for (const k of keys.slice(0, Math.max(0, keys.length - MAX_PARENTS))) delete next[k];
  return next;
}

function readJson(key: string): unknown {
  try {
    const raw = localStorage.getItem(key);
    return raw ? JSON.parse(raw) : null;
  } catch {
    return null;
  }
}

function writeJson(key: string, v: unknown) {
  try {
    if (v === null) localStorage.removeItem(key);
    else localStorage.setItem(key, JSON.stringify(v));
  } catch {
    // Private mode or full: it just isn't remembered.
  }
}

export function loadOrder(owner: string): Record<string, string[]> {
  // Free positions (from before cards only moved among their siblings) made overlaps: forget them.
  writeJson(`roll:agents:positions:${owner}`, null);
  return parseOrder(readJson(ORDER_KEY(owner)));
}

export function saveOrder(owner: string, order: Order) {
  writeJson(ORDER_KEY(owner), Object.keys(order).length ? order : null);
}

/** The view is per device: a phone and a laptop frame the canvas differently. */
export function loadView(owner: string): View | null {
  const v = readJson(VIEW_KEY(owner)) as View | null;
  return v && [v.x, v.y, v.k].every(Number.isFinite) ? { x: v.x, y: v.y, k: clampZoom(v.k) } : null;
}

export function saveView(owner: string, v: View | null) {
  writeJson(VIEW_KEY(owner), v && { x: Math.round(v.x), y: Math.round(v.y), k: Math.round(v.k * 1000) / 1000 });
}
