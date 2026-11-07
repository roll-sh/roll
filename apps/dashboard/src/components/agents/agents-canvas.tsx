"use client";

import { ArrowDownFromLine, ArrowRightFromLine, CircleDot, ListRestart, Maximize, Minus, Plus } from "lucide-react";
import {
  forwardRef,
  memo,
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useReducer,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { useReducedMotion } from "@roll/brand";
import type { Graph, GraphNode } from "@/lib/agents-graph";
import {
  arrange,
  bounds,
  COMPACT_SPACING,
  dropIndex,
  fitView,
  layoutCanvas,
  loadOrder,
  nextCard,
  loadView,
  moveTo,
  NO_WRAPS,
  planWraps,
  ROOTS,
  saveOrder,
  saveView,
  SPACING,
  toWorld,
  withOrder,
  type Arrow,
  type CanvasShape,
  type Direction,
  type Level,
  type Order,
  type Placement,
  type Point,
  type Rect,
  type Room,
  type Size,
  type View,
} from "@/lib/agents-layout";
import { CanvasEngine, type SceneEdge, type SceneMeta } from "./canvas-engine";
import { AnimatePresence, motion, useMotionValue, type Transition } from "motion/react";
import { CARD_OUT, POPOVER_IN } from "@/lib/agents-motion";
import { usePresenceTracks } from "./presence";
import { Minimap, type MinimapHandle } from "./minimap";

export interface CanvasHandle {
  /** Pan (and zoom) so a node is in view; `zoom` sets the scale. */
  focus: (key: string, zoom?: number) => void;
  /** Pan to a node only if it isn't already in view (it keeps the zoom). */
  reveal: (key: string) => void;
  fit: () => void;
}

/**
 * The canvas: the engine's canvas underneath, the cards in a world layer on top, placed by the
 * tidy layout, growing to the right or down (`direction`), in the order the user dragged cards
 * into (a card moves among its siblings, so nothing ever overlaps). The order is the caller's
 * (`order`, `onOrder`: a canvas view's, synced through roll), else kept here under `storageKey`;
 * the view (pan and zoom) is remembered per device under `storageKey`, and a new `storageKey`
 * (another canvas view) goes back to how that one was framed, or fits it. Keyboard: + / − zoom, 0
 * is 100 %, 1 fits, arrows pan, ⌥↑ / ⌥↓ move the focused card among its siblings (⌥← / ⌥→ where
 * they sit side by side), ⇧D switches the direction, Escape calls `onEscape`.
 */
export const AgentsCanvas = memo(forwardRef<
  CanvasHandle,
  {
    graph: Graph;
    /** Where the view (and an uncontrolled order) is remembered on this device. */
    storageKey: string;
    /** The order to show; without it the canvas keeps its own. */
    order?: Order;
    onOrder?: (order: Order) => void;
    /** When it changes (the view's filter did), fit what's shown again. */
    fitKey?: string;
    /** Which way the tree grows (`horizontal` when absent). */
    direction?: Direction;
    /** With it, the toolbar switches the direction. */
    onDirection?: (direction: Direction) => void;
    /** `compact`: cards fold their details (`[data-detail]`) and sit closer. */
    density?: "compact";
    /** Long lists and many projects wrap to the screen's shape (`planWraps`) instead of one long column. */
    wrap?: boolean;
    /** Enter on a focused card: open it, saying whether it did (else Enter goes into the card). */
    onOpen?: (key: string) => boolean;
    render: (node: GraphNode) => ReactNode;
    defaultSize: (node: GraphNode) => Size;
    onEscape?: () => void;
    overlay?: ReactNode;
    /** Off while something covers the canvas: its keys go to that instead. */
    active?: boolean;
    /** The card whose conversation or settings are open beside it. */
    selected?: string;
    /** More cards marked the same way (a run's threads while the automations sheet shows it). */
    highlighted?: readonly string[];
    /** Threads Fit frames when everything is too small to read (running, waiting on you, failed). */
    matters?: (key: string) => boolean;
    /**
     * A card's launch handle (`data-launch="p:…"`, `m:…`, `t:…`) dragged out and dropped on empty
     * canvas, with where (client coordinates). A plain click on the handle stays a click.
     */
    onLaunch?: (source: string, at: Point) => void;
    /**
     * Something over the canvas's bottom (the prompt bar): fitting, focusing and revealing keep
     * cards above its top edge, wherever it is at that moment.
     */
    keepClear?: React.RefObject<HTMLElement | null>;
    /** Keys shown inside another card (a fork or subagent on its thread's card, `absorbChildren`): that card is what's revealed, focused and marked. */
    host?: ReadonlyMap<string, string>;
  }
>(function AgentsCanvas({ graph, storageKey, order: shownOrder, onOrder, fitKey, direction = "horizontal", onDirection, density, wrap = true, onOpen, render, defaultSize, onEscape, overlay, active = true, selected: selectedKey, highlighted, matters, onLaunch, keepClear, host }, ref) {
  // A card shown inside another stands for that one.
  const selected = selectedKey && (host?.get(selectedKey) ?? selectedKey);
  const reduced = useReducedMotion();
  const container = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const world = useRef<HTMLDivElement>(null);
  const engine = useRef<CanvasEngine | null>(null);
  const [sizes, setSizes] = useState<Map<string, Size>>(() => new Map());
  const [ownOrder, setOwnOrder] = useState<Order>({});
  const order = shownOrder ?? ownOrder;
  // While a card is dragged: its siblings in the order it would drop into.
  const [preview, setPreview] = useState<{ parent: string; list: string[] } | null>(null);
  // The zoom bar's "60%", written as the view moves: as state, every step of a zoom rendered the
  // whole canvas (200 cards) again.
  const zoomText = useRef<HTMLSpanElement>(null);
  const minimap = useRef<MinimapHandle | null>(null);
  const fitted = useRef(false);
  const viewChanged = useRef<((v: View) => void) | null>(null);
  const controlled = shownOrder !== undefined;
  useEffect(() => {
    if (!controlled) setOwnOrder(loadOrder(storageKey));
  }, [storageKey, controlled]);

  const setOrder = useCallback(
    (next: Order) => {
      if (onOrder) return onOrder(next);
      setOwnOrder(next);
      saveOrder(storageKey, next);
    },
    [storageKey, onOrder],
  );

  useLayoutEffect(() => {
    if (!container.current || !canvas.current || !world.current) return;
    let zoomTextAt = 0;
    let zoomTextLater: ReturnType<typeof setTimeout> | undefined;
    const e = new CanvasEngine(container.current, canvas.current, world.current, {
      reducedMotion: reduced,
      onView: (v: View) => {
        const c = container.current;
        // Too small to read, a card's text is a bar its own length and its details go (`[data-greek]`):
        // the card keeps its size, so nothing moves.
        if (c && v.k < GREEK !== c.hasAttribute("data-greek")) c.toggleAttribute("data-greek", v.k < GREEK);
        // A step before: details (meta lines, chips) go while titles still read (`[data-brief]`).
        if (c && v.k < BRIEF !== c.hasAttribute("data-brief")) c.toggleAttribute("data-brief", v.k < BRIEF);
        // Further still, the identity edges thicken again (they stay 2 screen px).
        if (c && v.k < FAR !== c.hasAttribute("data-far")) c.toggleAttribute("data-far", v.k < FAR);
        minimap.current?.view(v);
        // The zoom's readout: new text repaints the page, so while it changes, at most every
        // ZOOM_TEXT_MS, and where it rests.
        const text = `${Math.round(v.k * 100)}%`;
        const show = () => {
          if (zoomText.current && zoomText.current.textContent !== text) zoomText.current.textContent = text;
          zoomTextAt = performance.now();
        };
        clearTimeout(zoomTextLater);
        if (performance.now() - zoomTextAt >= ZOOM_TEXT_MS) show();
        else zoomTextLater = setTimeout(show, ZOOM_TEXT_MS);
        viewChanged.current?.(v);
      },
      onSizes: (changed) =>
        setSizes((prev) => {
          const next = new Map(prev);
          for (const [k, s] of changed) next.set(k, s);
          return next;
        }),
    });
    engine.current = e;
    if (zoomText.current) zoomText.current.textContent = `${Math.round(e.view.k * 100)}%`;
    // Cards mounted before the engine existed register now.
    world.current.querySelectorAll<HTMLElement>("[data-card]").forEach((el) => e.register(el.dataset.key!, el));
    // Follow light/dark switches.
    const mq = window.matchMedia("(prefers-color-scheme: light)");
    const onTheme = () => e.refreshColors();
    mq.addEventListener("change", onTheme);
    return () => {
      mq.removeEventListener("change", onTheme);
      clearTimeout(zoomTextLater);
      e.destroy();
      engine.current = null;
    };
    // The engine lives as long as the canvas; options follow below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => engine.current?.setOptions({ reducedMotion: reduced }), [reduced]);

  const sizeOf = useCallback(
    (key: string): Size => sizes.get(key) ?? defaultSize(graph.nodes.get(key)!),
    [sizes, defaultSize, graph],
  );

  const arranged = useMemo(() => arrange(graph, preview ? withOrder(order, preview.parent, preview.list) : order), [graph, order, preview]);
  // What each node's children are to the layout: a machine's projects and a drilled project's
  // status sections are lanes, a project's or a section's threads the items of a grid.
  const level = useCallback(
    (k: string): Level | null => {
      const n = graph.nodes.get(k);
      if (n?.kind === "machine") return "lanes";
      if (n?.kind === "project") return n.children.some((c) => graph.nodes.get(c)?.kind === "group") ? "lanes" : "items";
      return n?.kind === "group" ? "items" : null;
    },
    [graph],
  );
  // A drilled project's status groups are sections: a header row over their cards.
  const isSection = useCallback((k: string) => graph.nodes.get(k)?.kind === "group", [graph]);
  // The screen the layout is planned for (`planWraps`) and fitted to (`roomFor`), coarse.
  const [screen, setScreen] = useState<Size>({ w: 1440, h: 760 });
  /** The layout: the cards' measured sizes, wrapped the way that shows them best on this screen (`planWraps`). */
  const shown = useMemo((): Placement & { size: (k: string) => Size } => {
    const size = (k: string): Size => {
      const n = graph.nodes.get(k);
      if (!n) return ZERO;
      // A section's header is a fixed row (drawn as wide as its cards, `--section-w`): measured, it would grow with them.
      return n.kind === "group" ? SECTION_HEAD : sizeOf(k);
    };
    const shape: CanvasShape = { direction, level, section: isSection, spacing: (density === "compact" ? COMPACT_SPACING : SPACING)[direction] };
    const wraps = wrap ? planWraps(arranged, size, shape, roomFor(screen.w, screen.h)) : NO_WRAPS;
    return { ...layoutCanvas(arranged, size, shape, wraps), size };
  }, [graph, sizeOf, direction, density, level, isSection, wrap, arranged, screen]);
  const { positions } = shown;
  const shownSize = shown.size;
  const across = useCallback((parent: string) => shown.across.get(parent) ?? false, [shown]);

  useEffect(() => {
    const e = engine.current;
    if (!e) return;
    const meta = new Map<string, SceneMeta>();
    const sz = new Map<string, Size>();
    for (const n of graph.nodes.values()) {
      meta.set(n.key, { busy: n.busy, parent: n.parent });
      sz.set(n.key, shownSize(n.key));
    }
    // Only the edges the layout drew: a section's cards sit under its header, a grid row's later cards beside its first.
    const edges: SceneEdge[] = [];
    for (const x of graph.edges) {
      const points = shown.routes.get(`${x.from}>${x.to}`);
      if (points) edges.push({ ...x, dashed: false, points });
    }
    e.setScene(positions, meta, edges, sz);
  }, [positions, graph, shownSize, shown]);

  // How much of the canvas's bottom `keepClear` covers now (at most half of it).
  const coveredBelow = useCallback(() => {
    const el = keepClear?.current;
    const c = container.current;
    if (!el || !c) return 0;
    const box = c.getBoundingClientRect();
    const top = el.getBoundingClientRect().top;
    return Math.max(0, Math.min(box.bottom - top, box.height / 2));
  }, [keepClear]);

  const mattersRef = useRef(matters);
  mattersRef.current = matters;
  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  /**
   * Fit: everything, filling about 80 % of the room above the prompt bar, but never smaller than
   * the cards read (`READABLE`): then from the top (a phone: its first machine or status column).
   * `all` (the Fit button, `1`) frames what matters at a readable zoom; again from there, everything.
   */
  const fit = useCallback(
    (animate = true, all = false) => {
      const e = engine.current;
      if (!e) return;
      const { w } = e.viewport;
      const h = e.viewport.h - coveredBelow();
      if (!w || h <= 0) return;
      const phone = w < 640;
      // A phone's waiting request puts the zoom bar at the top (`data-waiting`): frame below it.
      const top = phone && container.current?.closest("[data-waiting]") ? PHONE_TOP_BAR : 0;
      const room = roomFor(w, h - top);
      const r = bounds(positions, shownSize);
      if (!r) return;
      let column: string | undefined;
      if (phone)
        for (const [k, p] of positions)
          if (graph.nodes.get(k)?.kind === "group" && (column === undefined || p.x < positions.get(column)!.x)) column = k;
      const key = column ?? arranged.roots.find((k) => positions.has(k));
      const lead = key ? (bounds(positions, shownSize, [key]) ?? undefined) : undefined;
      // Asked for (the Fit button, `1`): follow the work again, so cards that grow or arrive stay framed.
      if (all) following.current = true;
      // Bigger than the screen at the floor: the minimap shows in the bottom right, so the frame
      // keeps clear of its corner (the layout was planned so; `planWraps`).
      const over = r.w * room.floor > room.w - room.pad * 2 + 0.5 || r.h * room.floor > room.h - room.pad * 2 + 0.5;
      const wide = over ? room.w - (room.reserve ?? 0) : room.w;
      let v = fitView(r, wide, room.h, room.pad, 1, all ? 0 : room.floor, lead);
      if (all && v.k < room.floor) {
        // Too much to read at once: at the floor (the columns the layout planned all show), framed
        // on what matters (the structure, what's running, waiting on you or failed, the open card);
        // Fit again from there shows everything.
        const keys = [...positions.keys()].filter((k) => {
          const n = graph.nodes.get(k);
          return !!n && (FRAMED.has(n.kind) || n.busy || k === selectedRef.current || !!mattersRef.current?.(k));
        });
        const m = bounds(positions, shownSize, keys);
        const framed = m && fitView(r, wide, room.h, room.pad, room.floor, room.floor, m);
        const now = e.goal;
        const there = framed && Math.abs(now.k - framed.k) < 0.01 && Math.abs(now.x - framed.x) < 2 && Math.abs(now.y - framed.y - top) < 2;
        if (framed && !there) v = framed;
      }
      e.setView({ ...v, y: v.y + top }, animate);
    },
    [positions, shownSize, arranged, coveredBelow, graph],
  );

  // Fit once, when the first layout with measured cards is in; then keep everything in view as
  // work appears, until the user moves the view themselves.
  const following = useRef(true);
  // Another canvas view: frame it as it was left on this device (or fit it), easing there.
  const framedKey = useRef(storageKey);
  const easeIn = useRef(false);
  useLayoutEffect(() => {
    if (framedKey.current === storageKey) return;
    framedKey.current = storageKey;
    if (!fitted.current) return;
    fitted.current = false;
    following.current = true;
    easeIn.current = true;
  }, [storageKey]);
  // A new filter shows other cards: follow them again (the extent effect fits, easing).
  const fitFor = useRef(fitKey);
  useLayoutEffect(() => {
    if (fitFor.current === fitKey) return;
    fitFor.current = fitKey;
    following.current = true;
  }, [fitKey]);
  // The tree turned: everything moves, so frame it again (the cards ease there with the view).
  const shownDirection = useRef(direction);
  // `fit`, then the open conversation's card revealed (set below, once `reveal` exists).
  const frameRef = useRef<(animate: boolean) => void>(() => {});
  useEffect(() => {
    if (shownDirection.current === direction) return;
    shownDirection.current = direction;
    following.current = true;
    frameRef.current(true);
  }, [direction]);
  // The canvas got narrower or wider (the side panel opened or closed, the window resized): frame
  // it again, unless the view was moved by hand (then it stays where it was put).
  useEffect(() => {
    const c = container.current;
    if (!c) return;
    let width = c.getBoundingClientRect().width;
    let t: ReturnType<typeof setTimeout> | undefined;
    const shape = () => {
      const r = c.getBoundingClientRect();
      // In steps of 32 px, so a few pixels of the prompt bar growing don't lay everything out again.
      const step = (v: number) => Math.max(32, Math.round(v / 32) * 32);
      if (r.width && r.height) setScreen((s) => (s.w === step(r.width) && s.h === step(r.height - coveredBelow()) ? s : { w: step(r.width), h: step(r.height - coveredBelow()) }));
    };
    shape();
    const ro = new ResizeObserver(() => {
      shape();
      const now = c.getBoundingClientRect().width;
      if (Math.abs(now - width) < 24) return;
      width = now;
      clearTimeout(t);
      t = setTimeout(() => fitted.current && following.current && frameRef.current(true), 120);
    });
    ro.observe(c);
    return () => {
      ro.disconnect();
      clearTimeout(t);
    };
  }, []);

  // The stream re-renders constantly; only a change in the overall extent refits.
  const extent = useMemo(() => {
    const r = bounds(positions, shownSize);
    return r ? [r.x, r.y, r.w, r.h].map(Math.round).join(",") : "";
  }, [positions, shownSize]);
  useEffect(() => {
    if (!extent) return;
    if (!fitted.current) {
      const e = engine.current;
      // Nothing to frame into yet (a phone's canvas can have no size as the first cards land): the
      // canvas taking its shape (`screen`) comes back here.
      if (sizes.size === 0 || !e?.viewport.w || !e.viewport.h) return;
      fitted.current = true;
      // Back where it was left on this device, if that still shows some of the canvas.
      const saved = loadView(storageKey);
      const r = bounds(positions, shownSize);
      const animate = easeIn.current;
      easeIn.current = false;
      if (saved && r && overlaps(r, saved, e.viewport)) {
        following.current = false;
        e.setView(saved, animate);
      } else frameRef.current(animate);
      return;
    }
    if (!following.current) return;
    const t = setTimeout(() => following.current && frameRef.current(true), 250);
    return () => clearTimeout(t);
    // `fit` changes with every layout; the extent is what matters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [extent, sizes.size > 0, storageKey, fitKey, screen]);
  useEffect(() => {
    const c = container.current;
    if (!c) return;
    // Only moving the view by hand stops following: a drag on the background, the wheel, the
    // pan and zoom keys. A click on a card (to open it) moves nothing.
    const stop = () => (following.current = false);
    const down = (e: PointerEvent) => !(e.target as HTMLElement | null)?.closest?.("[data-card]") && stop();
    const key = (e: KeyboardEvent) => VIEW_KEYS.has(e.key) && !(e.target as HTMLElement | null)?.closest?.("[data-card]") && stop();
    c.addEventListener("pointerdown", down);
    c.addEventListener("wheel", stop, { passive: true });
    c.addEventListener("keydown", key);
    return () => {
      c.removeEventListener("pointerdown", down);
      c.removeEventListener("wheel", stop);
      c.removeEventListener("keydown", key);
    };
  }, []);

  // The view is remembered once the user moves it (not while it follows new work).
  useEffect(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    viewChanged.current = (v) => {
      if (following.current || !fitted.current) return;
      clearTimeout(t);
      t = setTimeout(() => saveView(storageKey, v), 400);
    };
    return () => {
      clearTimeout(t);
      viewChanged.current = null;
    };
  }, [storageKey]);

  const focus = useCallback(
    (asked: string, k?: number, auto = false) => {
      const e = engine.current;
      const key = host?.get(asked) ?? asked;
      if (!e) return;
      const nk = k ?? Math.max(e.view.k, 0.85);
      const p = positions.get(key);
      if (!p) return;
      // Asked for (a card shown from ⌘K, the keyboard): the view is the person's now. `auto` (a
      // panel opening reveals its card) leaves the canvas following its work.
      if (!auto) following.current = false;
      const s = shownSize(key);
      const { w } = e.viewport;
      const h = e.viewport.h - coveredBelow();
      // Keep tall cards' tops in view, below the canvas toolbar.
      const cy = s.h * nk > h - 96 ? p.y + (h / 2 - 64) / nk : p.y + s.h / 2;
      e.setView({ k: nk, x: w / 2 - (p.x + s.w / 2) * nk, y: h / 2 - cy * nk }, true);
    },
    [positions, shownSize, coveredBelow, host],
  );

  const reveal = useCallback(
    (asked: string) => {
      const e = engine.current;
      const key = host?.get(asked) ?? asked;
      const p = positions.get(key);
      if (!e || !p) return;
      // Where the view is going, not where an easing has got to: a reveal right after a refit
      // judges the frame the refit is heading for.
      const { x, y, k } = e.goal;
      const { w } = e.viewport;
      const h = e.viewport.h - coveredBelow();
      // The card and what's waiting beside it (its request cards) all in view, above the prompt bar.
      const r = bounds(positions, shownSize, [key, ...(graph.nodes.get(key)?.children.filter((c) => graph.nodes.get(c)?.kind === "request") ?? [])]) ?? { x: p.x, y: p.y, ...shownSize(key) };
      const inView = r.x * k + x >= 0 && r.y * k + y >= 0 && (r.x + r.w) * k + x <= w && (r.y + r.h) * k + y <= h;
      if (!inView) focus(key, k, true);
    },
    [positions, shownSize, focus, coveredBelow, graph, host],
  );

  // Framing by itself (following new work, the panel changing the canvas's width) keeps the open
  // conversation's card in view too: a refit that lands after the panel revealed it must not put
  // it back under the prompt bar. A layout effect, so it's set before the effects above frame the
  // first layout.
  useLayoutEffect(() => {
    frameRef.current = (animate: boolean) => {
      fit(animate);
      if (selected && positions.has(selected)) reveal(selected);
    };
  });

  useImperativeHandle(ref, () => ({ focus, reveal, fit: () => fit(true) }), [focus, reveal, fit]);

  // A card's siblings in the order shown.
  const siblingsOf = useCallback(
    (key: string) => {
      const parent = graph.nodes.get(key)?.parent ?? ROOTS;
      return { parent, list: [...(parent === ROOTS ? arranged.roots : (arranged.nodes.get(parent)?.children ?? []))] };
    },
    [graph, arranged],
  );

  // Dragging a card by its handle ([data-drag]) moves it among its siblings: they make room as it
  // passes them and it drops into the gap. Buttons and fields inside keep their clicks.
  const drag = useRef<{
    key: string;
    id: number;
    start: Point;
    from: Point;
    moved: boolean;
    el: HTMLElement;
    parent: string;
    list: string[];
    rects: Map<string, Rect>;
    /** The siblings sit side by side (measured on x), else one under another (on y), or fill a grid. */
    across: boolean | "grid";
  } | null>(null);
  // Dragging a launch handle out of a card: where it started, and the ghost line drawn meanwhile.
  const launch = useRef<{ source: string; id: number; start: Point; from: Point; moved: boolean } | null>(null);
  const [ghost, setGhost] = useState<{ source: string; from: Point; to: Point; over: boolean } | null>(null);
  useEffect(() => {
    if (!ghost) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      launch.current = null;
      setGhost(null);
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [ghost]);
  const onPointerDown = (ev: React.PointerEvent) => {
    const e = engine.current;
    if (!e || ev.button !== 0) return;
    const t = ev.target as HTMLElement;
    // A launch handle: a drag from it draws a line to where a new agent (or a fork) would start.
    const handle = onLaunch ? t.closest<HTMLElement>("[data-launch]") : null;
    if (handle?.dataset.launch) {
      const r = container.current!.getBoundingClientRect();
      const h = handle.getBoundingClientRect();
      launch.current = { source: handle.dataset.launch, id: ev.pointerId, start: { x: ev.clientX, y: ev.clientY }, from: { x: h.left + h.width / 2 - r.left, y: h.top + h.height / 2 - r.top }, moved: false };
      return;
    }
    if (!t.closest("[data-drag]") || t.closest("button, a, input, textarea, select, [data-nodrag]")) return;
    const card = t.closest<HTMLElement>("[data-card]");
    const key = card?.dataset.key;
    const from = key && e.positionOf(key);
    if (!key || !from) return;
    const r = container.current!.getBoundingClientRect();
    const { parent, list } = siblingsOf(key);
    // Where the siblings are as the drag starts: the gap moves, what it's measured against doesn't.
    const rects = new Map(list.map((k) => [k, { ...(positions.get(k) ?? { x: 0, y: 0 }), ...shownSize(k) }]));
    drag.current = { key, id: ev.pointerId, start: toWorld(e.view, ev.clientX - r.left, ev.clientY - r.top), from, moved: false, el: card!, parent, list, rects, across: across(parent) };
  };
  const onPointerMove = (ev: React.PointerEvent) => {
    const l = launch.current;
    if (l && l.id === ev.pointerId) {
      if (!l.moved && Math.hypot(ev.clientX - l.start.x, ev.clientY - l.start.y) < 6) return;
      // Capture once it's a drag, so the handle's click still works.
      if (!l.moved) container.current!.setPointerCapture?.(ev.pointerId);
      l.moved = true;
      const r = container.current!.getBoundingClientRect();
      setGhost({ source: l.source, from: l.from, to: { x: ev.clientX - r.left, y: ev.clientY - r.top }, over: overCard(ev.clientX, ev.clientY) });
      return;
    }
    const d = drag.current;
    const e = engine.current;
    if (!d || !e || d.id !== ev.pointerId) return;
    const r = container.current!.getBoundingClientRect();
    const p = toWorld(e.view, ev.clientX - r.left, ev.clientY - r.top);
    const dx = p.x - d.start.x;
    const dy = p.y - d.start.y;
    if (!d.moved && Math.hypot(dx, dy) * e.view.k < 4) return;
    // Capture only once it's a drag, so a plain click still reaches the card.
    if (!d.moved) d.el.setPointerCapture(ev.pointerId);
    d.moved = true;
    const at = { x: d.from.x + dx, y: d.from.y + dy };
    e.setDrag(d.key, at);
    if (d.list.length < 2) return;
    const s = shownSize(d.key);
    const list = moveTo(d.list, d.key, dropIndex(d.list, d.key, { x: at.x + s.w / 2, y: at.y + s.h / 2 }, d.rects, d.across));
    setPreview((cur) => (cur && cur.parent === d.parent && cur.list.join() === list.join() ? cur : { parent: d.parent, list }));
  };
  const onPointerUp = (ev: React.PointerEvent) => {
    const l = launch.current;
    if (l && l.id === ev.pointerId) {
      launch.current = null;
      setGhost(null);
      if (!l.moved) return;
      swallowClick();
      // Dropped on a card (or the drag was cancelled): nothing starts.
      if (ev.type === "pointercancel" || overCard(ev.clientX, ev.clientY)) return;
      onLaunch?.(l.source, { x: ev.clientX, y: ev.clientY });
      return;
    }
    const d = drag.current;
    const e = engine.current;
    if (!d || !e || d.id !== ev.pointerId) return;
    drag.current = null;
    if (!d.moved) return;
    e.setDrag(d.key, null);
    if (preview && preview.list.join() !== d.list.join()) setOrder(withOrder(order, preview.parent, preview.list));
    setPreview(null);
    // The drag ends in a click on the card; swallow it.
    const swallow = (c: MouseEvent) => {
      c.stopPropagation();
      c.preventDefault();
    };
    window.addEventListener("click", swallow, { capture: true, once: true });
    setTimeout(() => window.removeEventListener("click", swallow, { capture: true }), 0);
  };

  /** ⌥↑ / ⌥↓ (⌥← / ⌥→ for siblings side by side) on a focused card. */
  const nudge = useCallback(
    (key: string, by: number) => {
      const { parent, list } = siblingsOf(key);
      const i = list.indexOf(key);
      if (i < 0 || i + by < 0 || i + by >= list.length) return;
      setOrder(withOrder(order, parent, moveTo(list, key, i + by)));
    },
    [siblingsOf, order, setOrder],
  );

  const tidy = () => setOrder({});

  // ── moving between cards from the keyboard ──

  const slotOf = useCallback((key: string) => world.current?.querySelector<HTMLElement>(`[data-card][data-key="${CSS.escape(key)}"]`) ?? null, []);

  // One tab stop for the canvas (WAI-ARIA's roving tabindex): the card last focused, else the
  // first in reading order. Tab goes on into that card's own controls, then out of the canvas; the
  // arrows go from card to card. Every other card's controls are out of the tab order (they took
  // 39 Tabs before the first conversation).
  const [current, setCurrent] = useState<string | null>(null);
  const entry = useMemo(() => {
    if (current && positions.has(current)) return current;
    let best: string | null = null;
    for (const [k, p] of positions) {
      const b = best ? positions.get(best)! : null;
      if (!b || p.y < b.y - 0.5 || (Math.abs(p.y - b.y) <= 0.5 && p.x < b.x)) best = k;
    }
    return best;
  }, [current, positions]);
  useLayoutEffect(() => {
    const root = world.current;
    if (!root) return;
    const apply = (slots: Iterable<HTMLElement>) => {
      for (const slot of slots) rove(slot, slot.dataset.key === entry && !slot.hasAttribute("data-leaving"));
    };
    apply(root.querySelectorAll<HTMLElement>("[data-card]"));
    // Cards come and go and render their insides on their own (a request's answers, a terminal,
    // streamed text): keep up with the ones that changed, not every card on each mutation.
    let frame = 0;
    const dirty = new Set<HTMLElement>();
    const seen = new MutationObserver((records) => {
      for (const r of records) {
        const slot = (r.target as Element).closest?.<HTMLElement>("[data-card]");
        if (slot) dirty.add(slot);
        else for (const n of r.addedNodes) if (n instanceof HTMLElement && n.matches("[data-card]")) dirty.add(n);
      }
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        apply(dirty);
        dirty.clear();
      });
    });
    seen.observe(root, { childList: true, subtree: true });
    return () => {
      seen.disconnect();
      cancelAnimationFrame(frame);
    };
  }, [entry]);
  /** The card a focused element is the card itself of (not a control inside it). */
  const cardAt = useCallback((el: HTMLElement | null) => {
    const slot = el?.closest<HTMLElement>("[data-card]");
    return slot && world.current?.contains(slot) && (el === slot || el === focusTarget(slot)) ? slot : null;
  }, []);
  const goTo = useCallback(
    (key: string, answer = false) => {
      const slot = slotOf(key);
      if (!slot) return;
      (answer ? (slot.querySelector<HTMLElement>(ANSWER) ?? focusTarget(slot)) : focusTarget(slot)).focus({ preventScroll: true });
      reveal(key);
    },
    [slotOf, reveal],
  );
  /** Space: the request waiting on this card, else the next one waiting anywhere (in reading order, round again). */
  const requestFrom = useCallback(
    (key: string): string | null => {
      const own = graph.nodes.get(key)?.children.find((c) => graph.nodes.get(c)?.kind === "request");
      if (own) return own;
      const reading: string[] = [];
      const walk = (k: string) => {
        reading.push(k);
        for (const c of arranged.nodes.get(k)?.children ?? []) walk(c);
      };
      arranged.roots.forEach(walk);
      const requests = reading.filter((k) => graph.nodes.get(k)?.kind === "request" && k !== key);
      const at = reading.indexOf(key);
      return requests.find((k) => reading.indexOf(k) > at) ?? requests[0] ?? null;
    },
    [graph, arranged],
  );
  // Every request on the canvas in reading order: the toolbar's "next waiting on you" walks them.
  const waiting = useMemo(() => {
    const out: string[] = [];
    const walk = (k: string) => {
      if (graph.nodes.get(k)?.kind === "request") out.push(k);
      for (const c of arranged.nodes.get(k)?.children ?? []) walk(c);
    };
    arranged.roots.forEach(walk);
    return out;
  }, [graph, arranged]);
  const lastWaiting = useRef<string | null>(null);
  const nextWaiting = useCallback(() => {
    if (!waiting.length) return;
    const at = lastWaiting.current ? waiting.indexOf(lastWaiting.current) : -1;
    const to = waiting[(at + 1) % waiting.length];
    lastWaiting.current = to;
    following.current = false;
    goTo(to, true);
  }, [waiting, goTo]);
  useEffect(() => {
    if (!active) return;
    // Before the card's own Space (which would open it) and the engine's space-to-pan.
    const onSpace = (ev: KeyboardEvent) => {
      if (ev.key !== " " || ev.metaKey || ev.ctrlKey || ev.altKey || ev.shiftKey) return;
      const slot = cardAt(ev.target as HTMLElement | null);
      const to = slot && requestFrom(slot.dataset.key!);
      if (!to) return;
      ev.preventDefault();
      ev.stopPropagation();
      goTo(to, true);
    };
    window.addEventListener("keydown", onSpace, true);
    return () => window.removeEventListener("keydown", onSpace, true);
  }, [active, cardAt, requestFrom, goTo]);

  // Keyboard, when nothing editable has focus.
  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      const e = engine.current;
      const el = ev.target as HTMLElement | null;
      if (!e || !active || ev.metaKey || ev.ctrlKey) return;
      if (ev.altKey) {
        const key = el?.closest<HTMLElement>("[data-card]")?.dataset.key;
        // Along the axis its siblings are laid out on.
        const [back, on] = key && across(siblingsOf(key).parent) === true ? ["ArrowLeft", "ArrowRight"] : ["ArrowUp", "ArrowDown"];
        if (key && (ev.key === back || ev.key === on)) {
          ev.preventDefault();
          nudge(key, ev.key === back ? -1 : 1);
        }
        return;
      }
      if (el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName) || el.closest("[role=dialog]:not([data-surface]), [role=menu], [role=listbox], .xterm"))) return;
      // On a card: the arrows go to the next card that way, Enter opens it.
      const slot = cardAt(el);
      if (slot && !ev.shiftKey) {
        const key = slot.dataset.key!;
        const arrow = ARROWS[ev.key];
        if (arrow) {
          ev.preventDefault();
          const rects = new Map<string, Rect>();
          for (const [k, p] of positions) rects.set(k, { ...p, ...shownSize(k) });
          const to = nextCard(rects, key, arrow);
          if (to) goTo(to);
          return;
        }
        if (ev.key === "Enter" && el === slot) {
          ev.preventDefault();
          if (!onOpen?.(key)) slot.querySelector<HTMLElement>(FOCUSABLE)?.focus();
          return;
        }
      }
      const step = 80;
      switch (ev.key) {
        case "+":
        case "=":
          e.zoomBy(1.25);
          break;
        case "-":
        case "_":
          e.zoomBy(0.8);
          break;
        case "0":
          e.zoomBy(1 / e.view.k);
          break;
        case "1":
        case "!":
          fit(true, true);
          break;
        case "ArrowLeft":
          e.panBy(step, 0, true);
          break;
        case "ArrowRight":
          e.panBy(-step, 0, true);
          break;
        case "ArrowUp":
          e.panBy(0, step, true);
          break;
        case "ArrowDown":
          e.panBy(0, -step, true);
          break;
        case "D":
          if (!onDirection) return;
          onDirection(direction === "vertical" ? "horizontal" : "vertical");
          break;
        case "Escape":
          onEscape?.();
          return;
        default:
          return;
      }
      ev.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [fit, onEscape, active, nudge, direction, onDirection, across, siblingsOf, cardAt, positions, shownSize, goTo, onOpen]);

  const arrangedByUser = Object.keys(order).length > 0;

  // Many cards arriving at once (a filter cleared, 200 threads unfolded, the first load) mount a
  // few frames apart, STAGE at a time in the graph's order: 200 at once were one ~150 ms task of
  // style, layout and paint. Their edges join them as they're placed.
  const mounted = useRef<ReadonlySet<string>>(new Set());
  // Cards that arrived with many others: they mount still (no rise, no fade in), so a hundred
  // entrances at once don't repaint the canvas for their whole length.
  const still = useRef(new Set<string>());
  const staging = useRef(false);
  // Many leaving at once (a filter's first key): they go without their exit, read by each as it leaves.
  const massExit = useRef(false);
  const [, nextStage] = useReducer((n: number) => n + 1, 0);
  const all = [...graph.nodes.values()];
  const arriving = all.filter((n) => !mounted.current.has(n.key)).length;
  let leaving = 0;
  for (const k of mounted.current) if (!graph.nodes.has(k)) leaving++;
  massExit.current = leaving > STAGE;
  let fresh = 0;
  const staged = all.filter((n) => mounted.current.has(n.key) || fresh++ < STAGE);
  // One element per node (the graph keeps a node that didn't change): its slot doesn't render again.
  const cards = useMemo(() => new WeakMap<GraphNode, ReactNode>(), [render]);
  const cardOf = (n: GraphNode) => {
    if (!cards.has(n)) cards.set(n, render(n));
    return cards.get(n);
  };
  if (arriving > STAGE || staging.current) for (const n of staged) if (!mounted.current.has(n.key)) still.current.add(n.key);
  useLayoutEffect(() => {
    mounted.current = new Set(staged.map((n) => n.key));
    staging.current = staged.length < all.length;
    for (const k of still.current) if (!graph.nodes.has(k)) still.current.delete(k);
    if (staged.length === all.length) return;
    const id = requestAnimationFrame(nextStage);
    return () => cancelAnimationFrame(id);
  });

  // A section's header row spans its cards (`--section-w`), and in the overview a grid's pills share
  // their column's width (`--cell-w`): set on the slots, so no card renders again for it.
  useLayoutEffect(() => {
    const root = world.current;
    if (!root) return;
    for (const slot of root.querySelectorAll<HTMLElement>("[data-card]")) {
      const v = shown.widths.get(slot.dataset.key!);
      const text = v === undefined ? "" : `${Math.round(v)}px`;
      if (slot.style.getPropertyValue("--section-w") === text) continue;
      if (text) slot.style.setProperty("--section-w", text);
      else slot.style.removeProperty("--section-w");
    }
  }, [shown, staged.length]);

  return (
    <div
      ref={container}
      className="relative h-full w-full touch-none overflow-hidden overscroll-none bg-canvas text-fg select-none"
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      aria-label="Agents canvas"
      role="application"
      data-direction={direction}
      data-density={density ?? "comfortable"}
    >
      <canvas ref={canvas} aria-hidden className="pointer-events-none absolute inset-0" />
      <div
        ref={world}
        className="absolute top-0 left-0 origin-top-left"
        style={{ willChange: "transform" }}
        onFocus={(e) => {
          const k = (e.target as HTMLElement).closest<HTMLElement>("[data-card]")?.dataset.key;
          if (k && k !== current) setCurrent(k);
        }}
      >
        {/* A card that leaves the graph fades and shrinks where it was before it unmounts (it was gone in a frame).
            The engine places cards, not motion's layout: with `presenceAffectsLayout` (the default) every
            render gave every card a new presence context, so each streamed record rendered all of them. */}
        <AnimatePresence initial={false} presenceAffectsLayout={false}>
          {staged.map((n) => (
            <CardSlot key={n.key} nodeKey={n.key} kind={n.kind} engine={engine} still={still.current.has(n.key)} massExit={massExit} selected={n.key === selected || !!highlighted?.includes(n.key)}>
              {cardOf(n)}
            </CardSlot>
          ))}
        </AnimatePresence>
      </div>
      {overlay}
      {ghost && <LaunchGhost {...ghost} />}
      <Minimap ref={minimap} positions={positions} sizeOf={shownSize} nodes={graph.nodes} engine={engine} />
      {/* At the top while a phone's waiting request sits at the bottom (`data-waiting` above), so both can be reached. */}
      <div className="chrome-in absolute right-3 bottom-3 flex items-center gap-0.5 rounded-[10px] border border-line-2 p-0.5 text-muted shadow-pop inset-shadow-highlight glass [[data-waiting]_&]:top-3 [[data-waiting]_&]:bottom-auto">
        <ToolButton label="Zoom out (−)" onClick={() => engine.current?.zoomBy(0.8)}>
          <Minus className="size-3.5" />
        </ToolButton>
        <button
          onClick={() => engine.current?.zoomBy(1 / (engine.current?.view.k ?? 1))}
          className="h-7 w-12 rounded-md text-center font-mono text-[11px] tabular-nums transition-colors duration-150 hover:bg-panel-2 hover:text-fg"
          title="Zoom to 100% (0)"
        >
          <span ref={zoomText} />
        </button>
        <ToolButton label="Zoom in (+)" onClick={() => engine.current?.zoomBy(1.25)}>
          <Plus className="size-3.5" />
        </ToolButton>
        {waiting.length > 0 && (
          <button
            onClick={nextWaiting}
            title={`Next waiting on you (${waiting.length})`}
            aria-label={`Next waiting on you, ${waiting.length} in all`}
            className="flex h-7 items-center gap-1 rounded-md px-1.5 font-mono text-[11px] text-accent tabular-nums transition-colors duration-150 hover:bg-accent/10"
          >
            <CircleDot className="size-3.5 stroke-[1.75]" />
            {waiting.length}
          </button>
        )}
        <ToolButton label="Fit (1); again for everything" onClick={() => fit(true, true)}>
          <Maximize className="size-3.5" />
        </ToolButton>
        {onDirection && (
          <ToolButton
            label="Top-down (⇧D)"
            onClick={() => onDirection(direction === "vertical" ? "horizontal" : "vertical")}
            pressed={direction === "vertical"}
          >
            {direction === "vertical" ? <ArrowDownFromLine className="size-3.5" /> : <ArrowRightFromLine className="size-3.5" />}
          </ToolButton>
        )}
        {arrangedByUser && (
          <ToolButton label="Back to the default order" onClick={tidy}>
            <ListRestart className="size-3.5" />
          </ToolButton>
        )}
      </div>
    </div>
  );
}));

