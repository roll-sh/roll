/**
 * Placing the canvas graph: a tidy tree that grows to the right (machines on the left, then
 * projects, threads, and their satellites), with nodes of any size; the order the user dragged
 * cards into among their siblings; and the view math (pan, zoom about a point, zoom to fit).
 * Pure apart from the localStorage helpers.
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

export interface TreeLike {
  roots: readonly string[];
  nodes: ReadonlyMap<string, { children: readonly string[] }>;
}

export interface LayoutOptions {
  /** Between a node and its children. */
  gapX?: number;
  /** Between siblings. */
  gapY?: number;
  /** Between root trees. */
  rootGap?: number;
}

/**
 * Top-left corners for every node reachable from the roots. Each subtree takes the height of
 * its children stacked (or its own, if taller) and its root sits centred on them. Depths are
 * columns as wide as their widest card, so cards line up and no two ever overlap.
 */
export function tidyLayout(tree: TreeLike, size: (key: string) => Size, opts: LayoutOptions = {}): Map<string, Point> {
  const gapX = opts.gapX ?? 72;
  const gapY = opts.gapY ?? 20;
  const rootGap = opts.rootGap ?? 64;
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
    kids.forEach((c, i) => (block += measure(c, d + 1) + (i ? gapY : 0)));
    const s = Math.max(h, block);
    childBlock.set(key, block);
    span.set(key, s);
    return s;
  };
  for (const r of tree.roots) measure(r, 0);
  const colX = [0];
  for (let d = 1; d < colW.length; d++) colX[d] = colX[d - 1] + (colW[d - 1] ?? 0) + gapX;

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
      y += span.get(c)! + gapY;
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

/** Where a sibling dragged with its centre at `y` drops: after every other sibling whose centre is above it. */
export function dropIndex(siblings: readonly string[], key: string, y: number, centre: (key: string) => number): number {
  let i = 0;
  for (const s of siblings) if (s !== key && centre(s) < y) i++;
  return i;
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
 * where the machines are.
 */
export function fitView(r: Rect, vw: number, vh: number, pad = 48, maxK = 1, minK = 0): View {
  const k = clampZoom(Math.max(minK, Math.min(maxK, (vw - pad * 2) / Math.max(1, r.w), (vh - pad * 2) / Math.max(1, r.h))));
  const x = r.w * k > vw - pad * 2 ? pad - r.x * k : vw / 2 - (r.x + r.w / 2) * k;
  const y = r.h * k > vh - pad * 2 ? pad - r.y * k : vh / 2 - (r.y + r.h / 2) * k;
  return { k, x, y };
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
