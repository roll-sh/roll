/**
 * Review view logic, kept pure so it can be tested: where threads sit in a diff (unified and
 * split), suggestion mini-diffs, the review state in words, code owners, the pending review a
 * reviewer builds up before submitting, line selection and keyboard order. The contract is
 * docs/code/http-api.md "Reviews".
 */

import type {
  FileDiff,
  NewReviewComment,
  Review,
  Reviewer,
  ReviewRules,
  ReviewStatus,
  ReviewSummary,
  Thread,
  ThreadSide,
  Verdict,
} from "./code-api";
import type { Line, Side } from "./diff";

// ── threads in a diff ──

/** The threads of one file, in diff order. Renames: old-side threads may carry the old path. */
export function threadsForFile(file: Pick<FileDiff, "path" | "old_path">, threads: Thread[]) {
  return threads
    .filter((t) => t.path === file.path || (file.old_path != null && t.side === "old" && t.path === file.old_path))
    .sort(byPosition);
}

const byPosition = (a: Thread, b: Thread) => a.line - b.line || a.created_at - b.created_at || a.id.localeCompare(b.id);

/** Where a file's threads go: under a diff row, or below the file (outdated, or on lines the diff doesn't show). */
export interface Placement {
  /** Row index → the threads shown right after that row. */
  inline: Map<number, Thread[]>;
  /** Their lines changed in a later revision. */
  outdated: Thread[];
  /** Current, but on lines outside the diff's hunks. */
  offDiff: Thread[];
}

const onSide = (l: Line | null, side: ThreadSide, line: number) =>
  l != null && (side === "new" ? (l.kind === "add" || l.kind === "ctx") && l.new === line : (l.kind === "del" || l.kind === "ctx") && l.old === line);

function place<R>(rows: R[], threads: Thread[], at: (row: R, side: ThreadSide, line: number) => boolean): Placement {
  const inline = new Map<number, Thread[]>();
  const outdated: Thread[] = [];
  const offDiff: Thread[] = [];
  for (const t of threads) {
    if (t.outdated) {
      outdated.push(t);
      continue;
    }
    const i = rows.findIndex((r) => at(r, t.side, t.line));
    if (i === -1) offDiff.push(t);
    else inline.set(i, [...(inline.get(i) ?? []), t]);
  }
  return { inline, outdated, offDiff };
}

/** Threads in a unified diff: each under the row showing its last line on its side. */
export function placeUnified(lines: Line[], threads: Thread[]): Placement {
  return place(lines, threads, (l, side, line) => onSide(l, side, line));
}

/** Threads in a split diff: the left column is the old side, the right the new. */
export function placeSplit(rows: [Side, Side][], threads: Thread[]): Placement {
  return place(rows, threads, ([a, b], side, line) => (side === "old" ? onSide(a, "old", line) : onSide(b, "new", line)));
}

/** The text of new-side lines `start..=end` from a parsed patch; null when any of them isn't in it. */
export function newSideText(lines: Line[], start: number, end: number): string[] | null {
  const byLine = new Map<number, string>();
  for (const l of lines) if (l.new != null && (l.kind === "add" || l.kind === "ctx")) byLine.set(l.new, l.text);
  const out: string[] = [];
  for (let n = start; n <= end; n++) {
    const text = byLine.get(n);
    if (text === undefined) return null;
    out.push(text);
  }
  return out;
}

// ── suggestions ──

export interface MiniLine {
  kind: "ctx" | "del" | "add";
  text: string;
}

/** A suggestion's lines: an empty suggestion deletes the lines; one trailing newline is the fence's. */
export function suggestionLines(suggestion: string): string[] {
  if (suggestion === "") return [];
  const lines = suggestion.replace(/\r\n/g, "\n").split("\n");
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  return lines;
}

/**
 * What applying a suggestion changes, as a small diff: the lines both share at the start and end
 * stay as context, the rest of the original goes and the rest of the suggestion comes in. Without
 * the original lines (not in the diff), it's all additions.
 */
