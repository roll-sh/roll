/**
 * Blame gutter layout: which changeset each line belongs to, where a run of lines starts (the
 * gutter shows its id, author and age only there), and alternating bands so runs read apart. Pure,
 * so it's unit-tested.
 */

import type { Blame, BlameRange } from "./code-api";

export interface BlameRow {
  /** 1-based line number. */
  line: number;
  /** The range the line belongs to; null for lines the blame doesn't cover. */
  range: BlameRange | null;
  /** First line of a run: the gutter shows the changeset here. */
  first: boolean;
  /** 0 or 1, alternating per run. */
  band: 0 | 1;
}

/**
 * One row per line, `lines` long. Adjacent ranges of the same changeset read as one run. Ranges
 * past the end, overlapping or out of order are tolerated: the first range to claim a line wins.
 */
export function blameRows(blame: Pick<Blame, "ranges">, lines: number): BlameRow[] {
  const owner: (BlameRange | null)[] = Array.from({ length: lines }, () => null);
  const ranges = [...blame.ranges].sort((a, b) => a.start - b.start);
  for (const r of ranges) {
    const from = Math.max(1, r.start);
    const to = Math.min(lines, r.end);
    for (let n = from; n <= to; n++) if (!owner[n - 1]) owner[n - 1] = r;
  }
  const rows: BlameRow[] = [];
  let band: 0 | 1 = 1;
  let prev: string | null | undefined;
  for (let i = 0; i < lines; i++) {
    const r = owner[i];
    const key = r ? r.changeset : null;
    const first = i === 0 || key !== prev;
    if (first) band = band === 0 ? 1 : 0;
    rows.push({ line: i + 1, range: r, first, band });
    prev = key;
  }
  return rows;
}

/** Distinct changesets in the file, newest first: the legend under the toggle. */
export function blameChangesets(blame: Pick<Blame, "ranges">): BlameRange[] {
  const seen = new Map<string, BlameRange>();
  for (const r of blame.ranges) if (!seen.has(r.changeset)) seen.set(r.changeset, r);
  return [...seen.values()].sort((a, b) => b.time - a.time);
}

/** Why the gutter is empty or partial, in a sentence; null when there's nothing to say. */
export function blameNote(blame: Pick<Blame, "binary" | "complete">): string | null {
  if (blame.binary) return "Blame isn't available for binary files or files over 1 MiB.";
  if (!blame.complete) return "History is long: the oldest lines are attributed to the oldest changeset reached.";
  return null;
}

/** `b` toggles blame unless the user is typing. */
export function isBlameKey(e: Pick<KeyboardEvent, "key" | "metaKey" | "ctrlKey" | "altKey">, target: { tagName?: string; isContentEditable?: boolean } | null) {
  if (e.key !== "b" || e.metaKey || e.ctrlKey || e.altKey) return false;
  if (target && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName ?? ""))) return false;
  return true;
}
