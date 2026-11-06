"use client";

/**
 * The canvas from above (docs/code/agents.md, "Canvas"): every card as a small block in its place,
 * what's working or waiting in the accent, and the part on screen outlined. It shows only while the
 * canvas is bigger than the screen; a press or a drag on it moves the view there. Figma's and
 * Miro's minimaps; one SVG, its rects redrawn when the layout changes and its frame moved by the
 * engine's view callback (no React render per frame).
 */

import { useCallback, useEffect, useImperativeHandle, useMemo, useRef, type Ref } from "react";
import type { GraphNode } from "@/lib/agents-graph";
import { bounds, type Point, type Size, type View } from "@/lib/agents-layout";
import type { CanvasEngine } from "./canvas-engine";

const W = 168;
const H = 112;

export interface MinimapHandle {
  /** The view moved: move the frame (and show or hide the map). */
  view: (v: View) => void;
}

export function Minimap({
  ref,
  positions,
  sizeOf,
  nodes,
  engine,
}: {
  ref: Ref<MinimapHandle>;
  positions: ReadonlyMap<string, Point>;
  sizeOf: (key: string) => Size;
  nodes: ReadonlyMap<string, GraphNode>;
  engine: React.RefObject<CanvasEngine | null>;
}) {
  const root = useRef<HTMLDivElement>(null);
  // The frame's four edges, 1px boxes stretched by their transforms: moving them is the compositor's
  // alone (attributes on an SVG rect repainted the page under every frame of a zoom).
  const edges = useRef<(HTMLSpanElement | null)[]>([]);
  const world = useMemo(() => bounds(positions, sizeOf), [positions, sizeOf]);
  // World → map: the whole canvas fits, centred, with a little room.
  const map = useMemo(() => {
    if (!world) return null;
    const s = Math.min((W - 8) / world.w, (H - 8) / world.h);
    return { s, x: (W - world.w * s) / 2 - world.x * s, y: (H - world.h * s) / 2 - world.y * s };
  }, [world]);
  const last = useRef<View | null>(null);
  const draw = useCallback(
    (v: View) => {
      last.current = v;
      const e = engine.current;
      const el = root.current;
      const [top, right, bottom, left] = edges.current;
      if (!e || !el || !top || !right || !bottom || !left || !world || !map) return;
      const { w, h } = e.viewport;
      // Only when there's more canvas than screen.
      const bigger = world.w * v.k > w + 1 || world.h * v.k > h + 1;
      if (el.hidden === bigger) el.hidden = !bigger;
      if (!bigger) return;
      const x0 = (-v.x / v.k) * map.s + map.x;
      const y0 = (-v.y / v.k) * map.s + map.y;
      const x = Math.max(0, x0);
      const y = Math.max(0, y0);
      const fw = Math.max(4, Math.min(W, x0 + (w / v.k) * map.s) - x);
      const fh = Math.max(4, Math.min(H, y0 + (h / v.k) * map.s) - y);
      top.style.transform = `translate(${x}px, ${y}px) scale(${fw}, 1)`;
      bottom.style.transform = `translate(${x}px, ${y + fh - 1}px) scale(${fw}, 1)`;
      left.style.transform = `translate(${x}px, ${y}px) scale(1, ${fh})`;
      right.style.transform = `translate(${x + fw - 1}px, ${y}px) scale(1, ${fh})`;
    },
    [engine, world, map],
  );
  useImperativeHandle(ref, () => ({ view: draw }), [draw]);
  // The layout moved: the frame against the new map.
  useEffect(() => {
    const v = last.current ?? engine.current?.view;
    if (v) draw(v);
  }, [draw, engine]);

  // Press or drag: the view centred there, at the same zoom.
  const go = (ev: React.PointerEvent) => {
    const e = engine.current;
    if (!e || !map) return;
    const r = root.current!.getBoundingClientRect();
    const wx = (ev.clientX - r.left - map.x) / map.s;
    const wy = (ev.clientY - r.top - map.y) / map.s;
    const { k } = e.view;
    e.setView({ k, x: e.viewport.w / 2 - wx * k, y: e.viewport.h / 2 - wy * k }, false);
  };
  if (!map) return null;
  return (
    <div
      ref={root}
      hidden
      aria-hidden
      data-minimap
      className="chrome-in absolute right-3 bottom-13 overflow-hidden rounded-[10px] border border-line-2 shadow-pop inset-shadow-highlight glass max-sm:hidden"
      style={{ width: W, height: H }}
      onPointerDown={(ev) => {
        ev.stopPropagation();
        (ev.currentTarget as HTMLElement).setPointerCapture(ev.pointerId);
        go(ev);
      }}
      onPointerMove={(ev) => ev.buttons === 1 && go(ev)}
    >
      <svg width={W} height={H} className="block cursor-pointer">
        {[...positions].map(([k, p]) => {
          const n = nodes.get(k);
          if (!n) return null;
          const s = sizeOf(k);
          const tone = n.kind === "request" ? "var(--accent)" : n.busy ? "color-mix(in oklab, var(--accent) 60%, transparent)" : n.kind === "machine" || n.kind === "project" ? "var(--muted)" : "var(--line-3, var(--subtle))";
          return <rect key={k} x={p.x * map.s + map.x} y={p.y * map.s + map.y} width={Math.max(1.5, s.w * map.s)} height={Math.max(1.5, s.h * map.s)} rx={1} fill={tone} opacity={n.kind === "group" ? 0.5 : 0.9} />;
        })}
      </svg>
      {[0, 1, 2, 3].map((i) => (
        <span
          key={i}
          ref={(el) => {
            edges.current[i] = el;
          }}
          aria-hidden
          className="pointer-events-none absolute top-0 left-0 size-px origin-top-left bg-fg/55 will-change-transform"
        />
      ))}
    </div>
  );
}
