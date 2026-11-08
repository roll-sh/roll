"use client";

/**
 * A small hand-built popup (a field's suggestions, the composer's commands, the filter editor) that
 * comes and goes as Base UI's popups do (`popupMotion`): in from 0.98 and 2px toward its anchor with
 * a fade, 150 ms on `--ease-out-soft`; out the same way over 120 ms, inert meanwhile, keeping what it
 * showed; then it unmounts. Opening again while it leaves turns it around (`usePresenceTracks`).
 * Was `{open && <ul className="pop">}`: in only, gone in a frame. Reduced motion: at once.
 */

import { useReducedMotion } from "@roll/brand";
import { AnimatePresence, motion, type Transition } from "motion/react";
import type { HTMLAttributes, ReactNode, Ref } from "react";
import { POPOVER_IN as IN, POPOVER_OUT as OUT } from "@/lib/agents-motion";
import { useEnteringValue, usePresenceTracks } from "./presence";


type PopProps = Omit<HTMLAttributes<HTMLElement>, "onDrag" | "onDragStart" | "onDragEnd" | "onAnimationStart" | "style"> & {
  open: boolean;
  /** `ul` for a listbox. */
  as?: "div" | "ul";
  /** Which way it opened: from below its anchor (`down`, the default) or above (`up`). */
  from?: "down" | "up";
  ref?: Ref<HTMLElement>;
  children: ReactNode;
};

export function Pop({ open, ...rest }: PopProps) {
  return <AnimatePresence>{open && <PopBody key="pop" {...rest} />}</AnimatePresence>;
}

function PopBody({ as = "div", from = "down", ref, className, children, ...rest }: Omit<PopProps, "open">) {
  const reduced = useReducedMotion();
  const none: Transition = { duration: 0 };
  const shift = from === "down" ? -2 : 2;
  const opacity = useEnteringValue(0, 1);
  const scale = useEnteringValue(0.98, 1);
  const y = useEnteringValue(shift, 0);
  const present = usePresenceTracks(
    [
      { value: opacity, shown: 1, hidden: 0, enter: reduced ? none : IN, leave: reduced ? none : OUT },
      { value: scale, shown: 1, hidden: 0.98, enter: reduced ? none : IN, leave: reduced ? none : OUT },
      { value: y, shown: 0, hidden: shift, enter: reduced ? none : IN, leave: reduced ? none : OUT },
    ],
    `${from}|${reduced}`,
  );
  const Tag = as === "ul" ? motion.ul : motion.div;
  return (
    <Tag
      ref={ref as Ref<never>}
      {...rest}
      data-state={present ? "open" : "closing"}
      inert={!present}
      aria-hidden={!present || undefined}
      className={className}
      style={{ opacity, scale, y, transformOrigin: from === "down" ? "top" : "bottom" }}
    >
      {children}
    </Tag>
  );
}
