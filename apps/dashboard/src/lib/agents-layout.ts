/**
 * Placing the canvas graph (`layoutCanvas`): machines, their projects as lanes, each project's
 * threads in a grid of cells every grid shares, growing right or down; how it wraps to the screen
 * (`planWraps`); the order the user dragged cards into among their siblings; moving between cards
 * from the keyboard; and the view math (pan, zoom about a point, zoom to fit). Pure apart from the
 * localStorage helpers.
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

/** Where an edge meets a full card: its header's middle. */
export const ANCHOR = (h: number) => Math.min(h / 2, 28);

/**
 * What a node's children are, to the layout: `lanes` (a machine's projects, a drilled project's
 * status sections) each take their own row growing right or their own column growing down;
 * `items` (a project's or a section's threads) fill a grid of cells. Anything else under a node
 * (a thread's requests, terminals and pinned cards) is stacked under it, inside its cell.
 */
export type Level = "lanes" | "items";

/** The room between things on the canvas (world px). */
export interface Spacing {
  /** Between a parent and its children, along the direction (machine → projects → threads). */
  depth: number;
  /** Between lanes (projects). */
  lane: number;
  /** Between sections: at least twice a grid's gaps, so a section reads as one. */
  section: number;
  /** Between machines. */
  root: number;
  /** A grid's gaps between columns and rows. */
  cellX: number;
  cellY: number;
  /** Between a card and what's stacked under it in its cell. */
  stack: number;
  /** Growing down: how far a project's threads sit in from it (their trunk runs down the indent). */
  indent: number;
  /** Growing down: between a card and the grid or list under it. */
  under: number;
  /** Between a section's header and its cards. */
  head: number;
}

export const SPACING: Readonly<Record<Direction, Spacing>> = {
  horizontal: { depth: 72, lane: 40, section: 80, root: 72, cellX: 28, cellY: 24, stack: 10, indent: 0, under: 16, head: 14 },
  vertical: { depth: 64, lane: 48, section: 80, root: 80, cellX: 28, cellY: 24, stack: 10, indent: 32, under: 16, head: 14 },
};

/** The compact density's room: about half, so more of the canvas fits on a screen. */
export const COMPACT_SPACING: Readonly<Record<Direction, Spacing>> = {
  horizontal: { depth: 44, lane: 24, section: 56, root: 40, cellX: 16, cellY: 14, stack: 8, indent: 0, under: 12, head: 10 },
  vertical: { depth: 44, lane: 28, section: 56, root: 48, cellX: 16, cellY: 14, stack: 8, indent: 24, under: 12, head: 10 },
};

/** How a tree is drawn: which way it grows, what each node's children are, and its spacing. */
export interface CanvasShape {
  direction: Direction;
  level: (key: string) => Level | null;
  /** Lanes drawn as a section: a header row over its cards, with no edge to each of them. */
  section?: (key: string) => boolean;
  spacing?: Spacing;
}

/**
 * How the canvas wraps to its screen (`planWraps`). `lines`: growing right, the cards to a row of
 * every items grid (0 or 1: a plain column; projects stack, so the widest grid sets the width);
 * growing down, the rows a grid takes at most, each taking the columns its cards need (projects
 * sit side by side, so the tallest sets the height). `lanes` 1 lists projects down the screen
 * instead of side by side (a phone); `roots`: machines to a column growing right or to a row
 * growing down (0: all in one; idle machines always share one stack, last).
 */
export interface Wraps {
  lines: number;
  lanes: 0 | 1;
  roots: number;
}

/** A plain column of threads under each project, one stack of machines. */
export const NO_WRAPS: Wraps = { lines: 0, lanes: 0, roots: 0 };

/** Where everything goes. */
export interface Placement {
  positions: Map<string, Point>;
  /**
   * Each edge drawn, as an orthogonal line from the parent's side to the child's (world px), keyed
   * `from>to`. An edge with none isn't drawn: a section's cards sit under its header, and a grid
   * row's later cards beside its first.
   */
  routes: Map<string, Point[]>;
  /** A section header's width: its cards'. */
  widths: Map<string, number>;
  /** How each parent's children are arranged, for dragging among them (`dropIndex`). */
  across: Map<string, boolean | "grid">;
}

