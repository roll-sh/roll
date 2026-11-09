"use client";

import { useSyncExternalStore } from "react";

const QUERY = "(prefers-reduced-motion: reduce)";

// One query list, made again only if `matchMedia` itself is replaced (tests do): the snapshot is
// read on every render of every component that asks, and `matchMedia` per render showed in profiles.
let cached: { of: typeof window.matchMedia; mq: MediaQueryList } | null = null;
function query(): MediaQueryList {
  if (cached?.of !== window.matchMedia) cached = { of: window.matchMedia, mq: window.matchMedia(QUERY) };
  return cached.mq;
}

function subscribe(onChange: () => void) {
  const mq = query();
  mq.addEventListener("change", onChange);
  return () => mq.removeEventListener("change", onChange);
}

export function useReducedMotion(): boolean {
  return useSyncExternalStore(
    subscribe,
    () => query().matches,
    () => false,
  );
}
