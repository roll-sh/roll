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
  /** Parents whose leaf children are laid out as a grid block (`planGrids`). */
  grids?: ReadonlyMap<string, Grid>;
  /** Growing right: nodes that don't widen their column (a grid block). */
  overflows?: (key: string) => boolean;
  /** Children that stay out of their parent's grid and follow it (a fold card). */
  tail?: (key: string) => boolean;
  /** Where an edge meets a card of height h, from its top (the header's middle). */
  anchor?: (h: number) => number;
  /** From the top of a node's band to the line its edge meets, when that isn't its card's anchor (a grid block's first card). */
  lead?: (key: string) => number | undefined;
  /** Nodes laid out as sections: a header over its cards, as one block (`withSections`). */
  sections?: (key: string) => boolean;
  /** Roots (machines) to a column growing right, to a row growing down (`wrapRoots`); absent, one stack (row). */
  rootsPer?: number;
}

export type Gaps = Required<Omit<LayoutOptions, "direction" | "spreads" | "grids" | "overflows" | "tail" | "anchor" | "lead" | "rootsPer" | "sections">>;

/** Where an edge meets a full card: its header's middle. */
export const ANCHOR = (h: number) => Math.min(h / 2, 28);

/** The gaps a direction lays out with by default (the canvas's comfortable density). */
export const GAPS: Readonly<Record<Direction, Gaps>> = {
  horizontal: { depthGap: 72, siblingGap: 20, rootGap: 64, indent: 0, listGap: 0 },
  vertical: { depthGap: 56, siblingGap: 32, rootGap: 72, indent: 40, listGap: 14 },
};

/** The compact density's gaps: about half, so more of the canvas fits on a screen. */
export const COMPACT_GAPS: Readonly<Record<Direction, Gaps>> = {
  horizontal: { depthGap: 44, siblingGap: 10, rootGap: 36, indent: 0, listGap: 0 },
  vertical: { depthGap: 32, siblingGap: 16, rootGap: 40, indent: 28, listGap: 8 },
};

/** The overview's gaps (canvas-face.tsx): faces are drawn for about 40 %, so their gaps are too. */
export const OVERVIEW_GAPS: Readonly<Record<Direction, Gaps>> = {
  horizontal: { depthGap: 128, siblingGap: 28, rootGap: 160, indent: 0, listGap: 0 },
  vertical: { depthGap: 96, siblingGap: 64, rootGap: 160, indent: 88, listGap: 28 },
};

/**
 * Top-left corners for every node reachable from the roots, so that no two cards ever overlap.
 *
 * Growing right, it's a tidy tree: each subtree takes the height of its children stacked (or its
 * own, if taller) and its root sits centred on them (beside the first ones of a long column); depths
 * are columns as wide as their widest card, so cards line up.
 *
 * Growing down, it's a board: what spreads (the roots, a machine's projects) sits side by side in
 * rows as tall as their tallest card, its parent centred over them; everything else lists its
 * children down its column, indented, each child's own subtree before the next (a project's
 * threads, each with its requests and subagents under it), so a narrow screen reads it top to
 * bottom like Linear's board, a column per project.
 */
export function tidyLayout(tree: TreeLike, size: (key: string) => Size, opts: LayoutOptions = {}): Map<string, Point> {
  if (opts.rootsPer && new Set(tree.roots).size > opts.rootsPer) return wrapRoots(tree, size, opts);
  if (opts.sections) {
    const keys = [...tree.nodes.keys()].filter((k) => opts.sections!(k) && tree.nodes.get(k)!.children.length > 0);
    if (keys.length) return withSections(tree, size, opts, keys);
  }
  const grids = opts.grids;
  if (!grids?.size) return opts.direction === "vertical" ? boardLayout(tree, size, opts) : treeLayout(tree, size, opts);
  // Each grid is one block in its parent's place for the first of its cards; the cards go in it after.
  const block = new Map<string, Grid>();
  const nodes = new Map<string, { children: readonly string[] }>();
  for (const [k, n] of tree.nodes) {
    const g = grids.get(k);
    if (!g) {
      nodes.set(k, n);
      continue;
    }
    const key = `${GRID}${k}`;
    block.set(key, g);
    const members = new Set(g.members);
    const children: string[] = [];
    for (const c of n.children) {
      if (!members.has(c)) children.push(c);
      else if (c === g.members[0]) children.push(key);
    }
    nodes.set(k, { children });
  }
  for (const key of block.keys()) nodes.set(key, { children: [] });
  const sized = (k: string) => block.get(k)?.size ?? size(k);
  const anchor = opts.anchor ?? ANCHOR;
  // A block's band is its own (siblings never share one), so growing right it may reach past its
  // column: the columns after it stay where the cards' widths put them. Its parent lines up with
  // its first card, and growing down a parent with a grid lists it (its trunk runs down the left of
  // the block) rather than spreading it.
  const inner: LayoutOptions = {
    ...opts,
    grids: undefined,
    overflows: (k) => block.has(k),
    lead: (k) => {
      const g = block.get(k);
      return g ? g.cells[0].y + anchor(size(g.members[0]).h) : opts.lead?.(k);
    },
    spreads: (k) => !grids.has(k) && (opts.spreads?.(k) ?? true),
  };
  const out = opts.direction === "vertical" ? boardLayout({ roots: tree.roots, nodes }, sized, inner) : treeLayout({ roots: tree.roots, nodes }, sized, inner);
  for (const [key, g] of block) {
    const at = out.get(key);
    out.delete(key);
    if (!at) continue;
    for (const [k, p] of g.placed) out.set(k, { x: at.x + p.x, y: at.y + p.y });
  }
  return out;
}

