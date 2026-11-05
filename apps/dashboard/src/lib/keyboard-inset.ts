/**
 * The on-screen keyboard, from the visual viewport. iOS Safari doesn't shrink the layout viewport
 * (or `dvh`) when its keyboard opens: it covers what's fixed at the bottom. What it covers is the
 * window's height less the visible part and how far that part is scrolled down. A pinch zoom also
 * shrinks the visible part: that isn't a keyboard, so a zoomed page has none.
 */

import { useSyncExternalStore } from "react";

export interface Keyboard {
  /** How much of the bottom of the window it covers, in CSS px (0: none). */
  inset: number;
  /** How tall the visible part of the window is. */
  visible: number;
}

const NONE: Keyboard = { inset: 0, visible: 0 };

/** What the keyboard covers, from the window's height and the visual viewport. */
export function keyboardOf(innerHeight: number, vv: { height: number; offsetTop: number; scale: number } | null | undefined): Keyboard {
  if (!vv || Math.abs(vv.scale - 1) > 0.01) return { inset: 0, visible: innerHeight };
  const inset = Math.round(innerHeight - vv.height - vv.offsetTop);
  // A few px are toolbars settling, not a keyboard.
  return { inset: inset > 40 ? inset : 0, visible: Math.round(vv.height) };
}

let last: Keyboard = NONE;
function read(): Keyboard {
  const next = keyboardOf(window.innerHeight, window.visualViewport);
  if (next.inset !== last.inset || next.visible !== last.visible) last = next;
  return last;
}

function subscribe(fn: () => void): () => void {
  const vv = window.visualViewport;
  if (!vv) return () => {};
  vv.addEventListener("resize", fn);
  vv.addEventListener("scroll", fn);
  return () => {
    vv.removeEventListener("resize", fn);
    vv.removeEventListener("scroll", fn);
  };
}

/** The on-screen keyboard, live. */
export function useKeyboard(): Keyboard {
  return useSyncExternalStore(subscribe, read, () => NONE);
}
