import type { Transition } from "motion/react";

/**
 * The agents surface's motion, in one place (docs/code/agents-design.md, "Motion"). The CSS
 * classes in globals.css use the same curves.
 */

/** T3's `--ease-drawer`: things that slide in from an edge. */
export const EASE_DRAWER = [0.32, 0.72, 0, 1] as const;
/** `--ease-out-soft`: popovers, fades, small moves. */
export const EASE_OUT_SOFT = [0.23, 1, 0.32, 1] as const;
/**
 * Leaving: the standard ease-in-out. An ease-out (the drawer curve, even a quad) covers most of the
 * way in the first 100 ms, so a closing panel read as gone at once (the user: "maybe 100 ms but
 * that's the maximum"); this one is seen moving over its whole time and still settles softly.
 */
export const EASE_LEAVE = [0.4, 0, 0.2, 1] as const;

/** A panel or sheet sliding in from an edge. */
export const DRAWER: Transition = { duration: 0.26, ease: EASE_DRAWER };
/** A panel or sheet leaving: 260 ms, seen over the whole way. */
export const LEAVE: Transition = { duration: 0.26, ease: EASE_LEAVE };
/** A leaving panel fading: it holds for the first 100 ms of the slide, then fades with it. */
export const FADE_OUT: Transition = { duration: 0.16, delay: 0.1, ease: "easeIn" };
/** Fading in as something slides in. */
export const FADE_IN: Transition = { duration: 0.2, ease: "easeOut" };
/** The palette and dialogs: scale 0.98 → 1 with a fade, on the drawer curve. */
export const POP_IN: Transition = { duration: 0.2, ease: EASE_DRAWER };
/** …and back out: a touch quicker, but long enough to be seen going. */
export const POP_OUT: Transition = { duration: 0.17, ease: "easeIn" };
/** One pane replacing another in the same frame. */
export const PANE_SWITCH: Transition = { duration: 0.2, ease: EASE_OUT_SOFT };