const GRID = "#grid:";

/**
 * Sections (a drilled project's status groups): the header with its cards under it, flush left,
 * as one block, whichever way the tree grows; the block takes the section's place in the tree.
 * A section reads as a header row over its cards, not as a parent wired to each of them.
 */
function withSections(tree: TreeLike, size: (key: string) => Size, opts: LayoutOptions, keys: readonly string[]): Map<string, Point> {
  const anchor = opts.anchor ?? ANCHOR;
  const block = new Map<string, { placed: Map<string, Point>; size: Size }>();
  for (const s of keys) {
    const p = tidyLayout({ roots: [s], nodes: tree.nodes }, size, { ...opts, direction: "vertical", spreads: () => false, indent: 0, listGap: Math.max(12, opts.listGap ?? 0), sections: undefined, rootsPer: undefined });
    const r = bounds(p, size);
    if (!r) continue;
    const placed = new Map<string, Point>();
    for (const [k, q] of p) placed.set(k, { x: q.x - r.x, y: q.y - r.y });
    block.set(s, { placed, size: { w: r.w, h: r.h } });
  }
  const nodes = new Map(tree.nodes);
  for (const s of block.keys()) nodes.set(s, { children: [] });
  const out = tidyLayout({ roots: tree.roots, nodes }, (k) => block.get(k)?.size ?? size(k), {
    ...opts,
    sections: undefined,
    lead: (k) => (block.has(k) ? anchor(size(k).h) : opts.lead?.(k)),
  });
  for (const [s, b] of block) {
    const at = out.get(s);
    if (!at) continue;
    for (const [k, p] of b.placed) out.set(k, { x: at.x + p.x, y: at.y + p.y });
  }
  return out;
}
/** How far a thread's requests, subagents and forks sit in from it inside a grid's cell (world px). */
const CELL_INDENT = 64;

/**
 * Many machines on a wide screen: each machine's tree laid out on its own, `rootsPer` to a column
 * (growing right) or a row (growing down), the columns (rows) side by side, so machines use the
 * screen's width instead of one tall stack.
 */
function wrapRoots(tree: TreeLike, size: (key: string) => Size, opts: LayoutOptions): Map<string, Point> {
  const per = opts.rootsPer!;
  const horizontal = opts.direction !== "vertical";
  const gap = opts.rootGap ?? GAPS[horizontal ? "horizontal" : "vertical"].rootGap;
  const out = new Map<string, Point>();
  let x = 0;
  let y = 0;
  let line = 0;
  let count = 0;
  for (const r of new Set(tree.roots)) {
    if (out.has(r)) continue;
    const p = tidyLayout({ roots: [r], nodes: tree.nodes }, size, { ...opts, rootsPer: undefined });
    const b = bounds(p, size);
    if (!b) continue;
    if (count === per) {
      if (horizontal) (x += line + gap), (y = 0);
      else (y += line + gap), (x = 0);
      line = 0;
      count = 0;
    }
    for (const [k, q] of p) if (!out.has(k)) out.set(k, { x: x + q.x - b.x, y: y + q.y - b.y });
    if (horizontal) (y += b.h + gap), (line = Math.max(line, b.w));
    else (x += b.w + gap), (line = Math.max(line, b.h));
    count++;
  }
  return out;
}

