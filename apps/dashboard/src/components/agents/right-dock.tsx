"use client";

import { useReducedMotion } from "@roll/brand";
import { AnimatePresence, motion, type Transition } from "motion/react";
import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { DRAWER, FADE_IN, FADE_OUT, LEAVE, PANE_SWITCH } from "@/lib/agents-motion";
import { useEnteringValue, usePresenceTracks } from "./presence";

/** What's open beside the canvas. A new `key` crossfades in the same frame; `null` closes it. */
export type DockPane = { key: string; label: string; node: ReactNode };

const PANEL_WIDTH = "roll:agents:panel-width";
/** The canvas keeps at least this much room beside the panel. */
const CANVAS_MIN = 320;
const PANEL_MIN = 380;

/**
 * The one right sheet beside the canvas (a conversation, settings, the automations sheet). One
 * frame for all of them: opening grows an in-flow gap while the sheet, pinned to the gap's left
 * edge, slides in from the window's edge with it (T3's sidebar gap and container, ui/sidebar.tsx),
 * so the canvas narrows in step; closing is the same backwards and only then unmounts; switching
 * what's open crossfades inside the frame without moving it. `wide` grows it over the whole
 * working area (the maximized builder). On a phone it covers the canvas and slides over it.
 *
 * Every animation starts from where the last one was (open then close quickly reverses), a closing
 * sheet is inert (clicks reach the canvas behind it at once), and reduced motion jumps to the end.
 */
export function RightDock({ pane, phone, wide = false }: { pane: DockPane | null; phone: boolean; wide?: boolean }) {
  return <AnimatePresence>{pane && <Dock key="dock" pane={pane} phone={phone} wide={wide} />}</AnimatePresence>;
}

function Dock({ pane, phone, wide }: { pane: DockPane; phone: boolean; wide: boolean }) {
  const reduced = useReducedMotion();
  const ref = useRef<HTMLElement>(null);
  const area = useAreaWidth(ref);
  const [width, setWidth] = useState(() => {
    const saved = typeof window === "undefined" ? NaN : Number(localStorage.getItem(PANEL_WIDTH));
    return Number.isFinite(saved) && saved > 0 ? saved : 640;
  });
  const [dragging, setDragging] = useState(false);
  // Before the area is measured (first frame, jsdom) the saved width stands.
  const normal = area > 0 ? Math.max(Math.min(width, area - CANVAS_MIN), Math.min(PANEL_MIN, area)) : width;
  const target = wide && area > 0 ? area : normal;
  const instant: Transition = { duration: 0 };
  const gap = useEnteringValue(0, target);
  const fade = useEnteringValue(0, 1);
  const slide = useEnteringValue("100%", "0%");
  const present = usePresenceTracks(
    phone
      ? [{ value: slide, shown: "0%", hidden: "100%", enter: reduced ? instant : DRAWER, leave: reduced ? instant : LEAVE }]
      : [
          { value: gap, shown: target, hidden: 0, enter: reduced || dragging ? instant : DRAWER, leave: reduced ? instant : LEAVE },
          { value: fade, shown: 1, hidden: 0, enter: reduced ? instant : FADE_IN, leave: reduced ? instant : FADE_OUT },
        ],
    `${phone}|${target}|${reduced}|${dragging}`,
  );

  const onPointerDown = (e: React.PointerEvent) => {
    e.preventDefault();
    const x0 = e.clientX;
    const w0 = normal;
    const el = e.currentTarget as HTMLElement;
    el.setPointerCapture(e.pointerId);
    setDragging(true);
    let last = w0;
    const clamp = (w: number) => Math.round(Math.max(PANEL_MIN, Math.min(w, (area || window.innerWidth) - CANVAS_MIN)));
    const move = (m: PointerEvent) => setWidth((last = clamp(w0 + x0 - m.clientX)));
    const up = () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      el.removeEventListener("pointercancel", up);
      setDragging(false);
      try {
        localStorage.setItem(PANEL_WIDTH, String(last));
      } catch {
        // Not remembered.
      }
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
    el.addEventListener("pointercancel", up);
  };

  const panes = <Panes pane={pane} reduced={reduced} />;
  if (phone) {
    return (
      <motion.aside
        ref={ref}
        aria-label={pane.label}
        data-panel
        data-state={present ? "open" : "closing"}
        inert={!present}
        className="absolute inset-0 z-30 flex min-h-0 flex-col bg-sheet"
        style={{ x: slide }}
      >
        {panes}
      </motion.aside>
    );
  }
  return (
    // The gap in the row: the canvas beside it is flex-1, so it narrows and widens with it.
    <motion.aside
      ref={ref}
      aria-label={pane.label}
      data-panel
      data-state={present ? "open" : "closing"}
      inert={!present}
      className="relative shrink-0"
      style={{ width: gap }}
    >
      {/* At least its normal width while the gap is narrower, so it slides rather than squeezes. */}
      <motion.div className={`absolute inset-y-0 left-0 right-0 flex flex-col pr-2 pb-2 ${wide ? "pl-2" : ""}`} style={{ minWidth: normal, opacity: fade }}>
        {!wide && (
          <div
            role="separator"
            aria-orientation="vertical"
            aria-label="Resize the panel"
            onPointerDown={onPointerDown}
            className="group/resize absolute inset-y-0 -left-2 z-10 flex w-2 cursor-col-resize justify-center"
          >
            <span className="my-auto h-10 w-[3px] rounded-full bg-line-3 opacity-0 transition-[opacity,height,background-color] duration-150 group-hover/resize:h-16 group-hover/resize:bg-accent/70 group-hover/resize:opacity-100" />
          </div>
        )}
        <div className="relative flex min-h-0 flex-1 flex-col overflow-hidden rounded-xl border border-line-2 bg-sheet shadow-sheet inset-shadow-highlight">{panes}</div>
      </motion.div>
    </motion.aside>
  );
}

/** What's in the frame: a new pane fades in over the old one fading out, both filling the frame. */
function Panes({ pane, reduced }: { pane: DockPane; reduced: boolean }) {
  return (
    <AnimatePresence initial={false}>
      <Pane key={pane.key} reduced={reduced}>
        {pane.node}
      </Pane>
    </AnimatePresence>
  );
}

function Pane({ reduced, children }: { reduced: boolean; children: ReactNode }) {
  const t: Transition = reduced ? { duration: 0 } : PANE_SWITCH;
  const opacity = useEnteringValue(0, 1);
  const x = useEnteringValue(8, 0);
  const present = usePresenceTracks(
    [
      { value: opacity, shown: 1, hidden: 0, enter: t, leave: t },
      { value: x, shown: 0, hidden: -8, enter: t, leave: t },
    ],
    String(reduced),
  );
  return (
    <motion.div
      className="absolute inset-0 flex min-h-0 flex-col"
      data-pane={present ? "current" : "leaving"}
      inert={!present}
      aria-hidden={!present || undefined}
      style={{ opacity, x }}
    >
      {children}
    </motion.div>
  );
}

/** The width of the working area the dock sits in (its parent), kept current. */
function useAreaWidth(ref: React.RefObject<HTMLElement | null>): number {
  const [w, setW] = useState(0);
  useLayoutEffect(() => {
    const parent = ref.current?.parentElement;
    if (!parent) return;
    const read = () => setW(Math.round(parent.getBoundingClientRect().width));
    read();
    const ro = new ResizeObserver(read);
    ro.observe(parent);
    return () => ro.disconnect();
  }, [ref]);
  return w;
}