/** A laid-out piece of the canvas, from its top-left. */
interface Block {
  w: number;
  h: number;
  at: [string, Point][];
  routes: [string, Point[]][];
  /** What an edge into the block meets: its row heads or lanes, with the line (`y`) and their top. */
  heads: { key: string; x: number; y: number; top: number }[];
  /** The heads sit side by side (a row of lanes): fed by a bus above them, not a trunk beside. */
  row: boolean;
  /** The line an edge into the block meets, from its top. */
  lead: number;
}

/** Where a satellite's connector and a top-down drop run, in from the card's left (world px). */
const RAIL = 16;

/**
 * Lay the canvas out: machines, then their projects (lanes), then each project's threads in a grid
 * (`Wraps.lines`) whose columns every grid shares (a column's width is its widest card's, so
 * projects' grids line up), each row as tall as its tallest cell. A thread's requests, terminals
 * and pins stack under it in its cell. Growing right, a parent sits beside its first child (their
 * edge straight) and a trunk in the gap before its children reaches each row's first card;
 * growing down, a parent's row of lanes hangs off a bus in the gap above them, dropping into each
 * card's top, and a project's grid hangs off a trunk down its indent. No two cards overlap.
 */
export function layoutCanvas(tree: TreeLike, size: (key: string) => Size, shape: CanvasShape, wraps: Wraps = NO_WRAPS): Placement {
  const horizontal = shape.direction !== "vertical";
  const g = shape.spacing ?? SPACING[shape.direction];
  const claimed = new Set<string>();
  const claim = (k: string) => tree.nodes.has(k) && !claimed.has(k) && (claimed.add(k), true);
  const kids = (k: string) => tree.nodes.get(k)?.children ?? [];
  const widths = new Map<string, number>();
  const across = new Map<string, boolean | "grid">();

  // A card and everything under it, stacked (the cell of a grid's item, or a lane with no level).
  const stack = (key: string): Block => {
    const at: [string, Point][] = [];
    const routes: [string, Point[]][] = [];
    let y = 0;
    let w = 0;
    const visit = (k: string, parent: string | null, from: number) => {
      const s = size(k);
      if (parent) routes.push([`${parent}>${k}`, [{ x: RAIL, y: from }, { x: RAIL, y }]]);
      at.push([k, { x: 0, y }]);
      w = Math.max(w, s.w);
      const bottom = y + s.h;
      y = bottom + g.stack;
      for (const c of kids(k)) if (claim(c)) visit(c, k, bottom);
    };
    visit(key, null, 0);
    const lead = ANCHOR(size(key).h);
    return { w, h: y - g.stack, at, routes, heads: [{ key, x: 0, y: lead, top: 0 }], row: false, lead };
  };

  // Every grid's cells first, so a column is as wide as its widest card in any grid.
  const cells = new Map<string, { mine: Block[]; cols: number }>();
  const colW: number[] = [];
  const lines = Math.floor(wraps.lines);
  const scan = (k: string) => {
    if (shape.level(k) === "items") {
      const mine: Block[] = [];
      for (const c of kids(k)) if (claim(c)) mine.push(stack(c));
      const cols = lines <= 0 ? 1 : Math.max(1, horizontal ? Math.min(lines, mine.length) : Math.ceil(mine.length / lines));
      cells.set(k, { mine, cols });
      mine.forEach((b, i) => (colW[i % cols] = Math.max(colW[i % cols] ?? 0, size(b.at[0][0]).w)));
      return;
    }
    for (const c of kids(k)) scan(c);
  };
  for (const r of new Set(tree.roots)) scan(r);
  const colX = colW.map((_, c) => colW.slice(0, c).reduce((s, w) => s + w + g.cellX, 0));

  const grid = (k: string): Block | null => {
    const { mine, cols: n } = cells.get(k) ?? { mine: [], cols: 1 };
    if (!mine.length) return null;
    across.set(k, n > 1 ? "grid" : false);
    const at: [string, Point][] = [];
    const routes: [string, Point[]][] = [];
    const heads: Block["heads"] = [];
    let y = 0;
    let w = 0;
    for (let r = 0; r * n < mine.length; r++) {
      const row = mine.slice(r * n, r * n + n);
      let right = -Infinity;
      let h = 0;
      row.forEach((b, c) => {
        // A cell wider than its column (a pinned conversation) pushes the rest of its row along.
        const x = Math.max(colX[c], right + g.cellX);
        for (const [key, p] of b.at) at.push([key, { x: x + p.x, y: y + p.y }]);
        for (const [key, pts] of b.routes) routes.push([key, pts.map((p) => ({ x: x + p.x, y: y + p.y }))]);
        right = x + Math.max(b.w, colW[c]);
        h = Math.max(h, b.h);
        if (!c) heads.push({ key: b.at[0][0], x: 0, y: y + b.lead, top: y });
      });
      w = Math.max(w, right);
      y += h + g.cellY;
    }
    return { w, h: y - g.cellY, at, routes, heads, row: false, lead: heads[0].y };
  };

  const add = (into: { at: [string, Point][]; routes: [string, Point[]][] }, b: Block, dx: number, dy: number) => {
    for (const [k, p] of b.at) into.at.push([k, { x: p.x + dx, y: p.y + dy }]);
    for (const [k, pts] of b.routes) into.routes.push([k, pts.map((p) => ({ x: p.x + dx, y: p.y + dy }))]);
  };

  // A lane (or a machine): its card with what it holds beside it (growing right) or under it.
  const lane = (k: string): Block => {
    const level = shape.level(k);
    if (!level) return stack(k);
    const child = level === "items" ? grid(k) : lanes(k);
    const s = size(k);
    if (shape.section?.(k)) {
      // A header row over its cards, as wide as they are; edges meet its label, never its rule.
      const w = Math.max(s.w, child?.w ?? 0);
      widths.set(k, w);
      const b: Block = { w, h: s.h, at: [[k, { x: 0, y: 0 }]], routes: [], heads: [{ key: k, x: 0, y: s.h / 2, top: 0 }], row: false, lead: s.h / 2 };
      if (child) {
        add(b, child, 0, s.h + g.head);
        b.h = s.h + g.head + child.h;
      }
      return b;
    }
    const a = ANCHOR(s.h);
    if (!child) return { w: s.w, h: s.h, at: [[k, { x: 0, y: 0 }]], routes: [], heads: [{ key: k, x: 0, y: a, top: 0 }], row: false, lead: a };
    const b: Block = { w: 0, h: 0, at: [], routes: [], heads: [], row: false, lead: 0 };
    if (horizontal) {
      // Beside its first child, their edge straight; a trunk in the gap reaches the rest.
      const cy = Math.max(0, child.lead - a);
      const by = Math.max(0, a - child.lead);
      const bx = s.w + g.depth;
      const tx = s.w + g.depth / 2;
      b.at.push([k, { x: 0, y: cy }]);
      add(b, child, bx, by);
      for (const h of child.heads) b.routes.push([`${k}>${h.key}`, simplify([{ x: s.w, y: cy + a }, { x: tx, y: cy + a }, { x: tx, y: by + h.y }, { x: bx + h.x, y: by + h.y }])]);
      Object.assign(b, { w: bx + child.w, h: Math.max(cy + s.h, by + child.h), heads: [{ key: k, x: 0, y: cy + a, top: cy }], lead: cy + a });
    } else if (child.row) {
      // A row of lanes under it: down to a bus in the gap above them, along, and into each top.
      const by = s.h + g.depth;
      const bus = s.h + g.depth / 2;
      b.at.push([k, { x: 0, y: 0 }]);
      add(b, child, 0, by);
      for (const h of child.heads) b.routes.push([`${k}>${h.key}`, simplify([{ x: RAIL, y: s.h }, { x: RAIL, y: bus }, { x: h.x + RAIL, y: bus }, { x: h.x + RAIL, y: by + h.top }])]);
      Object.assign(b, { w: Math.max(s.w, child.w), h: by + child.h, heads: [{ key: k, x: 0, y: a, top: 0 }], lead: a });
    } else {
      // A grid or list under it, indented: a trunk down the indent into each row's first card.
      const by = s.h + g.under;
      const tx = g.indent / 2;
      b.at.push([k, { x: 0, y: 0 }]);
      add(b, child, g.indent, by);
      for (const h of child.heads) b.routes.push([`${k}>${h.key}`, simplify([{ x: tx, y: s.h }, { x: tx, y: by + h.y }, { x: g.indent + h.x, y: by + h.y }])]);
      Object.assign(b, { w: Math.max(s.w, g.indent + child.w), h: by + child.h, heads: [{ key: k, x: 0, y: a, top: 0 }], lead: a });
    }
    return b;
  };

  // A lanes parent's lanes: stacked growing right (or listed down a phone), side by side growing down.
  const lanes = (k: string): Block | null => {
    const mine: [string, Block][] = [];
    for (const c of kids(k)) if (claim(c)) mine.push([c, lane(c)]);
    if (!mine.length) return null;
    const gap = mine.some(([c]) => shape.section?.(c)) ? g.section : g.lane;
    const side = !horizontal && wraps.lanes === 0;
    across.set(k, side);
    const b: Block = { w: 0, h: 0, at: [], routes: [], heads: [], row: side, lead: mine[0][1].lead };
    let along = 0;
    for (const [c, l] of mine) {
      const [x, y] = side ? [along, 0] : [0, along];
      add(b, l, x, y);
      b.heads.push({ key: c, x, y: y + l.lead, top: y + (l.heads[0]?.top ?? 0) });
      b.w = Math.max(b.w, x + l.w);
      b.h = Math.max(b.h, y + l.h);
      along += (side ? l.w : l.h) + gap;
    }
    return b;
  };

  // Machines: each with its work, `wraps.roots` to a column (growing right) or a row (down); the
  // ones with nothing open share one stack, last, so they never sit among another's projects.
  const out: Placement = { positions: new Map(), routes: new Map(), widths, across };
  const blocks: Block[] = [];
  const idle: Block = { w: 0, h: 0, at: [], routes: [], heads: [], row: false, lead: 0 };
  for (const r of tree.roots) {
    if (!claim(r)) continue;
    const b = lane(r);
    if (b.at.length > 1) blocks.push(b);
    else {
      const y = idle.h ? idle.h + g.root / 2 : 0;
      add(idle, b, 0, y);
      idle.w = Math.max(idle.w, b.w);
      idle.h = y + b.h;
    }
  }
  if (idle.at.length) blocks.push(idle);
  const per = wraps.roots > 0 ? wraps.roots : Infinity;
  across.set(ROOTS, per < blocks.length ? "grid" : !horizontal);
  const all = { at: [] as [string, Point][], routes: [] as [string, Point[]][] };
  let line = 0;
  let along = 0;
  let thick = 0;
  blocks.forEach((b, i) => {
    if (i && i % per === 0) {
      line += thick + g.root;
      along = 0;
      thick = 0;
    }
    if (horizontal) add(all, b, line, along);
    else add(all, b, along, line);
    along += (horizontal ? b.h : b.w) + g.root;
    thick = Math.max(thick, horizontal ? b.w : b.h);
  });
  for (const [k, p] of all.at) out.positions.set(k, p);
  for (const [k, pts] of all.routes) out.routes.set(k, pts);
  return out;
}