/**
 * A parent's cards wrapped into a grid (a busy project's threads, a machine's many projects), placed
 * as one block: in columns of `per` growing right, or rows of `per` growing down, whichever way the
 * tree grows. A card with something under it (requests, subagents, a fork; a project's own grid)
 * takes its whole subtree into its cell, laid out as the tree would.
 */
export interface Grid {
  members: readonly string[];
  /** Top-left of each member inside the block. */
  cells: readonly Point[];
  /** Every card in the block (members and what's under them), from the block's top-left. */
  placed: ReadonlyMap<string, Point>;
  size: Size;
  /** Members in the block's first column (the ones its parent's trunk reaches directly). */
  first: ReadonlySet<string>;
  /** How many go down a column before the next (growing right), or across a row (growing down). */
  per: number;
  /** Each member's column, and each column's width. */
  column: ReadonlyMap<string, number>;
  colW: readonly number[];
}

/**
 * The grids for the parents `per` names a count for (cards per column growing right, per row growing
 * down): a parent with more members than that wraps. Members `tail` picks (a fold card) stay out of
 * the block and follow it in the parent's flow. Inner grids are planned first, so a machine's grid of
 * projects holds each project's own grid of threads.
 */
export function planGrids(
  tree: TreeLike,
  size: (key: string) => Size,
  opts: {
    direction: Direction;
    per: (key: string) => number | undefined;
    tail?: (key: string) => boolean;
    /** Parents whose members keep what's under them compact: listed under them, indented, inside their cell. */
    compact?: (key: string) => boolean;
    gap?: Point;
    /** Room (world px) over a block of more than one column, for the bus its later columns hang off. */
    bus?: number;
    layout?: LayoutOptions;
  },
): Map<string, Grid> {
  const out = new Map<string, Grid>();
  const horizontal = opts.direction === "horizontal";
  const gx = opts.gap?.x ?? 40;
  const gy = opts.gap?.y ?? (horizontal ? 20 : 14);
  type Cell = { placed: Map<string, Point>; size: Size };
  // A member's subtree laid out on its own (with the grids inside it), from its top-left. A thread's
  // requests, subagents and forks go under it, indented, so its cell stays a card wide: beside it, a
  // fork made a whole column wide and the next sat far off.
  const cell = (key: string, compact: boolean): Cell => {
    if (!tree.nodes.get(key)?.children.length) return { placed: new Map([[key, { x: 0, y: 0 }]]), size: size(key) };
    const layout: LayoutOptions = compact
      ? { ...opts.layout, direction: "vertical", spreads: () => false, indent: CELL_INDENT, listGap: Math.max(8, gy / 2), grids: out, tail: opts.tail }
      : { ...opts.layout, direction: opts.direction, grids: out, tail: opts.tail };
    const p = tidyLayout({ roots: [key], nodes: tree.nodes }, size, layout);
    const r = bounds(p, size)!;
    const placed = new Map<string, Point>();
    for (const [k, q] of p) placed.set(k, { x: q.x - r.x, y: q.y - r.y });
    return { placed, size: { w: r.w, h: r.h } };
  };
  // Deepest first: a parent's cells are laid out with its descendants' grids in them.
  const order: string[] = [];
  const seen = new Set<string>();
  const walk = (k: string) => {
    if (seen.has(k)) return;
    seen.add(k);
    for (const c of tree.nodes.get(k)?.children ?? []) walk(c);
    order.push(k);
  };
  for (const r of tree.roots) walk(r);
  // Items grids (a lane's threads) share one column width, so a drilled project's Done lines up
  // with its Quiet and every pill in them is as wide as the widest.
  const plans = new Map<string, { per: number; members: string[]; cells: Cell[] }>();
  for (const k of order) {
    const per = opts.per(k);
    const members = (tree.nodes.get(k)?.children ?? []).filter((c) => !opts.tail?.(c));
    if (!per || members.length <= per || !opts.compact?.(k)) continue;
    plans.set(k, { per, members, cells: members.map((m) => cell(m, true)) });
  }
  let uniform = 0;
  for (const p of plans.values()) for (const c of p.cells) uniform = Math.max(uniform, c.size.w);
  for (const k of order) {
    const per = opts.per(k);
    const members = (tree.nodes.get(k)?.children ?? []).filter((c) => !opts.tail?.(c));
    if (!per || members.length <= per) continue;
    const cells = plans.get(k)?.cells ?? members.map((m) => cell(m, false));
    const cols = horizontal ? Math.ceil(members.length / per) : per;
    // Growing right, columns fill first (newest down the first column); growing down, rows do.
    const col = (i: number) => (horizontal ? Math.floor(i / per) : i % per);
    const colW = new Array<number>(cols).fill(plans.has(k) ? uniform : 0);
    cells.forEach((s, i) => (colW[col(i)] = Math.max(colW[col(i)], s.size.w)));
    const colX = colW.map((_, c) => colW.slice(0, c).reduce((s, x) => s + x + gx, 0));
    // Each column stacks its own cells (a card with something under it makes only its column
    // longer, never an empty row beside it), under room for the bus the later columns hang off.
    const room = cols > 1 ? (opts.bus ?? 0) : 0;
    const colY = new Array<number>(cols).fill(room);
    const placed = new Map<string, Point>();
    cells.forEach((s, i) => {
      const c = col(i);
      for (const [key, q] of s.placed) placed.set(key, { x: colX[c] + q.x, y: colY[c] + q.y });
      colY[c] += s.size.h + gy;
    });
    out.set(k, {
      members,
      cells: members.map((m) => placed.get(m)!),
      placed,
      size: { w: colX[cols - 1] + colW[cols - 1], h: Math.max(...colY) - gy },
      first: new Set(members.filter((_, i) => col(i) === 0)),
      per,
      column: new Map(members.map((m, i) => [m, col(i)])),
      colW,
    });
  }
  return out;
}

