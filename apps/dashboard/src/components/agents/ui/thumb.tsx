"use client";

/**
 * The pressed look of a segmented control or a tab bar as one thing that slides from the old
 * choice to the new (Linear's and Vercel's tabs; T3's segmented controls only swap colours). Put it
 * first inside the items' container (positioned); it sits under them, placed on the selected one
 * (`selector`), and follows it with a CSS transition (200 ms on `--ease-out-soft`, interruptible:
 * a quick second pick turns it from where it is). It's placed without a transition the first time,
 * and when nothing is selected it fades where it was. Reduced motion: it jumps.
 */

import { useLayoutEffect, useRef, useState } from "react";
import { cn } from "./cn";

type Box = { x: number; y: number; w: number; h: number };

export function SlidingThumb({ selector, className }: { selector: string; className?: string }) {
  // Its own element: the container is its parent (a parent's ref isn't attached yet when a
  // child's layout effects run, so measuring through one missed the first frame for good).
  const self = useRef<HTMLSpanElement>(null);
  const [box, setBox] = useState<Box | null>(null);
  const [shown, setShown] = useState(false);
  // Placed once without moving, then it glides.
  const [placed, setPlaced] = useState(false);
  const last = useRef<Box | null>(null);
  const measure = () => {
    const c = self.current?.parentElement;
    const el = c?.querySelector<HTMLElement>(selector);
    if (!c || !el) {
      setShown(false);
      return;
    }
    const cr = c.getBoundingClientRect();
    const r = el.getBoundingClientRect();
    const next = { x: r.left - cr.left - c.clientLeft + c.scrollLeft, y: r.top - cr.top - c.clientTop + c.scrollTop, w: r.width, h: r.height };
    const prev = last.current;
    setShown(true);
    if (prev && prev.x === next.x && prev.y === next.y && prev.w === next.w && prev.h === next.h) return;
    setBox((last.current = next));
  };
  // Placed before the first paint; later choices arrive through the observers below. (Measuring on
  // every render of the owner forced a layout of the whole page into each commit: with 200 cards on
  // the canvas, ~80 ms of every filter key.)
  useLayoutEffect(() => {
    const c = self.current?.parentElement;
    if (!c) return;
    measure();
    // Items growing (a count appearing, fonts loading) move it too; so does a choice made outside
    // a render of its owner.
    const ro = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(() => measure());
    ro?.observe(c);
    for (const child of c.children) ro?.observe(child);
    const mo = new MutationObserver(() => measure());
    mo.observe(c, { subtree: true, attributes: true, attributeFilter: ["aria-selected", "aria-pressed", "data-pressed", "data-selected"], childList: true });
    return () => {
      ro?.disconnect();
      mo.disconnect();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selector]);
  useLayoutEffect(() => {
    if (!box || placed) return;
    const id = requestAnimationFrame(() => setPlaced(true));
    return () => cancelAnimationFrame(id);
  }, [box, placed]);
  return (
    <span
      ref={self}
      aria-hidden
      data-thumb
      className={cn(
        "pointer-events-none absolute top-0 left-0 duration-200 ease-(--ease-out-soft) motion-reduce:transition-none",
        placed ? "transition-[transform,width,height,opacity]" : "transition-none",
        className,
      )}
      style={box ? { transform: `translate(${box.x}px, ${box.y}px)`, width: box.w, height: box.h, opacity: shown ? 1 : 0 } : { opacity: 0 }}
    />
  );
}