const ARROWS: Partial<Record<string, Arrow>> = { ArrowLeft: "left", ArrowRight: "right", ArrowUp: "up", ArrowDown: "down" };
/** What a card's answer is: a question's options, else its primary button (Approve, Implement). */
const ANSWER = "[role=radio], [role=checkbox], [data-answer]:not([disabled])";
const FOCUSABLE = "button:not([disabled]), [href], input, textarea, select, [tabindex='0']";

/** What takes focus for a card: its own clickable frame (a thread's), else its slot. */
function focusTarget(slot: HTMLElement): HTMLElement {
  return slot.querySelector<HTMLElement>("[data-drag][role=button]") ?? slot;
}

const TABBABLE = "a[href], button, input, select, textarea, [tabindex]";

/**
 * A card in or out of the tab order: the one the canvas's tab stop is on keeps its card at 0 and
 * its controls as they are; any other is -1 throughout (the original tabindex is kept in
 * `data-roving` to put back).
 */
function rove(slot: HTMLElement, on: boolean) {
  const target = focusTarget(slot);
  const want = on ? "0" : "-1";
  if (target.getAttribute("tabindex") !== want) target.setAttribute("tabindex", want);
  if (target !== slot && slot.getAttribute("tabindex") !== "-1") slot.setAttribute("tabindex", "-1");
  for (const el of slot.querySelectorAll<HTMLElement>(TABBABLE)) {
    if (el === target) continue;
    if (on) {
      if (!el.hasAttribute("data-roving")) continue;
      const was = el.getAttribute("data-roving");
      if (was) el.setAttribute("tabindex", was);
      else el.removeAttribute("tabindex");
      el.removeAttribute("data-roving");
    } else if (!el.hasAttribute("data-roving")) {
      el.setAttribute("data-roving", el.getAttribute("tabindex") ?? "");
      el.setAttribute("tabindex", "-1");
    }
  }
}