/** The screen a layout is planned and fitted for (px): the canvas above the prompt bar. */
export interface Room {
  w: number;
  h: number;
  /** Kept clear around what's fitted. */
  pad: number;
  /** Fit never zooms out further than this (the cards stop reading). */
  floor: number;
  /** Kept clear down the right while the canvas is bigger than the screen (the minimap's corner). */
  reserve?: number;
}

/** The zoom that shows all of `r` in `room`, at most 1. */
export const fitZoom = (r: Size, room: Room) => Math.min(1, (room.w - room.pad * 2) / Math.max(1, r.w), (room.h - room.pad * 2) / Math.max(1, r.h));

const LINES = [1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 16];

/**
 * The wraps that show this tree best at the zoom Fit will use in `room`: every number of a grid's
 * columns or rows (and of machines to a line, and growing down whether projects sit side by side) is laid
 * out. Everything fitting, the biggest zoom wins; nothing fits at the floor, the arrangement as
 * wide as the screen takes at that zoom (whole columns, clear of the minimap) and as short as that
 * allows wins, so what doesn't fit runs on below and never off the right. A wrap must earn 2 %.
 */
export function planWraps(tree: TreeLike, size: (key: string) => Size, shape: CanvasShape, room: Room): Wraps {
  let most = 1;
  for (const [k, n] of tree.nodes) if (shape.level(k) === "items") most = Math.max(most, n.children.length);
  // Simplest first (a plain column), so a wrap that doesn't earn its keep loses the tie.
  const lines = [...new Set([...LINES.filter((c, i) => c <= most || LINES[i - 1] < most), ...(shape.direction === "vertical" ? [0] : [])])].sort((a, b) => (shape.direction === "vertical" ? (b || Infinity) - (a || Infinity) : a - b));
  const lanes: (0 | 1)[] = shape.direction === "vertical" ? [0, 1] : [0];
  const machines = new Set(tree.roots).size;
  const roots = machines < 2 ? [0] : [0, 1, 2, 3].filter((r) => r < machines);
  let best = NO_WRAPS;
  let bestScore = -Infinity;
  for (const r of roots)
    for (const l of lanes)
      for (const c of lines) {
        const wraps: Wraps = { lines: c, lanes: l, roots: r };
        const box = bounds(layoutCanvas(tree, size, shape, wraps).positions, size);
        if (!box) return best;
        const score = scoreFor(box, room);
        if (score > bestScore * 1.02) (bestScore = score), (best = wraps);
      }
  return best;
}

