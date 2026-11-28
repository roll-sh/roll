import { resolveColor } from "@roll/brand";
import { useEffect, useState, type RefObject } from "react";

/** Whether `el` is on screen (true where there's no IntersectionObserver), so motion pauses offscreen. */
export function useInView(el: RefObject<Element | null>, rootMargin = "0px") {
  const [inView, setInView] = useState(false);
  useEffect(() => {
    if (!el.current) return;
    if (typeof IntersectionObserver === "undefined") return setInView(true);
    const io = new IntersectionObserver(([entry]) => setInView(entry.isIntersecting), { rootMargin });
    io.observe(el.current);
    return () => io.disconnect();
  }, [el, rootMargin]);
  return inView;
}

/** Once `el` has been on screen it stays "seen": for draw-in effects that play once. */
export function useSeen(el: RefObject<Element | null>) {
  const inView = useInView(el, "0px 0px -15% 0px");
  const [seen, setSeen] = useState(false);
  useEffect(() => {
    if (inView) setSeen(true);
  }, [inView]);
  return seen;
}

export type Rgb = [number, number, number];

/** The canvas inks an element paints with: its text colour, the accent and the danger red. */
export function inksOf(el: HTMLElement): { ink: Rgb; accent: Rgb; danger: Rgb } {
  const s = getComputedStyle(el);
  return {
    ink: resolveColor(s.color) as Rgb,
    accent: resolveColor(s.getPropertyValue("--accent").trim() || "currentColor") as Rgb,
    danger: resolveColor(s.getPropertyValue("--danger").trim() || "currentColor") as Rgb,
  };
}

/** Calls `fn` when the colour scheme flips, so canvases re-resolve their cached inks. */
export function onSchemeChange(fn: () => void) {
  const mq = window.matchMedia("(prefers-color-scheme: light)");
  mq.addEventListener("change", fn);
  return () => mq.removeEventListener("change", fn);
}

export const rgba = ([r, g, b]: Rgb, a: number) => `rgba(${r},${g},${b},${a})`;
