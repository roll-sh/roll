"use client";

import { useEffect, useRef } from "react";

/**
 * Escape peels a conversation from the inside out: what opened last inside it (the diff or a
 * terminal beside it, a confirm, a form) closes first, and only then the conversation (the
 * surface's Escape in `agents-page.tsx` asks here before it does anything). Layers register while
 * they're open; the newest is on top.
 */
const layers: (() => void)[] = [];

/** While `open`, the next Escape runs `close` (the innermost layer first). */
export function useEscapeLayer(open: boolean, close: () => void) {
  const latest = useRef(close);
  useEffect(() => {
    latest.current = close;
  });
  useEffect(() => {
    if (!open) return;
    const layer = () => latest.current();
    layers.push(layer);
    return () => {
      const i = layers.lastIndexOf(layer);
      if (i >= 0) layers.splice(i, 1);
    };
  }, [open]);
}

/** Closes the innermost open layer; false when none is open. */
export function closeTopLayer(): boolean {
  const top = layers.at(-1);
  if (!top) return false;
  top();
  return true;
}