export function suggestionDiff(original: string[] | null, suggestion: string): MiniLine[] {
  const next = suggestionLines(suggestion);
  if (!original) return next.map((text) => ({ kind: "add", text }));
  let head = 0;
  while (head < original.length && head < next.length && original[head] === next[head]) head++;
  let tail = 0;
  while (
    tail < original.length - head &&
    tail < next.length - head &&
    original[original.length - 1 - tail] === next[next.length - 1 - tail]
  )
    tail++;
  return [
    ...original.slice(0, head).map((text) => ({ kind: "ctx" as const, text })),
    ...original.slice(head, original.length - tail).map((text) => ({ kind: "del" as const, text })),
    ...next.slice(head, next.length - tail).map((text) => ({ kind: "add" as const, text })),
    ...original.slice(original.length - tail).map((text) => ({ kind: "ctx" as const, text })),
  ];
}

/** A comment's body without its ```suggestion block (that's drawn as a mini-diff instead). */
export function stripSuggestion(body: string): string {
  return body
    .replace(/(^|\n)[ \t]*(`{3,}|~{3,})[ \t]*suggestion[^\n]*\n[\s\S]*?\n[ \t]*\2[ \t]*(?=\n|$)/g, "$1")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

/** Whether the viewer can apply a comment's suggestion, and why not otherwise. */
export function applyState(opts: {
  thread: Pick<Thread, "outdated" | "side" | "resolved">;
  suggestion: string | null;
  writer: boolean;
  open: boolean;
}): { can: boolean; reason: string | null } {
  if (opts.suggestion == null) return { can: false, reason: null };
  if (!opts.writer) return { can: false, reason: "Only writers can apply suggestions." };
  if (!opts.open) return { can: false, reason: "The change isn't open." };
  if (opts.thread.outdated) return { can: false, reason: "Its lines changed since; it can't apply cleanly." };
  if (opts.thread.side === "old") return { can: false, reason: "It's on removed lines." };
  return { can: true, reason: null };
}

// ── review state ──

export type Tone = "ok" | "danger" | "accent" | "muted";

/** The review state in words, for badges and the summary card. */
export function reviewHeadline(s: ReviewSummary): { label: string; detail: string; tone: Tone } {
  const count = s.required > 0 ? `${s.approvals} of ${s.required} approval${s.required === 1 ? "" : "s"}` : null;
  switch (s.state) {
    case "approved":
      return { label: "Approved", detail: count ?? (s.approvals ? `${s.approvals} approval${s.approvals === 1 ? "" : "s"}` : "no approval required"), tone: "ok" };
    case "auto_approved":
      return { label: "Auto-approved", detail: "an auto-approve rule matched", tone: "ok" };
    case "changes_requested":
      return { label: "Changes requested", detail: count ?? "a reviewer asked for changes", tone: "danger" };
    case "pending":
      return { label: s.required > 0 ? "Review required" : "Awaiting review", detail: count ?? "no approval required", tone: s.required > 0 ? "accent" : "muted" };
  }
}

/** Code owners per pattern, the ones still owed first. */
export function codeownerRows(status: Pick<ReviewStatus, "codeowners">) {
  return [...status.codeowners]
    .sort((a, b) => Number(a.satisfied) - Number(b.satisfied))
    .map((c) => ({
      pattern: c.path_pattern,
      owners: c.owners.length ? c.owners.join(", ") : "no owners",
      satisfied: c.satisfied,
      files: c.paths.length,
    }));
}

/** Why each approval does or doesn't count, by review id. */
export function approvalNotes(status: Pick<ReviewStatus, "approvals"> | null) {
  const out = new Map<string, { counted: boolean; reason: string | null }>();
  for (const a of status?.approvals ?? []) out.set(a.review, { counted: a.counted, reason: a.reason });
  return out;
}

/** A reviewer's display name: GitHub bots without `[bot]` (`coderabbitai[bot]` → `coderabbitai`). */
export function reviewerName(r: Pick<Reviewer, "name"> | string) {
  const name = typeof r === "string" ? r : r.name;
  return name.replace(/\[bot\]$/, "");
}

/** `user` / `agent` / `app` / `roll`, plus `github` for ingested records. */
export function reviewerBadges(r: Reviewer, source: "roll" | "github") {
  const badges: string[] = [r.kind];
  if (source === "github") badges.push("github");
  return badges;
}

const verdictWords: Record<Verdict, string> = { approve: "approved", request_changes: "requested changes", comment: "commented" };
export const verdictLabel = (v: Verdict) => verdictWords[v];

/** Reviews oldest first; dismissed and stale ones stay, marked. */
export function timeline(reviews: Review[]) {
  return [...reviews].sort((a, b) => a.created_at - b.created_at || a.id.localeCompare(b.id));
}

// ── composing a review ──

export interface LineSelection {
  path: string;
  side: ThreadSide;
  start: number;
  end: number;
}

/**
 * Clicking a line selects it; shift-click extends the selection to a range on the same file and
 * side (otherwise it starts over).
 */
export function selectLine(prev: LineSelection | null, at: { path: string; side: ThreadSide; line: number }, extend: boolean): LineSelection {
  if (extend && prev && prev.path === at.path && prev.side === at.side) {
    const anchor = at.line < prev.start ? prev.end : prev.start;
    return { path: at.path, side: at.side, start: Math.min(anchor, at.line), end: Math.max(anchor, at.line) };
  }
  return { path: at.path, side: at.side, start: at.line, end: at.line };
}

export const inSelection = (sel: LineSelection | null, path: string, side: ThreadSide, line: number | null) =>
  sel != null && line != null && sel.path === path && sel.side === side && line >= sel.start && line <= sel.end;

/** The (side, line) spots a diff row shows: a context row is on both sides. */
export function rowSpots(l: Line | null): { side: ThreadSide; line: number }[] {
  if (!l) return [];
  const out: { side: ThreadSide; line: number }[] = [];
  if ((l.kind === "del" || l.kind === "ctx") && l.old != null) out.push({ side: "old", line: l.old });
  if ((l.kind === "add" || l.kind === "ctx") && l.new != null) out.push({ side: "new", line: l.new });
  return out;
}

/** Whether a selection ends on this row (the line comment composer goes under it). */
export function endsSelection(sel: LineSelection | null, path: string, l: Line | null, side?: ThreadSide) {
  if (!sel || sel.path !== path || (side && sel.side !== side)) return false;
  return rowSpots(l).some((s) => s.side === sel.side && s.line === sel.end);
}

/** A selection as `L12` or `L12–L18`. */
export const selectionLabel = (sel: Pick<LineSelection, "start" | "end">) => (sel.start === sel.end ? `L${sel.start}` : `L${sel.start}–L${sel.end}`);

export interface DraftComment extends NewReviewComment {
  /** Local only. */
  id: string;
}

/** Comments a reviewer has written but not submitted, kept per change in local storage. */
export interface PendingReview {
  change: string;
  /** The revision the comments were written against. */
  revision: number;
  body: string;
  comments: DraftComment[];
}

export const pendingKey = (change: string) => `roll.review-draft.${change}`;

export const emptyPending = (change: string, revision: number): PendingReview => ({ change, revision, body: "", comments: [] });

type Store = Pick<Storage, "getItem" | "setItem" | "removeItem">;

/**
 * The pending review for a change. `stale` when it was written against an older revision: the
 * comments stay, but their lines may have moved (the server refuses ones off the file).
 */
export function loadPending(store: Store, change: string, revision: number): { pending: PendingReview; stale: boolean } {
  try {
    const raw = store.getItem(pendingKey(change));
    if (!raw) return { pending: emptyPending(change, revision), stale: false };
    const p = JSON.parse(raw) as PendingReview;
    if (p.change !== change || !Array.isArray(p.comments) || typeof p.revision !== "number") throw new Error("bad draft");
    return { pending: { ...p, body: typeof p.body === "string" ? p.body : "" }, stale: p.revision !== revision };
  } catch {
    return { pending: emptyPending(change, revision), stale: false };
  }
}

export function savePending(store: Store, p: PendingReview) {
  if (!p.comments.length && !p.body.trim()) store.removeItem(pendingKey(p.change));
  else store.setItem(pendingKey(p.change), JSON.stringify(p));
}

let draftSeq = 0;
export function addDraft(p: PendingReview, c: NewReviewComment, id = `draft_${Date.now().toString(36)}_${draftSeq++}`): PendingReview {
  return { ...p, comments: [...p.comments, { ...c, id }] };
}
export function updateDraft(p: PendingReview, id: string, body: string): PendingReview {
  return { ...p, comments: p.comments.map((c) => (c.id === id ? { ...c, body } : c)) };
}
export function removeDraft(p: PendingReview, id: string): PendingReview {
  return { ...p, comments: p.comments.filter((c) => c.id !== id) };
}

/** A draft for a selection: `line` is the last line, `start_line` only for ranges. */
export function draftFor(sel: LineSelection, body: string): NewReviewComment {
  return { path: sel.path, side: sel.side, line: sel.end, ...(sel.start < sel.end ? { start_line: sel.start } : {}), body };
}

/** Whether a review can be submitted: request changes and comment need words or comments. */
export function canSubmit(verdict: Verdict, body: string, comments: number) {
  return verdict === "approve" || body.trim().length > 0 || comments > 0;
}

/** The request body for submitting a pending review. */
export function reviewPayload(p: PendingReview, verdict: Verdict): { verdict: Verdict; body?: string; comments?: NewReviewComment[] } {
  const body = p.body.trim();
  const comments = p.comments.filter((c) => c.body.trim()).map(({ id: _id, ...c }) => ({ ...c, body: c.body.trim() }));
  return { verdict, ...(body ? { body } : {}), ...(comments.length ? { comments } : {}) };
}

/** Pending comments drawn under a diff row (`side` limits it to one column of a split view). */
export function draftsOnRow(p: PendingReview, path: string, l: Line | null, side?: ThreadSide) {
  const spots = rowSpots(l).filter((s) => !side || s.side === side);
  return p.comments.filter((c) => c.path === path && spots.some((s) => s.side === (c.side ?? "new") && s.line === c.line));
}

// ── keyboard ──

/**
 * The order `n`/`p` walk threads in: file order, then by line. Unresolved threads only, unless
 * every thread is resolved.
 */
export function threadOrder(files: Pick<FileDiff, "path" | "old_path">[], threads: Thread[]): string[] {
  const open = threads.filter((t) => !t.resolved);
  const pool = open.length ? open : threads;
  const seen = new Set<string>();
  const out: string[] = [];
  for (const f of files)
    for (const t of threadsForFile(f, pool)) {
      if (seen.has(t.id)) continue;
      seen.add(t.id);
      out.push(t.id);
    }
  // Threads on files the diff doesn't list (renamed away, or gone) go last.
  for (const t of [...pool].sort((a, b) => a.path.localeCompare(b.path) || byPosition(a, b))) if (!seen.has(t.id)) out.push(t.id);
  return out;
}

/** The next (`dir` 1) or previous (-1) thread, wrapping around; the first or last from nowhere. */
export function stepThread(order: string[], current: string | null, dir: 1 | -1): string | null {
  if (!order.length) return null;
  const i = current == null ? -1 : order.indexOf(current);
  if (i === -1) return dir === 1 ? order[0] : order[order.length - 1];
  return order[(i + dir + order.length) % order.length];
}

// ── settings ──

export const RULE_KEYS = [
  "required_approvals",
  "require_codeowners",
  "count_ai_approval",
  "block_on_request_changes",
  "dismiss_stale",
  "require_resolved_threads",
  "allow_self_approval",
  "auto_approve",
] as const satisfies readonly (keyof ReviewRules)[];

/** Only what changed, for the PATCH. */
export function rulesPatch(stored: ReviewRules, edited: ReviewRules): Partial<ReviewRules> {
  const out: Partial<ReviewRules> = {};
  for (const k of RULE_KEYS) {
    if (JSON.stringify(stored[k]) !== JSON.stringify(edited[k])) (out as Record<string, unknown>)[k] = edited[k];
  }
  return out;
}

/** Rules the repo's `.roll/review.yaml` tightened beyond what's stored. */
export function tightened(stored: ReviewRules, effective: ReviewRules): (keyof ReviewRules)[] {
  return RULE_KEYS.filter((k) => k !== "auto_approve" && JSON.stringify(stored[k]) !== JSON.stringify(effective[k]));
}

/** Globs from a comma- or newline-separated field. */
export function parseGlobs(text: string): string[] {
  return [...new Set(text.split(/[,\n]/).map((g) => g.trim()).filter(Boolean))];
}

export const clampApprovals = (n: number) => Math.max(0, Math.min(20, Math.round(Number.isFinite(n) ? n : 0)));
