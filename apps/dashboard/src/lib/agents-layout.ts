/**
 * Placing the canvas graph: a tidy tree that grows to the right (machines on the left, then
 * projects, threads, and their satellites), with nodes of any size; manual positions the user
 * dragged to, remembered per owner; and the view math (pan, zoom about a point, zoom to fit).
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
 * its children stacked (or its own, if taller) and its root sits centred on them.
 */
export function tidyLayout(tree: TreeLike, size: (key: string) => Size, opts: LayoutOptions = {}): Map<string, Point> {
  const gapX = opts.gapX ?? 72;
  const gapY = opts.gapY ?? 20;
  const rootGap = opts.rootGap ?? 64;
  const span = new Map<string, number>();
  const childBlock = new Map<string, number>();
  const visiting = new Set<string>();

  const measure = (key: string): number => {
    const known = span.get(key);
    if (known !== undefined) return known;
    if (visiting.has(key)) return 0;
    visiting.add(key);
    const kids = tree.nodes.get(key)?.children ?? [];
    let block = 0;
    kids.forEach((c, i) => (block += measure(c) + (i ? gapY : 0)));
    const s = Math.max(size(key).h, block);
    childBlock.set(key, block);
    span.set(key, s);
    return s;
  };

  const out = new Map<string, Point>();
  const place = (key: string, x: number, top: number) => {
    if (out.has(key)) return;
    const s = span.get(key)!;
    const { w, h } = size(key);
    out.set(key, { x, y: top + (s - h) / 2 });
    const kids = tree.nodes.get(key)?.children ?? [];
    let y = top + (s - (childBlock.get(key) ?? 0)) / 2;
    for (const c of kids) {
      place(c, x + w + gapX, y);
      y += span.get(c)! + gapY;
    }
  };

  let top = 0;
  for (const r of tree.roots) {
    measure(r);
    place(r, 0, top);
    top += span.get(r)! + rootGap;
  }
  return out;
}

/**
 * Apply the user's manual positions: a dragged node goes where it was dropped, and everything
 * under it moves with it (unless it was dragged itself).
 */
export function withManual(tree: TreeLike, auto: ReadonlyMap<string, Point>, manual: Readonly<Record<string, Point>>): Map<string, Point> {
  const out = new Map<string, Point>();
  const walk = (key: string, dx: number, dy: number) => {
    const a = auto.get(key);
    if (!a || out.has(key)) return;
    const m = manual[key];
    if (m) {
      dx = m.x - a.x;
      dy = m.y - a.y;
    }
    out.set(key, { x: a.x + dx, y: a.y + dy });
    for (const c of tree.nodes.get(key)?.children ?? []) walk(c, dx, dy);
  };
  for (const r of tree.roots) walk(r, 0, 0);
  return out;
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

/** The view that shows `r` inside a `vw`×`vh` viewport with `pad` around it, at most at `maxK`. */
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

// ── remembered positions ──

const POS_KEY = (owner: string) => `roll:agents:positions:${owner}`;
const MAX_REMEMBERED = 500;

export function loadPositions(owner: string): Record<string, Point> {
  try {
    const raw = localStorage.getItem(POS_KEY(owner));
    const v = raw ? (JSON.parse(raw) as unknown) : null;
    if (!v || typeof v !== "object") return {};
    const out: Record<string, Point> = {};
    for (const [k, p] of Object.entries(v as Record<string, unknown>)) {
      const q = p as Point;
      if (q && Number.isFinite(q.x) && Number.isFinite(q.y)) out[k] = { x: q.x, y: q.y };
    }
    return out;
  } catch {
    return {};
  }
}

export function savePositions(owner: string, positions: Record<string, Point>) {
  try {
    const entries = Object.entries(positions);
    const kept = entries.slice(Math.max(0, entries.length - MAX_REMEMBERED));
    if (kept.length) localStorage.setItem(POS_KEY(owner), JSON.stringify(Object.fromEntries(kept)));
    else localStorage.removeItem(POS_KEY(owner));
  } catch {
    // Private mode or full: positions just aren't remembered.
  }
}
