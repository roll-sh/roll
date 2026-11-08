"use client";

/**
 * Find in a diff, T3's way (its `CodeViewSearch` in the @pierre/diffs patch, MIT): ⌘F while the
 * diff has focus opens a small bar over it; matches come from the parsed lines (`lib/diff-search`),
 * so rows a big diff hasn't drawn are found; stepping to one opens its file, keeps its block
 * rendered and scrolls to it. Matches are painted with the CSS Custom Highlight API on whichever
 * rows are mounted, so highlighting never re-renders a row (a 10,000-line diff stays one render).
 */

import { ChevronDown, ChevronUp, X } from "lucide-react";
import { useEffect, useMemo, type RefObject } from "react";
import { lineKey, MAX_MATCHES, type DiffMatch, type DiffQuery } from "@/lib/diff-search";
import { scroller } from "./diff-window";

const btn = "grid size-6 place-items-center rounded-md text-subtle transition hover:bg-panel-2 hover:text-fg disabled:opacity-30";

export function FindBar({
  query,
  onQuery,
  count,
  current,
  onStep,
  onClose,
  input,
}: {
  query: DiffQuery;
  onQuery: (q: DiffQuery) => void;
  count: number;
  current: number;
  onStep: (by: 1 | -1) => void;
  onClose: () => void;
  input: RefObject<HTMLInputElement | null>;
}) {
  const label = !query.text ? "" : count === 0 ? "No results" : `${current + 1} of ${count >= MAX_MATCHES ? `${MAX_MATCHES}+` : count}`;
  return (
    <div className="sticky top-2 z-20 flex h-0 justify-end">
      <div role="search" className="flex h-9 items-center gap-1 rounded-lg border border-line-2 bg-panel py-1 pr-1 pl-2.5 text-[12.5px]">
        <input
          ref={input}
          value={query.text}
          onChange={(e) => onQuery({ ...query, text: e.target.value })}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === "ArrowDown" || e.key === "ArrowUp") {
              e.preventDefault();
              onStep(e.key === "ArrowUp" || (e.key === "Enter" && e.shiftKey) ? -1 : 1);
            } else if (e.key === "Escape") {
              // Handled here: the conversation's Escape checks defaultPrevented, so it stays open.
              e.preventDefault();
              onClose();
            }
          }}
          placeholder="Find in diff"
          aria-label="Find in diff"
          spellCheck={false}
          autoComplete="off"
          className="w-40 min-w-0 bg-transparent text-fg outline-none placeholder:text-subtle"
        />
        <span className="min-w-14 text-right font-mono text-[11px] text-subtle tabular-nums" aria-live="polite">
          {label}
        </span>
        <button
          type="button"
          className={`${btn} font-mono text-[11px] ${query.caseSensitive ? "bg-panel-2 text-fg" : ""}`}
          aria-pressed={query.caseSensitive}
          aria-label="Match case"
          title="Match case"
          onClick={() => onQuery({ ...query, caseSensitive: !query.caseSensitive })}
        >
          Aa
        </button>
        <button type="button" className={btn} disabled={count === 0} aria-label="Previous match" title="Previous match (⇧↵)" onClick={() => onStep(-1)}>
          <ChevronUp className="size-3.5" strokeWidth={1.5} />
        </button>
        <button type="button" className={btn} disabled={count === 0} aria-label="Next match" title="Next match (↵)" onClick={() => onStep(1)}>
          <ChevronDown className="size-3.5" strokeWidth={1.5} />
        </button>
        <button type="button" className={btn} aria-label="Close find" title="Close (Esc)" onClick={onClose}>
          <X className="size-3.5" strokeWidth={1.5} />
        </button>
      </div>
    </div>
  );
}

// ── painting ──

let shared: { match: Highlight; current: Highlight } | null | undefined;
/** Two highlights for the page; each diff adds and removes only its own ranges. Colours in globals.css. */
function highlights() {
  if (shared !== undefined) return shared;
  if (typeof CSS === "undefined" || !("highlights" in CSS) || typeof Highlight === "undefined") return (shared = null);
  shared = { match: new Highlight(), current: new Highlight() };
  CSS.highlights.set("diff-find", shared.match);
  CSS.highlights.set("diff-find-current", shared.current);
  return shared;
}

