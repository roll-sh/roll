/**
 * Find in a diff (T3's `CodeViewSearch.collectMatches` in its @pierre/diffs patch, MIT): matches
 * come from every file's parsed lines, not the rendered rows, so rows a big diff hasn't drawn yet
 * and folded files are found too. Hunk headers and "\ No newline" notes aren't code: skipped.
 */

import type { Line } from "./diff";

/** One match: file `file`'s line `line` (an index into its parsed lines), characters [start, end). */
export interface DiffMatch {
  file: number;
  line: number;
  start: number;
  end: number;
}

export interface DiffQuery {
  text: string;
  caseSensitive: boolean;
}

/** Past this many the count says "10000+" and stepping covers the first ones. */
export const MAX_MATCHES = 10_000;

export function findInDiff(files: readonly (readonly Line[])[], q: DiffQuery): DiffMatch[] {
  const out: DiffMatch[] = [];
  if (!q.text || q.text.includes("\n")) return out;
  // A regex over the original text: lowercasing can change a line's length, and the offsets with it.
  const pattern = new RegExp(q.text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), q.caseSensitive ? "g" : "gi");
  for (let f = 0; f < files.length; f++) {
    const lines = files[f];
    for (let i = 0; i < lines.length; i++) {
      const l = lines[i];
      if (l.kind === "hunk" || l.kind === "note") continue;
      pattern.lastIndex = 0;
      for (let m = pattern.exec(l.text); m; m = pattern.exec(l.text)) {
        out.push({ file: f, line: i, start: m.index, end: m.index + m[0].length });
        if (out.length >= MAX_MATCHES) return out;
      }
    }
  }
  return out;
}

/** The first match at or after file `file`'s line `line` (where the reader is), else the first. */
export function matchFrom(matches: readonly DiffMatch[], file: number, line: number): number {
  let lo = 0;
  let hi = matches.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    const m = matches[mid];
    if (m.file < file || (m.file === file && m.line < line)) lo = mid + 1;
    else hi = mid;
  }
  return lo < matches.length ? lo : 0;
}

/** The key a rendered line's code element carries (`data-l`), and its matches are grouped under. */
export const lineKey = (file: number, line: number) => `${file}:${line}`;