/** Whether a point (client coordinates) is on a card. */
function overCard(x: number, y: number): boolean {
  return !!document.elementFromPoint?.(x, y)?.closest("[data-card]");
}

/** The click a drag ends in, swallowed. */
function swallowClick() {
  const swallow = (c: MouseEvent) => {
    c.stopPropagation();
    c.preventDefault();
  };
  window.addEventListener("click", swallow, { capture: true, once: true });
  setTimeout(() => window.removeEventListener("click", swallow, { capture: true }), 0);
}

/**
 * The line from a launch handle to the pointer, calm: a dashed accent line, a dot where it would
 * start, and what letting go does. Over a card it fades (nothing starts there).
 */
function LaunchGhost({ source, from, to, over }: { source: string; from: Point; to: Point; over: boolean }) {
  const label = over ? "Drop on empty canvas" : source.startsWith("t:") ? "Fork here" : "New agent here";
  return (
    <>
      <svg aria-hidden className="fade-in pointer-events-none absolute inset-0 z-20 h-full w-full">
        <line x1={from.x} y1={from.y} x2={to.x} y2={to.y} stroke="var(--accent)" strokeOpacity={over ? 0.25 : 0.7} strokeWidth={1.5} strokeDasharray="4 5" strokeLinecap="round" />
        <circle cx={from.x} cy={from.y} r={3} fill="var(--accent)" fillOpacity={over ? 0.3 : 0.9} />
        <circle cx={to.x} cy={to.y} r={6} fill="var(--accent)" fillOpacity={over ? 0.05 : 0.15} stroke="var(--accent)" strokeOpacity={over ? 0.3 : 0.8} />
      </svg>
      <div
        role="status"
        className={`pointer-events-none absolute z-20 rounded-md border border-line-2 px-2 py-1 text-[11.5px] whitespace-nowrap shadow-pop glass ${over ? "text-subtle" : "text-fg"}`}
        style={{ left: to.x + 14, top: to.y + 12 }}
      >
        {label}
      </div>
    </>
  );
}
/** Fit doesn't zoom out further than cards read (titles ~10 px, meta ~8 px); `1` twice does. */
const READABLE = 0.75;
/** The minimap's corner (its width and margin), kept clear while the canvas is bigger than the screen. */
const MINIMAP_RESERVE = 192;

