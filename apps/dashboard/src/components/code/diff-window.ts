"use client";

/**
 * Big diffs stay cheap: a file's rows go in blocks of BLOCK (one `<tbody>` each), and only the
 * blocks near the scroll viewport hold their rows; the rest are one row of their last measured
 * height. Syntax highlighting happens per block once it's been near the viewport, one block per
 * idle slot, with shiki loaded on first use.
 */

import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import type { Line } from "@/lib/diff";
import type { Token } from "@/lib/highlight";

export const BLOCK = 64;
/** A row's height in a unified diff (`leading-5`); split rows that wrap get measured. */
const ROW_PX = 20;
/** Blocks this far outside the viewport are already rendered, so scrolling never shows a gap. */
const MARGIN = "1500px 0px";
/** Blocks shown before the first intersection report (and where there's no IntersectionObserver). */
const FIRST = 2;

/** The element that scrolls the diff: the first scrolling ancestor above `from`, or the page. */
function scroller(from: HTMLElement | null): Element | null {
  for (let el = from; el; el = el.parentElement) {
    const y = getComputedStyle(el).overflowY;
    if (y === "auto" || y === "scroll" || y === "overlay") return el;
  }
  return null;
}

export interface RowWindow {
  /** On the table's horizontal scroller; the vertical scroller is found above it. */
  frame: (el: HTMLDivElement | null) => void;
  /** On each block's `<tbody>`. */
  block: (b: number) => (el: HTMLTableSectionElement | null) => void;
  /** Whether block `b` renders its rows. */
  live: (b: number) => boolean;
  /** Whether block `b` has been near the viewport (and may be highlighted). */
  seen: (b: number) => boolean;
  /** A placeholder's height for block `b` of `rows` rows. */
  height: (b: number, rows: number) => number;
}

export function useRowWindow(forced: ReadonlySet<number>): RowWindow {
  const [near, setNear] = useState<ReadonlySet<number> | null>(null);
  const [frameEl, setFrame] = useState<HTMLDivElement | null>(null);
  const heights = useRef(new Map<number, number>()).current;
  const index = useRef(new Map<Element, number>()).current;
  const refs = useRef(new Map<number, (el: HTMLTableSectionElement | null) => void>()).current;
  const io = useRef<IntersectionObserver | null>(null);

  useEffect(() => {
    if (!frameEl || typeof IntersectionObserver === "undefined") return;
    const obs = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          const b = index.get(e.target);
          // Keep a block's real height for when it's a placeholder again.
          if (b != null && !e.isIntersecting && (e.target as HTMLElement).dataset.live) heights.set(b, (e.target as HTMLElement).offsetHeight);
        }
        setNear((prev) => {
          const next = new Set(prev ?? []);
          for (const e of entries) {
            const b = index.get(e.target);
            if (b == null) continue;
            if (e.isIntersecting) next.add(b);
            else next.delete(b);
          }
          return next;
        });
      },
      { root: scroller(frameEl.parentElement), rootMargin: MARGIN },
    );
    io.current = obs;
    for (const el of index.keys()) obs.observe(el);
    return () => {
      obs.disconnect();
      io.current = null;
    };
  }, [frameEl, heights, index]);

  const block = useCallback(
    (b: number) => {
      let ref = refs.get(b);
      if (!ref) {
        let mine: Element | null = null;
        ref = (el) => {
          if (mine) {
            index.delete(mine);
            io.current?.unobserve(mine);
          }
          mine = el;
          if (el) {
            index.set(el, b);
            io.current?.observe(el);
          }
        };
        refs.set(b, ref);
      }
      return ref;
    },
    [refs, index],
  );

  return {
    frame: setFrame,
    block,
    live: (b) => forced.has(b) || (near ? near.has(b) : b < FIRST),
    seen: (b) => near?.has(b) ?? false,
    height: (b, rows) => heights.get(b) ?? rows * ROW_PX,
  };
}

// ── lazy highlighting ──

/** Tokens per parsed line; null when the file's language has no grammar (or the line is too long). */
const tokens = new WeakMap<Line, Token[] | null>();
const MAX_LINE = 2_000;

type Job = { cancelled: boolean; run: () => Promise<void> };
const jobs: Job[] = [];
let pumping = false;

const idle = () =>
  new Promise<void>((r) => (typeof requestIdleCallback === "function" ? requestIdleCallback(() => r(), { timeout: 300 }) : setTimeout(r, 16)));

/** One block per idle slot, newest first (the block just scrolled to beats the ones scrolled past). */
async function pump() {
  if (pumping) return;
  pumping = true;
  try {
    while (jobs.length) {
      await idle();
      const job = jobs.pop()!;
      if (!job.cancelled) await job.run().catch(() => {});
    }
  } finally {
    pumping = false;
  }
}

async function tokenize(path: string, lines: Line[]) {
  const { highlight, languageFor } = await import("@/lib/highlight");
  const lang = languageFor(path);
  const sides: Line[][] = [lines.filter((l) => l.kind === "add" || l.kind === "ctx"), lines.filter((l) => l.kind === "del")];
  for (const side of sides) {
    if (!side.length) continue;
    const plain = !lang || lang === "plaintext" || lang === "txt" || side.some((l) => l.text.length > MAX_LINE);
    const out = plain ? null : await highlight(side.map((l) => l.text).join("\n"), lang);
    side.forEach((l, i) => tokens.set(l, out?.[i] ?? null));
  }
}

/** Highlights `lines` (one block's) once `on`; re-renders the caller when the tokens are in. */
export function useHighlight(path: string, lines: Line[], on: boolean) {
  const [, bump] = useReducer((n: number) => n + 1, 0);
  useEffect(() => {
    if (!on) return;
    const todo = lines.filter((l) => l.kind !== "hunk" && l.kind !== "note" && !tokens.has(l));
    if (!todo.length) return;
    const job: Job = {
      cancelled: false,
      run: async () => {
        await tokenize(path, todo);
        if (!job.cancelled) bump();
      },
    };
    jobs.push(job);
    void pump();
    return () => {
      job.cancelled = true;
    };
  }, [path, lines, on]);
}

/** A line's code: highlighted when its tokens are in, plain text until then. */
export function codeOf(l: Line) {
  return tokens.get(l) ?? null;
}
