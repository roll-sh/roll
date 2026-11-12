import { useReducedMotion } from "@roll/brand";
import { motion, type Transition } from "motion/react";
import { useEffect, useState, type ReactNode } from "react";
import { DRAWER, FADE_IN, FADE_OUT, LEAVE } from "@/lib/agents-motion";
import { useEnteringValue, usePresenceTracks } from "./presence";

/**
 * A sheet docked on the left edge (the threads sidebar), under `AnimatePresence`. Like the right
 * dock: an in-flow gap grows to `width` while the content, pinned to the gap's right edge, slides in
 * from the window's edge with it, so what's beside it moves over in step; leaving is the same
 * backwards, inert meanwhile, then it unmounts. On a phone it covers the page and slides over it.
 * `instant` skips the motion (the first frame, when the sheet opens because it was left open).
 */
export function EdgeSheet({ label, width, phone, instant = false, className = "", children }: { label: string; width: number; phone: boolean; instant?: boolean; className?: string; children: ReactNode }) {
  const reduced = useReducedMotion() || instant;
  const none: Transition = { duration: 0 };
  const gap = useEnteringValue(0, width);
  const fade = useEnteringValue(0, 1);
  const slide = useEnteringValue("-100%", "0%");
  const present = usePresenceTracks(
    phone
      ? [{ value: slide, shown: "0%", hidden: "-100%", enter: reduced ? none : DRAWER, leave: reduced ? none : LEAVE }]
      : [
          { value: gap, shown: width, hidden: 0, enter: reduced ? none : DRAWER, leave: reduced ? none : LEAVE },
          { value: fade, shown: 1, hidden: 0, enter: reduced ? none : FADE_IN, leave: reduced ? none : FADE_OUT },
        ],
    `${phone}|${width}|${reduced}`,
  );
  if (phone) {
    // Over a conversation or settings open in the right dock (z-30): it opened behind them.
    return (
      <motion.aside aria-label={label} data-state={present ? "open" : "closing"} inert={!present} className={`absolute inset-0 z-40 flex min-h-0 flex-col ${className}`} style={{ x: slide }}>
        {children}
      </motion.aside>
    );
  }
  return (
    <motion.aside aria-label={label} data-state={present ? "open" : "closing"} inert={!present} className="relative shrink-0 overflow-x-clip" style={{ width: gap }}>
      <motion.div className={`absolute inset-y-0 right-0 flex min-h-0 flex-col ${className}`} style={{ width, opacity: fade }}>
        {children}
      </motion.div>
    </motion.aside>
  );
}

/** False for the first frame after mount: a sheet that opens then (restored) appears without moving. */
export function useAfterFirstFrame(): boolean {
  const [after, setAfter] = useState(false);
  useEffect(() => {
    const id = requestAnimationFrame(() => setAfter(true));
    return () => cancelAnimationFrame(id);
  }, []);
  return after;
}
