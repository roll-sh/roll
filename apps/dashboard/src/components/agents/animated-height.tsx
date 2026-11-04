/**
 * Height that animates when what's inside grows or shrinks (T3 Code's `AnimatedHeight.tsx`, MIT):
 * the wrapper gets the content's measured height in px and transitions it (200 ms ease-out),
 * clipping only while it changes. `Collapse` is the open/closed form every disclosure in the
 * conversation uses. Reduced motion: no transition.
 */

import { useReducedMotion } from "@roll/brand";
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";

const FALLBACK_MS = 250;

/**
 * `closed` shrinks it to nothing while its content stays (so a disclosure folds what it showed, not
 * an empty box); `onClosed` says when it got there.
 */
export function AnimatedHeight({ children, className = "", closed = false, onClosed }: { children: ReactNode; className?: string; closed?: boolean; onClosed?: () => void }) {
  const content = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<{ height: number | null; clipping: boolean }>({ height: null, clipping: false });
  const done = useRef(onClosed);
  done.current = onClosed;

  // The transition may never report its end (nothing changed, a hidden tab, reduced motion): stop
  // clipping anyway.
  useEffect(() => {
    if (!state.clipping) return;
    const t = window.setTimeout(() => {
      setState((s) => (s.clipping ? { ...s, clipping: false } : s));
      if (closed) done.current?.();
    }, FALLBACK_MS);
    return () => window.clearTimeout(t);
  }, [state.height, state.clipping, closed]);

  const measure = useCallback(() => {
    const el = content.current;
    // Out of the document (a list kept while its sheet is closed) it measures nothing: not a height.
    if (!el?.isConnected) return;
    const h = closed ? 0 : Math.ceil(el.scrollHeight || el.getBoundingClientRect().height);
    setState((s) => (s.height === h ? s : { height: h, clipping: s.height !== null }));
  }, [closed]);
  // Every render, before paint: what React changed is measured before it shows (measured only by
  // the observer, new content overflowed the old height for a frame, then snapped to clipped).
  useLayoutEffect(measure);
  // Already nothing tall (nothing to fold): shut at once.
  useLayoutEffect(() => {
    if (closed && state.height === 0 && !state.clipping) done.current?.();
  }, [closed, state.height, state.clipping]);
  useLayoutEffect(() => {
    if (typeof ResizeObserver === "undefined" || !content.current) return;
    const ro = new ResizeObserver(measure);
    ro.observe(content.current);
    return () => ro.disconnect();
  }, [measure]);

  return (
    <div
      data-slot="animated-height"
      data-closed={closed || undefined}
      className={`transition-[height] duration-200 ease-out motion-reduce:transition-none [overflow-clip-margin:2px] ${className}`}
      style={state.height === null ? undefined : { height: state.height, overflow: state.clipping || state.height === 0 ? "clip" : "visible" }}
      onTransitionEnd={(e) => {
        if (e.target !== e.currentTarget || e.propertyName !== "height") return;
        setState((s) => (s.clipping ? { ...s, clipping: false } : s));
        if (closed) done.current?.();
      }}
    >
      <div ref={content} inert={closed || undefined}>
        {children}
      </div>
    </div>
  );
}

/**
 * What's under a disclosure: its height animating open and shut. It keeps what it showed while it
 * folds, unmounts once shut, and opening again mid-fold turns the same height around.
 */
export function Collapse({ open, children, className }: { open: boolean; children: ReactNode; className?: string }) {
  const reduced = useReducedMotion();
  const [mounted, setMounted] = useState(open);
  if (open && !mounted) setMounted(true);
  if (!open && mounted && reduced) setMounted(false);
  const last = useRef(children);
  if (open) last.current = children;
  return (
    <AnimatedHeight className={className} closed={!open} onClosed={() => setMounted(false)}>
      {mounted ? (open ? children : last.current) : null}
    </AnimatedHeight>
  );
}
