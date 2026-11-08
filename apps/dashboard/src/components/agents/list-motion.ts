/**
 * Rows that come, go and move in a list (a conversation's queue, the threads sidebar): a row
 * arriving folds open (height from 0 and a fade, 200 ms), one leaving folds shut (160 ms) before it
 * unmounts, inert meanwhile, and rows that change places slide to their new place (200 ms) instead
 * of jumping. Plain WAAPI on the rows (motion's components keep `onDragStart` for their own gesture,
 * which the queue's drag and drop needs); presence comes from motion's `AnimatePresence`. Anything
 * interrupted turns around from where it is. Reduced motion: at once.
 */

import { useReducedMotion } from "@roll/brand";
import { PresenceContext, usePresence } from "motion/react";
import { useContext, useLayoutEffect, useRef, type RefObject } from "react";
import { EASE_LEAVE, EASE_OUT_SOFT } from "@/lib/agents-motion";

const IN = { duration: 200, easing: `cubic-bezier(${EASE_OUT_SOFT.join(",")})` };
const OUT = { duration: 160, easing: `cubic-bezier(${EASE_LEAVE.join(",")})` };

/**
 * For a row under `AnimatePresence`: put `ref` on it. A row there from the first render (or under
 * `initial={false}` on the list's first render) just shows; one added later folds open (unless
 * `arrive` is false: a list filling in what it already had); removed, it folds shut and then
 * unmounts. `leaving` while it goes (make it inert).
 */
export function useFold<T extends HTMLElement>(arrive = true): { ref: RefObject<T | null>; leaving: boolean } {
  const ref = useRef<T | null>(null);
  const [present, safeToRemove] = usePresence();
  const presence = useContext(PresenceContext);
  const reduced = useReducedMotion();
  // Whether it arrives animated: under a presence that plays entrances.
  const enters = useRef(arrive && presence !== null && presence.initial !== false);
  const ran = useRef(false);
  const remove = useRef(safeToRemove);
  remove.current = safeToRemove;
  useLayoutEffect(() => {
    const el = ref.current;
    const first = !ran.current;
    ran.current = true;
    if (!el) return void (!present && remove.current?.());
    if (first && present && !enters.current) return;
    if (reduced || typeof el.animate !== "function") return void (!present && remove.current?.());
    // From where it is (mid-fold, its inline height), else nothing (arriving).
    const from = first ? 0 : el.getBoundingClientRect().height;
    const fromOpacity = first ? 0 : Number(getComputedStyle(el).opacity);
    el.style.height = "";
    const natural = el.getBoundingClientRect().height;
    const to = present ? natural : 0;
    el.style.overflow = "clip";
    const run = el.animate([{ height: `${from}px`, opacity: fromOpacity }, { height: `${to}px`, opacity: present ? 1 : 0 }], { ...(present ? IN : OUT), fill: "forwards" });
    let done = false;
    run.onfinish = () => {
      done = true;
      if (!present) return remove.current?.();
      run.cancel();
      el.style.height = el.style.opacity = el.style.overflow = "";
    };
    return () => {
      if (done) return;
      // Turning around: keep where it got to.
      try {
        run.commitStyles();
      } catch {
        // Not rendered: nothing to keep.
      }
      run.cancel();
    };
  }, [present, reduced]);
  return { ref, leaving: !present };
}

/**
 * Rows (`[data-flip="<key>"]`, children of `list`) that change places slide from where they were
 * to where they are. Only a change of order moves them: a row folding shut beside them already
 * moves them smoothly.
 */
export function useFlip(list: RefObject<HTMLElement | null>) {
  const reduced = useReducedMotion();
  const last = useRef<{ order: string[]; tops: Map<string, number> } | null>(null);
  useLayoutEffect(() => {
    const el = list.current;
    if (!el) return;
    // Out of the document nothing has a place: start again once it's back.
    if (!el.isConnected) return void (last.current = null);
    const items = [...el.querySelectorAll<HTMLElement>(":scope > [data-flip]")];
    const order = items.map((i) => i.dataset.flip!);
    const tops = new Map(items.map((i) => [i.dataset.flip!, i.offsetTop]));
    const prev = last.current;
    last.current = { order, tops };
    if (!prev || reduced) return;
    // The same order among the rows in both (rows only came or went): nothing moved.
    const kept = order.filter((k) => prev.tops.has(k));
    const before = prev.order.filter((k) => tops.has(k));
    if (kept.every((k, i) => k === before[i])) return;
    for (const item of items) {
      const was = prev.tops.get(item.dataset.flip!);
      const now = tops.get(item.dataset.flip!)!;
      if (was === undefined || was === now || typeof item.animate !== "function") continue;
      item.animate([{ transform: `translateY(${was - now}px)` }, { transform: "none" }], IN);
    }
  });
}