/** What a wrapping parent holds: lanes (a machine's projects, a project's status groups) or items (threads). */
export type WrapLevel = "lanes" | "items";

/**
 * How many go per column (growing right) or per row (growing down), tried for each level: 0 never
 * wraps. Lanes keep their own column growing right and their own row growing down (a project
 * beside another's threads misread the tree), except that a narrow screen may stack them (1).
 * Items fill columns growing right, rows growing down.
 */
const PER: Readonly<Record<Direction, Record<WrapLevel, readonly number[]>>> = {
  horizontal: { lanes: [0], items: [0, 3, 4, 6, 8, 12] },
  vertical: { lanes: [0, 1], items: [0, 2, 3, 4, 6] },
};

export interface Wraps {
  lanes: number;
  items: number;
  /** Machines to a column (row); 0: one stack. */
  roots?: number;
}

/**
 * The best way to wrap this tree for a screen `aspect` (width ÷ height) wide: every combination of
 * machines per column, lanes per stack and items per column is laid out, and the one that shows
 * everything biggest wins (a wrap must earn its keep: ties go to fewer). One rule for every shape
 * of data: one machine or five, a machine with one project or ten, a project with 2 threads or
 * 200, a wide screen or a narrow one beside a panel. Big trees try fewer combinations.
 */
export function planWraps(
  tree: TreeLike,
  size: (key: string) => Size,
  opts: { direction: Direction; aspect: number; level: (key: string) => WrapLevel | null; tail?: (key: string) => boolean; gap?: Point; bus?: number; layout?: LayoutOptions },
): Wraps {
  const per = PER[opts.direction];
  const big = tree.nodes.size > 400;
  const lanes = per.lanes;
  const items = big ? per.items.filter((p) => p === 0 || p === 6 || p === 12 || (opts.direction === "vertical" && p === 4)) : per.items;
  const many = new Set(tree.roots).size;
  const roots = many < 2 ? [0] : big ? [0, 2] : [0, 1, 2, 3].filter((r) => r < many);
  let best: Wraps = { lanes: 0, items: 0, roots: 0 };
  let bestScore = -Infinity;
  for (const o of roots)
    for (const l of lanes)
      for (const i of items) {
        const wraps = { lanes: l, items: i, roots: o };
        const grids = gridsFor(tree, size, { ...opts, wraps });
        const r = bounds(tidyLayout(tree, size, { ...opts.layout, direction: opts.direction, grids, tail: opts.tail, rootsPer: o || undefined }), size);
        if (!r) return best;
        // The zoom that fits it on a screen 1 high and `aspect` wide; each kind of wrap costs 3 %.
        const fit = Math.min(opts.aspect / Math.max(1, r.w), 1 / Math.max(1, r.h));
        const score = fit * (1 - 0.03 * ((l ? 1 : 0) + (i ? 1 : 0) + (o ? 1 : 0)));
        if (score > bestScore * 1.0001) (bestScore = score), (best = wraps);
      }
  return best;
}