/**
 * The room a canvas `w` × `h` (above the prompt bar) is planned and fitted for: about 80 % of it
 * (a 10 % pad, at least 48 px), never below where cards read (a phone: 100 %).
 */
function roomFor(w: number, h: number): Room {
  const phone = w < 640;
  return { w, h, pad: phone ? 24 : Math.max(48, Math.min(w, h) * 0.1), floor: phone ? 1 : READABLE, reserve: w >= 900 ? MINIMAP_RESERVE : 0 };
}
/** Below this a card's details hide (`[data-brief]`), its titles stay. */
const BRIEF = 0.55;
/** Below this a card's text is a bar (`[data-greek]`). */
const GREEK = 0.4;
/** Below this the identity edges thicken again (`[data-far]`). */
const FAR = 0.3;
const ZERO: Size = { w: 0, h: 0 };
/** A status group's header row in the cards' layout (StatusHeader). */
const SECTION_HEAD: Size = { w: 296, h: 40 };


/** How many new cards mount in one frame (more wait for the next). */
const STAGE = 48;
/** The zoom readout changes at most this often while the zoom moves. */
const ZOOM_TEXT_MS = 150;
/** What Fit always frames: the structure, and what waits on you. */
const FRAMED: ReadonlySet<GraphNode["kind"]> = new Set(["machine", "project", "group", "request"]);
/** Keys that pan or zoom the canvas (with no card focused). */
const VIEW_KEYS = new Set(["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "+", "=", "-", "0"]);
/** Room for a phone's zoom bar when it sits at the top (while a request waits at the bottom). */
const PHONE_TOP_BAR = 48;

/** Whether `r` (world) shows at all in a `viewport` seen through `v`. */
function overlaps(r: { x: number; y: number; w: number; h: number }, v: View, viewport: { w: number; h: number }) {
  const x0 = r.x * v.k + v.x;
  const y0 = r.y * v.k + v.y;
  return x0 < viewport.w && y0 < viewport.h && x0 + r.w * v.k > 0 && y0 + r.h * v.k > 0;
}

function ToolButton({ label, onClick, pressed, children }: { label: string; onClick: () => void; pressed?: boolean; children: ReactNode }) {
  return (
    <button onClick={onClick} title={label} aria-label={label} aria-pressed={pressed} className="grid size-7 place-items-center rounded-md transition-colors duration-150 hover:bg-panel-2 hover:text-fg aria-pressed:bg-fg/[0.07] aria-pressed:text-fg [&>svg]:stroke-[1.75]">
      {children}
    </button>
  );
}

/**
 * A card's slot in the world layer; the engine positions it. It comes in with `.rise`; leaving the
 * graph, it stays where it was, inert and out of the pointer's way, fading and shrinking to 0.96
 * over 160 ms, then unmounts; back while it goes, it turns around.
 */
/**
 * `still`: it arrived with many others, so it mounts in place without its entrance (it still leaves as one).
 * Memoised on its key and its card (the page's `render` makes a card again only when what it shows
 * changed): a record arriving renders its own card, not every slot's wrappers (~1,700 components a
 * batch at 1000 threads).
 */
const CardSlot = memo(function CardSlot({ nodeKey, kind, engine, selected, still, massExit, children }: { nodeKey: string; kind: GraphNode["kind"]; engine: React.RefObject<CanvasEngine | null>; selected: boolean; still: boolean; massExit: React.RefObject<boolean>; children: ReactNode }) {
  const el = useRef<HTMLDivElement>(null);
  const reduced = useReducedMotion();
  const fade = useMotionValue(1);
  const scale = useMotionValue(1);
  const none: Transition = { duration: 0 };
  const present = usePresenceTracks(
    [
      { value: fade, shown: 1, hidden: 0, enter: reduced || still ? none : POPOVER_IN, leave: reduced || massExit.current ? none : CARD_OUT },
      { value: scale, shown: 1, hidden: 0.96, enter: reduced || still ? none : POPOVER_IN, leave: reduced || massExit.current ? none : CARD_OUT },
    ],
    String(reduced),
  );
  useLayoutEffect(() => {
    const e = engine.current;
    const d = el.current;
    if (!e || !d) return;
    e.register(nodeKey, d);
    return () => e.unregister(nodeKey, d);
  }, [engine, nodeKey]);
  return (
    <div
      ref={el}
      data-card
      data-key={nodeKey}
      data-kind={kind}
      data-selected={(present && selected) || undefined}
      data-leaving={!present || undefined}
      inert={!present}
      aria-hidden={!present || undefined}
      tabIndex={-1}
      onPointerDown={(e) => {
        // A tap shows this card's tools on a touch screen (globals.css, `data-tapped`): not
        // focus, which Safari doesn't move on a tap or a click.
        if (e.pointerType !== "touch") return;
        for (const t of document.querySelectorAll("[data-card][data-tapped]")) if (t !== e.currentTarget) t.removeAttribute("data-tapped");
        e.currentTarget.setAttribute("data-tapped", "");
      }}
      className={`absolute top-0 left-0 rounded-xl outline-hidden focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent/70 ${present ? "" : "pointer-events-none"}`}
      style={{ transform: "translate(-99999px, 0)" }}
    >
      <motion.div style={{ opacity: fade, scale }}>
        <div data-inner className={still ? undefined : "rise"}>{children}</div>
      </motion.div>
    </div>
  );
});