function scoreFor(r: Size, room: Room): number {
  const k = fitZoom(r, room);
  if (k >= room.floor) return k;
  // Read at the floor, from the top: how much shows, and none of it cut off at the right.
  const w = room.w - room.pad * 2 - (room.reserve ?? 0);
  const h = room.h - room.pad * 2;
  const wide = r.w * room.floor;
  const shown = Math.min(1, w / wide) * Math.min(1, h / (r.h * room.floor));
  return room.floor * shown * (wide <= w ? 1 : 0.5);
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
 * Where a sibling dragged with its centre at `at` drops: after every other sibling that reads
 * before it. Side by side (`across`) that's by x; listed, by y, except along a row that siblings
 * share (idle machines growing right): with `at` level with it, by x. `rects` are where the
 * siblings (the dragged one too) were as the drag started. In a grid (`"grid"`), the nearest cell.
 */
export function dropIndex(siblings: readonly string[], key: string, at: Point, rects: ReadonlyMap<string, Rect>, across: boolean | "grid"): number {
  // Wrapped into a grid, the siblings fill its cells in order: it drops into the cell nearest it.
  if (across === "grid") {
    let best = 0;
    let near = Infinity;
    siblings.forEach((s, i) => {
      const r = rects.get(s);
      const d = r ? Math.hypot(r.x + r.w / 2 - at.x, r.y + r.h / 2 - at.y) : Infinity;
      if (d < near) (near = d), (best = i);
    });
    return best;
  }
  const level = (a: Rect, b: Rect) => a.y < b.y + b.h && b.y < a.y + a.h;
  let i = 0;
  for (const s of siblings) {
    const r = rects.get(s);
    if (s === key || !r) continue;
    const row = !across && siblings.some((o) => o !== s && rects.has(o) && level(rects.get(o)!, r));
    const byX = across || (row && at.y >= r.y && at.y <= r.y + r.h);
    if (byX ? r.x + r.w / 2 < at.x : r.y + r.h / 2 < at.y) i++;
  }
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

/** Without repeated points or points in the middle of a straight run. */
function simplify(pts: Point[]): Point[] {
  const out: Point[] = [];
  for (const p of pts) {
    const q = out[out.length - 1];
    if (q && Math.abs(q.x - p.x) < 0.5 && Math.abs(q.y - p.y) < 0.5) continue;
    const o = out[out.length - 2];
    if (o && q && ((Math.abs(o.x - q.x) < 0.5 && Math.abs(q.x - p.x) < 0.5) || (Math.abs(o.y - q.y) < 0.5 && Math.abs(q.y - p.y) < 0.5))) out[out.length - 1] = p;
    else out.push(p);
  }
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
