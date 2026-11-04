"use client";

import { Maximize, Minus, Plus, Shrink } from "lucide-react";
import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { useReducedMotion } from "@roll/brand";
import type { Graph, GraphNode } from "@/lib/agents-graph";
import {
  bounds,
  fitView,
  loadPositions,
  savePositions,
  tidyLayout,
  toWorld,
  withManual,
  type Point,
  type Size,
  type View,
} from "@/lib/agents-layout";
import { CanvasEngine, type SceneMeta } from "./canvas-engine";

export interface CanvasHandle {
  /** Pan (and zoom) so a node is in view; `zoom` sets the scale. */
  focus: (key: string, zoom?: number) => void;
  fit: () => void;
}

/**
 * The canvas: the engine's canvas underneath, the cards in a world layer on top, placed by the
 * tidy layout plus whatever the user dragged (remembered per owner). Keyboard: + / − zoom, 0 is
 * 100 %, 1 fits, arrows pan, Escape calls `onEscape`.
 */
export const AgentsCanvas = forwardRef<
  CanvasHandle,
  {
    graph: Graph;
    /** Where dragged positions are remembered. */
    storageKey: string;
    render: (node: GraphNode) => ReactNode;
    defaultSize: (node: GraphNode) => Size;
    onEscape?: () => void;
    overlay?: ReactNode;
    /** Off while something covers the canvas: its keys go to that instead. */
    active?: boolean;
  }
