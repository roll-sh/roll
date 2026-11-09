/**
 * Pages that replace each other in one place (the ⌘K palette's views, the automations sheet's
 * stack): the new page slides 16px in from the side it comes from while the old one slides the
 * other way and fades, both over 200 ms; going deeper comes from the right, going back from the
 * left. The old page is taken out of the flow as it goes (motion's `popLayout`), so the new one is
 * where it ends up from the first frame, and it's inert. Each page is driven by `usePresenceTracks`,
 * so a quick back-and-forth turns the same page around. A change that keeps the page (a tab on the
 * same level) doesn't move it. Reduced motion: the new page at once. T3 remounts its palette views
 * with no motion (`CommandPalette.tsx`, keyed by the stack's depth); the slide is roll's.
 */

import { useReducedMotion } from "@roll/brand";
import { AnimatePresence, motion, usePresenceData, type Transition } from "motion/react";
import { useLayoutEffect, useRef, type ReactNode, type Ref } from "react";
import { EASE_EMPHASIZED, EASE_OUT_SOFT, PANE_SWITCH } from "@/lib/agents-motion";
import { useEnteringValue, usePresenceTracks } from "./presence";

/** How far a page travels. */
const SHIFT = 16;
/** The page leaving: a touch quicker than the one arriving. */
const PAGE_OUT: Transition = { duration: 0.15, ease: EASE_OUT_SOFT };

/**
 * `direction`: 1 deeper, -1 back, 0 neither (a fade in place). `as="span"` inside phrasing content
 * (a dialog's title and lead). The parent is positioned: the page leaving is placed against it.
 */
export function SlidePages({ page, direction, className, as, children }: { page: string; direction: -1 | 0 | 1; className?: string; as?: "div" | "span"; children: ReactNode }) {
  return (
    <AnimatePresence mode="popLayout" initial={false} custom={direction}>
      <SlidePage key={page} className={className} as={as}>
        {children}
      </SlidePage>
    </AnimatePresence>
  );
}

function SlidePage({ className, as = "div", children, ref }: { className?: string; as?: "div" | "span"; children: ReactNode; ref?: Ref<HTMLDivElement> }) {
  const reduced = useReducedMotion();
  // The latest direction, for the page leaving too (its own props are frozen).
  const dir = (usePresenceData() as -1 | 0 | 1 | undefined) ?? 0;
  const x = useEnteringValue(SHIFT * dir, 0);
  const opacity = useEnteringValue(0, 1);
  const none: Transition = { duration: 0 };
  const present = usePresenceTracks(
    [
      { value: x, shown: 0, hidden: -SHIFT * dir, enter: reduced ? none : PANE_SWITCH, leave: reduced ? none : PAGE_OUT },
      { value: opacity, shown: 1, hidden: 0, enter: reduced ? none : PANE_SWITCH, leave: reduced ? none : PAGE_OUT },
    ],
    `${dir}|${reduced}`,
  );
  const Page = as === "span" ? motion.span : motion.div;
  return (
    <Page
      ref={ref}
      data-page={present ? "current" : "leaving"}
      data-from={present && dir !== 0 ? (dir > 0 ? "right" : "left") : undefined}
      inert={!present}
      aria-hidden={!present || undefined}
      className={className}
      style={{ x, opacity }}
    >
      {children}
    </Page>
  );
}

/**
 * The height of `el` eases from what it was to what it is whenever `key` changes (a page replaced by
 * a taller or shorter one), over 200 ms, then lets go; between changes it follows its content.
 */
export function useHeightGlide(el: React.RefObject<HTMLElement | null>, key: string) {
  const reduced = useReducedMotion();
  // The height it showed before this commit (kept by the observer), and the glide running.
  const last = useRef<{ key: string; height: number } | null>(null);
  const run = useRef<Animation | null>(null);
  useLayoutEffect(() => {
    const node = el.current;
    if (!node) return;
    const prev = last.current;
    if (prev && prev.key === key) return;
    // Mid-glide, from where it is now; else from what it showed.
    const gliding = run.current && run.current.playState !== "finished" && run.current.playState !== "idle";
    const from = gliding ? node.getBoundingClientRect().height : prev?.height;
    run.current?.cancel();
    run.current = null;
    const to = node.getBoundingClientRect().height;
    last.current = { key, height: to };
    if (from === undefined || reduced || Math.abs(from - to) < 1 || typeof node.animate !== "function") return;
    const a = node.animate({ height: [`${from}px`, `${to}px`] }, { duration: 200, easing: `cubic-bezier(${EASE_EMPHASIZED.join(",")})` });
    run.current = a;
    a.onfinish = () => {
      if (run.current === a) run.current = null;
    };
  }, [el, key, reduced]);
  useLayoutEffect(() => {
    const node = el.current;
    if (!node || typeof ResizeObserver === "undefined") return;
    // Between page changes it follows its content (typing filters the list).
    const ro = new ResizeObserver(() => {
      const gliding = run.current && run.current.playState !== "finished" && run.current.playState !== "idle";
      if (last.current && !gliding) last.current.height = node.getBoundingClientRect().height;
    });
    ro.observe(node);
    return () => {
      ro.disconnect();
      run.current?.cancel();
    };
  }, [el]);
}

/**
 * `el`'s height eases (200 ms, from wherever it is) to its new natural height whenever one of
 * `parts` changes size: a dialog whose content changes (a step replaced by the next, a result
 * appearing) grows or shrinks instead of jumping. `el` is the box whose height follows its parts
 * (the dialog's sheet, capped by its max height); the parts keep their natural size (a scroller's
 * content, not the scroller). While it eases, `el` has `data-gliding` (a scroller hides its bar).
 * Reduced motion: the new height at once.
 */
export function useContentGlide(el: () => HTMLElement | null, parts: readonly React.RefObject<HTMLElement | null>[]) {
  const reduced = useReducedMotion();
  useLayoutEffect(() => {
    const node = el();
    if (!node || typeof ResizeObserver === "undefined") return;
    // offsetHeight: the layout height, not scaled by the sheet's own way in.
    let last = node.offsetHeight;
    let run: Animation | null = null;
    const gliding = () => run !== null && run.playState === "running";
    // The parts only, never `el` itself: easing it inside the callback would resize what's observed
    // in the same frame (WebKit reports "ResizeObserver loop completed with undelivered notifications").
    const ro = new ResizeObserver(() => {
      const from = gliding() ? node.offsetHeight : last;
      run?.cancel();
      run = null;
      const to = node.offsetHeight;
      last = to;
      if (reduced || Math.abs(from - to) < 1 || typeof node.animate !== "function") return;
      const a = node.animate({ height: [`${from}px`, `${to}px`] }, { duration: 200, easing: `cubic-bezier(${EASE_EMPHASIZED.join(",")})` });
      run = a;
      node.setAttribute("data-gliding", "");
      const settle = () => {
        if (run !== a) return;
        run = null;
        node.removeAttribute("data-gliding");
      };
      a.onfinish = settle;
      a.oncancel = settle;
    });
    for (const p of parts) if (p.current) ro.observe(p.current);
    // The window resized it (its max height): where it is now, unless it's easing.
    const resized = () => {
      if (!gliding()) last = node.offsetHeight;
    };
    window.addEventListener("resize", resized);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", resized);
      run?.cancel();
      node.removeAttribute("data-gliding");
    };
    // The parts are refs, read once their elements exist.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [el, reduced]);
}