/** A range over characters [start, end) of `el`'s text, across its token spans. */
function textRange(el: Element, start: number, end: number): Range | null {
  const walk = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
  let seen = 0;
  let from: Text | null = null;
  let fromAt = 0;
  for (let n = walk.nextNode() as Text | null; n; n = walk.nextNode() as Text | null) {
    const len = n.data.length;
    if (!from && start < seen + len) {
      from = n;
      fromAt = start - seen;
    }
    if (from && end <= seen + len) {
      const r = document.createRange();
      r.setStart(from, fromAt);
      r.setEnd(n, end - seen);
      return r;
    }
    seen += len;
  }
  return null;
}

/**
 * Paints `matches` on the rows of `root` that are mounted, the `current` one apart, and again as
 * blocks mount or get their syntax colours (a mutation observer, at most once a frame).
 */
export function usePaintMatches(root: RefObject<HTMLElement | null>, matches: readonly DiffMatch[], current: DiffMatch | null) {
  const byLine = useMemo(() => {
    const m = new Map<string, DiffMatch[]>();
    for (const x of matches) {
      const k = lineKey(x.file, x.line);
      const list = m.get(k);
      if (list) list.push(x);
      else m.set(k, [x]);
    }
    return m;
  }, [matches]);
  useEffect(() => {
    const hl = highlights();
    const el = root.current;
    if (!hl || !el || !byLine.size) return;
    let mine: Range[] = [];
    const clear = () => {
      for (const r of mine) {
        hl.match.delete(r);
        hl.current.delete(r);
      }
      mine = [];
    };
    const paint = () => {
      clear();
      for (const code of el.querySelectorAll<HTMLElement>("[data-l]")) {
        const list = byLine.get(code.dataset.l!);
        if (!list) continue;
        for (const m of list) {
          const r = textRange(code, m.start, m.end);
          if (!r) continue;
          (m === current ? hl.current : hl.match).add(r);
          mine.push(r);
        }
      }
    };
    paint();
    let frame = 0;
    const obs = new MutationObserver(() => {
      if (!frame)
        frame = requestAnimationFrame(() => {
          frame = 0;
          paint();
        });
    });
    obs.observe(el, { childList: true, subtree: true });
    return () => {
      obs.disconnect();
      cancelAnimationFrame(frame);
      clear();
    };
  }, [root, byLine, current]);
}

// ── where the reader is, and getting a match there ──

/** The first line on screen in `root` (file, line), or null when none is mounted. */
export function firstVisibleLine(root: HTMLElement): [number, number] | null {
  const top = (scroller(root.parentElement)?.getBoundingClientRect().top ?? 0) + 1;
  for (const code of root.querySelectorAll<HTMLElement>("[data-l]")) {
    if (code.getBoundingClientRect().bottom <= top) continue;
    const [f, l] = code.dataset.l!.split(":").map(Number);
    return [f, l];
  }
  return null;
}

/**
 * Scrolls `m` into the middle of the diff's scroller (and sideways into its table's view) once its
 * row is mounted; false while it isn't yet. Not `scrollIntoView`: that also scrolls the canvas the
 * diff may be pinned on.
 */
export function revealMatch(root: HTMLElement, m: DiffMatch): boolean {
  const code = root.querySelector<HTMLElement>(`[data-l="${lineKey(m.file, m.line)}"]`);
  if (!code) return false;
  const box = scroller(root.parentElement);
  const row = code.getBoundingClientRect();
  const view = box ? box.getBoundingClientRect() : { top: 0, height: window.innerHeight };
  const comfy = row.top >= view.top + view.height * 0.15 && row.bottom <= view.top + view.height * 0.85;
  if (!comfy) {
    const by = row.top + row.height / 2 - (view.top + view.height / 2);
    if (box) box.scrollTop += by;
    else window.scrollBy(0, by);
  }
  const frame = code.closest<HTMLElement>("[data-diff-frame]");
  const range = textRange(code, m.start, m.end);
  if (frame && range && frame.scrollWidth > frame.clientWidth) {
    const r = range.getBoundingClientRect();
    const f = frame.getBoundingClientRect();
    // Sideways only when it's out of view, and back to the start (the line numbers) when that shows it.
    if (r.left < f.left || r.right > f.right - 24) {
      const x = r.left - f.left + frame.scrollLeft;
      frame.scrollLeft = x + r.width <= f.width - 24 ? 0 : x - f.width / 3;
    }
  }
  return true;
}