>(function AgentsCanvas({ graph, storageKey, render, defaultSize, onEscape, overlay, active = true }, ref) {
  const reduced = useReducedMotion();
  const container = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const world = useRef<HTMLDivElement>(null);
  const engine = useRef<CanvasEngine | null>(null);
  const [sizes, setSizes] = useState<Map<string, Size>>(() => new Map());
  const [manual, setManual] = useState<Record<string, Point>>({});
  const [zoom, setZoom] = useState(1);
  const fitted = useRef(false);

  useEffect(() => setManual(loadPositions(storageKey)), [storageKey]);

  useLayoutEffect(() => {
    if (!container.current || !canvas.current || !world.current) return;
    const e = new CanvasEngine(container.current, canvas.current, world.current, {
      reducedMotion: reduced,
      onView: (v: View) => setZoom((z) => (Math.abs(z - v.k) > 0.005 ? v.k : z)),
      onSizes: (changed) =>
        setSizes((prev) => {
          const next = new Map(prev);
          for (const [k, s] of changed) next.set(k, s);
          return next;
        }),
    });
    engine.current = e;
    // Cards mounted before the engine existed register now.
    world.current.querySelectorAll<HTMLElement>("[data-card]").forEach((el) => e.register(el.dataset.key!, el));
    // Follow light/dark switches.
    const mq = window.matchMedia("(prefers-color-scheme: light)");
    const onTheme = () => e.refreshColors();
    mq.addEventListener("change", onTheme);
    return () => {
      mq.removeEventListener("change", onTheme);
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

  const positions = useMemo(() => {
    const auto = tidyLayout(graph, (k) => (graph.nodes.has(k) ? sizeOf(k) : { w: 0, h: 0 }));
    return withManual(graph, auto, manual);
  }, [graph, sizeOf, manual]);

  useEffect(() => {
    const e = engine.current;
    if (!e) return;
    const meta = new Map<string, SceneMeta>();
    const sz = new Map<string, Size>();
    for (const n of graph.nodes.values()) {
      meta.set(n.key, { busy: n.busy, parent: n.parent });
      sz.set(n.key, sizeOf(n.key));
    }
    e.setScene(positions, meta, graph.edges, sz);
  }, [positions, graph, sizeOf]);

  const fit = useCallback(
    (animate = true) => {
      const e = engine.current;
      if (!e) return;
      const r = bounds(positions, sizeOf);
      const { w, h } = e.viewport;
      if (!r || !w || !h) return;
      // On a phone, readable cards beat seeing everything: pan from the machines on.
      e.setView(w < 640 ? fitView(r, w, h, 24, 1, 0.55) : fitView(r, w, h, 64, 1), animate);
    },
    [positions, sizeOf],
  );

  // Fit once, when the first layout with measured cards is in; then keep everything in view as
  // work appears, until the user moves the view themselves.
  const following = useRef(true);
  // The stream re-renders constantly; only a change in the overall extent refits.
  const extent = useMemo(() => {
    const r = bounds(positions, sizeOf);
    return r ? [r.x, r.y, r.w, r.h].map(Math.round).join(",") : "";
  }, [positions, sizeOf]);
  useEffect(() => {
    if (!extent) return;
    if (!fitted.current) {
      if (sizes.size === 0) return;
      fitted.current = true;
      fit(false);
      return;
    }
    if (!following.current) return;
    const t = setTimeout(() => following.current && fit(true), 250);
    return () => clearTimeout(t);
    // `fit` changes with every layout; the extent is what matters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [extent, sizes.size > 0]);
  useEffect(() => {
    const c = container.current;
    if (!c) return;
    const stop = () => (following.current = false);
    c.addEventListener("pointerdown", stop);
    c.addEventListener("wheel", stop, { passive: true });
    c.addEventListener("keydown", stop);
    return () => {
      c.removeEventListener("pointerdown", stop);
      c.removeEventListener("wheel", stop);
      c.removeEventListener("keydown", stop);
    };
  }, []);

  const focus = useCallback(
    (key: string, k?: number) => {
      const e = engine.current;
      const p = positions.get(key);
      if (!e || !p) return;
      following.current = false;
      const s = sizeOf(key);
      const { w, h } = e.viewport;
      const nk = k ?? Math.max(e.view.k, 0.85);
      // Keep tall cards' tops in view, below the canvas toolbar.
      const cy = s.h * nk > h - 96 ? p.y + (h / 2 - 64) / nk : p.y + s.h / 2;
      e.setView({ k: nk, x: w / 2 - (p.x + s.w / 2) * nk, y: h / 2 - cy * nk }, true);
    },
    [positions, sizeOf],
  );

  useImperativeHandle(ref, () => ({ focus, fit: () => fit(true) }), [focus, fit]);

  // Dragging cards by their handle ([data-drag]); buttons and fields inside keep their clicks.
  const drag = useRef<{ key: string; id: number; start: Point; from: Point; moved: boolean; el: HTMLElement } | null>(null);
  const onPointerDown = (ev: React.PointerEvent) => {
    const e = engine.current;
    if (!e || ev.button !== 0) return;
    const t = ev.target as HTMLElement;
    if (!t.closest("[data-drag]") || t.closest("button, a, input, textarea, select, [data-nodrag]")) return;
    const card = t.closest<HTMLElement>("[data-card]");
    const key = card?.dataset.key;
    const from = key && e.positionOf(key);
    if (!key || !from) return;
    const r = container.current!.getBoundingClientRect();
    drag.current = { key, id: ev.pointerId, start: toWorld(e.view, ev.clientX - r.left, ev.clientY - r.top), from, moved: false, el: card! };
  };
  const onPointerMove = (ev: React.PointerEvent) => {
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
    e.setDrag(d.key, { x: d.from.x + dx, y: d.from.y + dy });
  };
  const onPointerUp = (ev: React.PointerEvent) => {
    const d = drag.current;
    const e = engine.current;
    if (!d || !e || d.id !== ev.pointerId) return;
    drag.current = null;
    if (!d.moved) return;
    const p = e.positionOf(d.key)!;
    e.setDrag(d.key, null);
    setManual((m) => {
      const next = { ...m, [d.key]: { x: Math.round(p.x), y: Math.round(p.y) } };
      savePositions(storageKey, next);
      return next;
    });
    // The drag ends in a click on the card; swallow it.
    const swallow = (c: MouseEvent) => {
      c.stopPropagation();
      c.preventDefault();
    };
    window.addEventListener("click", swallow, { capture: true, once: true });
    setTimeout(() => window.removeEventListener("click", swallow, { capture: true }), 0);
  };

  const tidy = () => {
    setManual({});
    savePositions(storageKey, {});
  };

  // Keyboard, when nothing editable has focus.
  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      const e = engine.current;
      const el = ev.target as HTMLElement | null;
      if (!e || !active || ev.metaKey || ev.ctrlKey || ev.altKey) return;
      if (el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName) || el.closest("[role=dialog]:not([data-surface]), [role=menu], [role=listbox], .xterm"))) return;
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
          fit(true);
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
  }, [fit, onEscape, active]);

  const hasManual = Object.keys(manual).length > 0;

  return (
    <div
      ref={container}
      className="relative h-full w-full touch-none overflow-hidden overscroll-none bg-bg text-fg select-none"
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      aria-label="Agents canvas"
      role="application"
    >
      <canvas ref={canvas} aria-hidden className="pointer-events-none absolute inset-0" />
      <div ref={world} className="absolute top-0 left-0 origin-top-left" style={{ willChange: "transform" }}>
        {[...graph.nodes.values()].map((n) => (
          <CardSlot key={n.key} node={n} engine={engine}>
            {render(n)}
          </CardSlot>
        ))}
      </div>
      {overlay}
      <div className="absolute right-4 bottom-4 flex items-center gap-0.5 rounded-lg border border-line bg-panel/90 p-0.5 text-muted backdrop-blur">
        <ToolButton label="Zoom out (−)" onClick={() => engine.current?.zoomBy(0.8)}>
          <Minus className="size-3.5" />
        </ToolButton>
        <button
          onClick={() => engine.current?.zoomBy(1 / (engine.current?.view.k ?? 1))}
          className="w-11 rounded-md py-1 text-center font-mono text-[11px] tabular-nums hover:bg-panel-2 hover:text-fg"
          title="Zoom to 100% (0)"
        >
          {Math.round(zoom * 100)}%
        </button>
        <ToolButton label="Zoom in (+)" onClick={() => engine.current?.zoomBy(1.25)}>
          <Plus className="size-3.5" />
        </ToolButton>
        <ToolButton label="Fit everything (1)" onClick={() => fit(true)}>
          <Maximize className="size-3.5" />
        </ToolButton>
        {hasManual && (
          <ToolButton label="Tidy: forget dragged positions" onClick={tidy}>
            <Shrink className="size-3.5" />
          </ToolButton>
        )}
      </div>
    </div>
  );
});

function ToolButton({ label, onClick, children }: { label: string; onClick: () => void; children: ReactNode }) {
  return (
    <button onClick={onClick} title={label} aria-label={label} className="grid size-7 place-items-center rounded-md hover:bg-panel-2 hover:text-fg">
      {children}
    </button>
  );
}

/** A card's slot in the world layer; the engine positions it. */
function CardSlot({ node, engine, children }: { node: GraphNode; engine: React.RefObject<CanvasEngine | null>; children: ReactNode }) {
  const el = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const e = engine.current;
    const d = el.current;
    if (!e || !d) return;
    e.register(node.key, d);
    return () => e.unregister(node.key, d);
  }, [engine, node.key]);
  return (
    <div ref={el} data-card data-key={node.key} data-kind={node.kind} className="absolute top-0 left-0" style={{ transform: "translate3d(-99999px,0,0)" }}>
      <div className="rise">{children}</div>
    </div>
  );
}
