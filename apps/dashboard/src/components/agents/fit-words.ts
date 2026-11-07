/**
 * Text cut after its last whole word (or `sep`-separated part) that fits a width, with an ellipsis:
 * a card's meta line never ends mid-word ("…the default bran…"). Measured with a canvas in the
 * card's font; CSS's `truncate` stays on the line as the fallback (before the font loads, in tests).
 */

let ctx: CanvasRenderingContext2D | null | undefined;
let family = "";
const cache = new Map<string, string>();
const MAX_CACHED = 4000;

function measure(): CanvasRenderingContext2D | null {
  if (ctx !== undefined) return ctx;
  try {
    ctx = typeof document === "undefined" ? null : document.createElement("canvas").getContext("2d");
    family = typeof document === "undefined" ? "" : getComputedStyle(document.body).fontFamily;
    void document.fonts?.ready.then(() => cache.clear());
  } catch {
    ctx = null;
  }
  return ctx;
}

/** `text` as it fits `width` px at `px` px (weight 400) in the page's sans font, cut between parts. */
export function fitWords(text: string, width: number, px = 12, sep: RegExp = /(\s+)/): string {
  const c = measure();
  if (!c || !text) return text;
  const key = `${px}|${width}|${sep.source}|${text}`;
  const hit = cache.get(key);
  if (hit !== undefined) return hit;
  c.font = `400 ${px}px ${family}`;
  // A little under the width: the canvas and the page round differently.
  const room = width - 2;
  let out = text;
  if (c.measureText(text).width > room) {
    const parts = text.split(sep);
    let kept = "";
    for (const p of parts) {
      const next = kept + p;
      if (next.trim() && c.measureText(`${next.trimEnd()}…`).width > room) break;
      kept = next;
    }
    const cut = kept.trimEnd().replace(/[\s,;:·–—-]+$/, "");
    // One word longer than the line: let CSS cut it.
    out = cut ? `${cut}…` : text;
  }
  if (cache.size > MAX_CACHED) cache.clear();
  cache.set(key, out);
  return out;
}

/** Words joined by hyphens kept on one line: a name like "notes-app" never breaks at its hyphen. */
export function unbroken(text: string): (string | { nowrap: string })[] {
  return text.split(/(\s+)/).map((w) => (w.includes("-") && !/\s/.test(w) ? { nowrap: w } : w));
}
