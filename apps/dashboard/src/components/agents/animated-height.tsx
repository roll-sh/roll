"use client";

/**
 * Height that animates when what's inside grows or shrinks (T3 Code's `AnimatedHeight.tsx`, MIT):
 * the wrapper gets the content's measured height in px and transitions it (200 ms ease-out),
 * clipping only while it changes. `Collapse` is the open/closed form every disclosure in the
 * conversation uses. Reduced motion: no transition.
 */

import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";

const FALLBACK_MS = 250;

export function AnimatedHeight({ children, className = "" }: { children: ReactNode; className?: string }) {
  const content = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<{ height: number | null; clipping: boolean }>({ height: null, clipping: false });

  // The transition may never report its end (nothing changed, a hidden tab): stop clipping anyway.
  useEffect(() => {
    if (!state.clipping) return;
    const t = window.setTimeout(() => setState((s) => (s.clipping ? { ...s, clipping: false } : s)), FALLBACK_MS);
    return () => window.clearTimeout(t);
  }, [state.height, state.clipping]);

  useLayoutEffect(() => {
    const el = content.current;
    if (!el) return;
    const measure = () => {
      const h = Math.ceil(el.scrollHeight || el.getBoundingClientRect().height);
      setState((s) => (s.height === h ? s : { height: h, clipping: s.height !== null }));
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  return (
    <div
      data-slot="animated-height"
      className={`transition-[height] duration-200 ease-out motion-reduce:transition-none [overflow-clip-margin:2px] ${className}`}
      style={state.height === null ? undefined : { height: state.height, overflow: state.clipping ? "clip" : "visible" }}
      onTransitionEnd={(e) => {
        if (e.target === e.currentTarget && e.propertyName === "height") setState((s) => (s.clipping ? { ...s, clipping: false } : s));
      }}
    >
      <div ref={content}>{children}</div>
    </div>
  );
}

/** What's under a disclosure: mounted while open, its height animating open and shut. */
export function Collapse({ open, children, className }: { open: boolean; children: ReactNode; className?: string }) {
  return <AnimatedHeight className={className}>{open ? children : null}</AnimatedHeight>;
}