/** The grids `wraps` makes. */
export function gridsFor(
  tree: TreeLike,
  size: (key: string) => Size,
  opts: { direction: Direction; wraps: Wraps; level: (key: string) => WrapLevel | null; tail?: (key: string) => boolean; gap?: Point; bus?: number; layout?: LayoutOptions },
): Map<string, Grid> {
  const per = (k: string) => {
    const l = opts.level(k);
    return (l === "lanes" ? opts.wraps.lanes : l === "items" ? opts.wraps.items : 0) || undefined;
  };
  return planGrids(tree, size, { direction: opts.direction, per, tail: opts.tail, compact: (k) => opts.level(k) === "items", gap: opts.gap, bus: opts.bus, layout: opts.layout });
}

/**
 * Growing right, a parent lines up with its first child (their edge is straight) instead of sitting
 * centred on all of them: centred on a long column it sat far from the projects it led to, its edges
 * converging across half the screen.
 */
function treeLayout(tree: TreeLike, size: (key: string) => Size, opts: LayoutOptions): Map<string, Point> {
  const depthGap = opts.depthGap ?? GAPS.horizontal.depthGap;
  const siblingGap = opts.siblingGap ?? GAPS.horizontal.siblingGap;
  const rootGap = opts.rootGap ?? GAPS.horizontal.rootGap;
  const anchor = opts.anchor ?? ANCHOR;
  const span = new Map<string, number>();
  const childBlock = new Map<string, number>();
  // Where the card and its children start in its band, and the line its edge meets (from the band's top).
  const offset = new Map<string, { card: number; kids: number; lead: number }>();
  const depth = new Map<string, number>();
  const colW: number[] = [];

  const measure = (key: string, d: number): number => {
    const known = span.get(key);
    if (known !== undefined) return known;
    if (depth.has(key)) return 0;
    depth.set(key, d);
    const { w, h } = size(key);
    colW[d] = Math.max(colW[d] ?? 0, opts.overflows?.(key) ? 0 : w);
    const kids = (tree.nodes.get(key)?.children ?? []).filter((c) => !depth.has(c));
    let block = 0;
    kids.forEach((c, i) => (block += measure(c, d + 1) + (i ? siblingGap : 0)));
    const own = opts.lead?.(key) ?? anchor(h);
    const first = kids.length ? offset.get(kids[0])!.lead : own;
    const card = kids.length ? Math.max(0, first - own) : 0;
    const under = kids.length ? Math.max(0, own - first) : 0;
    offset.set(key, { card, kids: under, lead: card + own });
    const s = Math.max(card + h, under + block);
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
    const d = depth.get(key)!;
    const o = offset.get(key)!;
    out.set(key, { x: colX[d], y: top + o.card });
    let y = top + o.kids;
    for (const c of tree.nodes.get(key)?.children ?? []) {
      if (depth.get(c) !== d + 1 || out.has(c)) continue;
      place(c, y);
      y += span.get(c)! + siblingGap;
    }
  };

  // Roots with nothing under them (a machine with nothing open) stack in the machines' column, so
  // they never sit among another machine's projects.
  const roots = [...new Set(tree.roots)].filter((r) => depth.get(r) === 0);
  let top = 0;
  for (const r of roots) {
    if (!childBlock.get(r)) {
      out.set(r, { x: 0, y: top });
      top += size(r).h + rootGap / 2;
      continue;
    }
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
      // At the start of its row (its first child under it), as growing right: centred over a wide
      // row it sat far from the first column, off a phone's screen.
      out.set(key, { x, y });
      let cx = x;
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

/**
 * Every edge as an orthogonal line (world px, from the parent's side to the child's), the way the
 * layout placed them: growing right, out of the parent's right side to a trunk in the gap before its
 * children, along it, and into each child's left side; growing down, out of the parent's bottom near
 * its left (a list's trunk runs down the indent) or, for a row of lanes, along a bus above them and
 * down into each. A grid's later columns hang off a bus over the block, through the gaps between
 * columns, so no line crosses a card. Siblings share their trunk and bus: drawn as one path.
 */
export function routeEdges(
  pos: ReadonlyMap<string, Point>,
  size: (key: string) => Size,
  edges: readonly { from: string; to: string }[],
  opts: { direction: Direction; grids: ReadonlyMap<string, Grid>; spreads: (key: string) => boolean; anchor: (h: number) => number; gaps: Gaps; gridGap: Point; bus?: number; sections?: (key: string) => boolean },
): Map<string, Point[]> {
  const out = new Map<string, Point[]>();
  // A section's header doesn't wire itself to each of its cards: they sit under it.
  edges = edges.filter((e) => !opts.sections?.(e.from));
  const rect = (k: string): Rect | null => {
    const p = pos.get(k);
    return p ? { ...p, ...size(k) } : null;
  };
  // Per parent: its first column's left (the trunk goes just before it) and its grid's top.
  const firstX = new Map<string, number>();
  const gridTop = new Map<string, number>();
  for (const e of edges) {
    const b = rect(e.to);
    if (!b) continue;
    const g = opts.grids.get(e.from);
    if (!g?.column.get(e.to)) firstX.set(e.from, Math.min(firstX.get(e.from) ?? Infinity, b.x));
    if (g?.column.has(e.to)) gridTop.set(e.from, Math.min(gridTop.get(e.from) ?? Infinity, b.y));
  }
  const { depthGap, indent, listGap } = opts.gaps;
  for (const e of edges) {
    const a = rect(e.from);
    const b = rect(e.to);
    if (!a || !b) continue;
    const col = opts.grids.get(e.from)?.column.get(e.to) ?? 0;
    const by = b.y + opts.anchor(b.h);
    let pts: Point[];
    // Beside its parent (the tree growing right), or under it (top-down, or kept compact in a grid's cell).
    const under = b.y >= a.y + a.h - 0.5 && b.x < a.x + a.w;
    if (!under) {
      const ay = a.y + opts.anchor(a.h);
      const x0 = Math.max(a.x + a.w + 6, (firstX.get(e.from) ?? b.x) - depthGap / 2);
      if (!col) pts = [{ x: a.x + a.w, y: ay }, { x: x0, y: ay }, { x: x0, y: by }, { x: b.x, y: by }];
      else {
        const bus = (gridTop.get(e.from) ?? b.y) - (opts.bus ?? opts.gridGap.y) / 2;
        const ux = b.x - opts.gridGap.x / 2;
        pts = [{ x: a.x + a.w, y: ay }, { x: x0, y: ay }, { x: x0, y: bus }, { x: ux, y: bus }, { x: ux, y: by }, { x: b.x, y: by }];
      }
    } else if (opts.direction === "vertical" && opts.spreads(e.from)) {
      const sx = a.x + indent / 2;
      const bus = b.y - depthGap / 2;
      const cx = b.x + indent / 2;
      pts = [{ x: sx, y: a.y + a.h }, { x: sx, y: bus }, { x: cx, y: bus }, { x: cx, y: b.y }];
    } else {
      // Down the indent, halfway between the parent's side and the child's.
      const tx = a.x + Math.max(6, (b.x - a.x) / 2);
      if (!col) pts = [{ x: tx, y: a.y + a.h }, { x: tx, y: by }, { x: b.x, y: by }];
      else {
        const bus = (gridTop.get(e.from) ?? b.y) - (opts.bus ?? Math.max(listGap, opts.gridGap.y)) / 2;
        const ux = b.x - opts.gridGap.x / 2;
        pts = [{ x: tx, y: a.y + a.h }, { x: tx, y: bus }, { x: ux, y: bus }, { x: ux, y: by }, { x: b.x, y: by }];
      }
    }
    out.set(`${e.from}>${e.to}`, simplify(pts));
  }
  return out;
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

/**
 * The first fit of a canvas: readable cards (`readable`) beat seeing everything, but when a little
 * less (down to `floor`, with half the padding) shows everything, that wins: nothing is left
 * behind the prompt bar at the bottom for want of a few pixels. Too much for that: readable,
 * framed from the top (`fitView`'s rule).
 */
export function fitReadable(r: Rect, vw: number, vh: number, pad: number, readable: number, floor: number): View {
  const v = fitView(r, vw, vh, pad, 1, readable);
  const fits = (view: View, p: number) => r.w * view.k <= vw - p * 2 + 0.5 && r.h * view.k <= vh - p * 2 + 0.5;
  if (fits(v, pad)) return v;
  const tight = fitView(r, vw, vh, pad / 2, 1, floor);
  return fits(tight, pad / 2) ? tight : v;
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
