/**
 * The merge queue's view logic, kept pure so it can be tested: lanes, wait times, headlines,
 * blocker copy and what a land answer means. The contract is docs/code/http-api.md "Merge queue".
 */

import {
  shortId,
  type Blocker,
  type CodeInfo,
  type LandPolicy,
  type LandResult,
  type Queue,
  type QueueBatch,
  type QueueEntry,
  type QueueEvent,
  type QueueState,
  type Repo,
  type RepoStats,
} from "./code-api";

// ── permissions ──

/** Open mode (no tokens configured) treats every caller as an admin, like the server does. */
export function isAdmin(info: Pick<CodeInfo, "auth"> | null | undefined) {
  if (!info) return false;
  return !info.auth.required || info.auth.caller.kind === "admin";
}

/** Admins and the repo owner's tokens can write. */
export function canWrite(info: Pick<CodeInfo, "auth"> | null | undefined, repo: Pick<Repo, "owner">) {
  if (isAdmin(info)) return true;
  const c = info?.auth.caller;
  return c?.kind === "owner" && c.owner === repo.owner;
}

// ── blockers ──

/** `ready`: mark it ready · `approve`: approve running it · `review`: go to the review composer. */
export type BlockerAction = "ready" | "approve" | "review" | null;

export interface BlockerCopy {
  title: string;
  /** The server's own words, which name the specifics (which checks, which state). */
  detail: string;
  /** What would clear it from the page, if anything. */
  action: BlockerAction;
}

/** Plain, actionable copy per blocker kind; unknown kinds read by their message. */
export function blockerCopy(b: Blocker): BlockerCopy {
  switch (b.kind) {
    case "state":
      return { title: "Not open", detail: b.message, action: null };
    case "draft":
      return { title: "Draft", detail: "Mark it ready when it's done, then queue it.", action: "ready" };
    case "untrusted":
      return {
        title: "Needs approval to run",
        detail: "It came from outside the repo's writers. A writer approves running this revision; a new revision needs approving again.",
        action: "approve",
      };
    case "checks":
      return { title: "Checks haven't passed", detail: b.message, action: null };
    case "needs_approval":
      return { title: "Needs approval", detail: b.message, action: "review" };
    case "needs_codeowner":
      return { title: "Needs a code owner's approval", detail: b.message, action: "review" };
    case "changes_requested":
      return { title: "Changes requested", detail: b.message, action: null };
    case "unresolved_threads":
      return { title: "Unresolved threads", detail: b.message, action: null };
    case "review_pending":
      return { title: "Review pending", detail: b.message, action: null };
    case "review_required":
      return { title: "Review required", detail: b.message, action: null };
    default:
      return { title: b.message, detail: "", action: null };
  }
}

// ── landing ──

/** The land button's verb under a repo's policy. */
export function landVerb(policy: LandPolicy | "required_checks") {
  return policy === "post_merge" ? "Land" : "Add to queue";
}

export interface LandSummary {
  tone: "ok" | "accent" | "danger";
  text: string;
}

/** One line for what a land, enqueue or bypass answered. */
export function landSummary(r: LandResult): LandSummary {
  switch (r.status) {
    case "landed":
      return { tone: "ok", text: `Landed as ${shortId(r.changeset)}` };
    case "queued":
      return { tone: "accent", text: r.position ? `Queued at position ${r.position}` : "Queued" };
    case "conflict": {
      const n = r.paths.length;
      return { tone: "danger", text: `Conflicts in ${n} file${n === 1 ? "" : "s"}` };
    }
    case "blocked": {
      const n = r.blockers.length;
      return { tone: "danger", text: n ? `Can't queue yet: ${n} blocker${n === 1 ? "" : "s"}` : r.error };
    }
  }
}

// ── the queue ──

export interface Lanes {
  /** In the batch being tested, in landing order. */
  batch: QueueEntry[];
  urgent: QueueEntry[];
  normal: QueueEntry[];
}

/** Entries split into the batch in flight and the two waiting lanes, keeping the queue's order. */
export function queueLanes(q: Pick<Queue, "batch" | "entries">): Lanes {
  const inBatch = new Set(q.batch?.changes ?? []);
  const order = new Map((q.batch?.changes ?? []).map((id, i) => [id, i]));
  const batch = q.entries.filter((e) => inBatch.has(e.change)).sort((a, b) => (order.get(a.change) ?? 0) - (order.get(b.change) ?? 0));
  const rest = q.entries.filter((e) => !inBatch.has(e.change));
  return { batch, urgent: rest.filter((e) => e.lane === "urgent"), normal: rest.filter((e) => e.lane !== "urgent") };
}

/** How long an entry waited: until now while queued, until it left otherwise. */
export function waitSeconds(e: Pick<QueueEntry, "enqueued_at" | "finished_at">, now: number) {
  return Math.max(0, (e.finished_at ?? now) - e.enqueued_at);
}

/** `12s`, `4m`, `1h 3m`. */
export function formatWait(s: number) {
  if (!Number.isFinite(s) || s < 0) return "—";
  if (s < 60) return `${Math.floor(s)}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return m ? `${h}h ${m}m` : `${h}h`;
}

const batchVerb: Record<QueueBatch["state"], string> = { testing: "Testing", bisecting: "Bisecting", landing: "Landing" };

/** The queue in a few words, for the tab's header. */
export function queueHeadline(q: Pick<Queue, "state" | "paused" | "batch" | "stats">) {
  const waiting = q.stats.depth - (q.batch?.changes.length ?? 0);
  const tail = waiting > 0 ? ` · ${waiting} waiting` : "";
  if (q.batch) {
    const n = q.batch.changes.length;
    return `${batchVerb[q.batch.state]} a batch of ${n}${tail}${q.paused ? " · pausing after it" : ""}`;
  }
  if (q.paused) return `Paused${tail}`;
  return waiting > 0 ? `${waiting} waiting` : "Idle — nothing waiting";
}

/** Whether the queue is doing something worth the accent (and the mark). */
export function queueBusy(state: QueueState) {
  return state === "testing" || state === "bisecting" || state === "landing";
}

export interface BatchSegment {
  change: string;
  number: number | null;
  title: string;
  /** In the prefix the running test covers. */
  tested: boolean;
}

/**
 * The batch as a row of segments in landing order. While bisecting, the server tests a prefix of
 * it (`testing_prefix`): `split` is where that prefix ends, and the segments after it wait.
 */
export function batchSegments(batch: QueueBatch, entries: QueueEntry[]): { segments: BatchSegment[]; split: number | null } {
  const byChange = new Map(entries.map((e) => [e.change, e]));
  const n = batch.changes.length;
  const prefix = batch.testing_prefix;
  const split = batch.state === "bisecting" && prefix != null && prefix > 0 && prefix < n ? prefix : null;
  const segments = batch.changes.map((change, i) => {
    const e = byChange.get(change);
    return { change, number: e?.number ?? null, title: e?.title ?? change, tested: split == null || i < split };
  });
  return { segments, split };
}

// ── live summaries ──

export interface QueueSummary {
  depth: number;
  state: QueueState;
  /** Changes in the batch in flight; null when nothing is being tested. */
  batchSize: number | null;
}

export function summaryFromEvent(e: QueueEvent): QueueSummary {
  return { depth: e.depth, state: e.state, batchSize: e.batch ? e.batch.changes.length : null };
}

/** The summary a page can show before the first queue event: the repo's own stats. */
export function summaryFromStats(stats: Pick<RepoStats, "queue_depth" | "queue_batch">): QueueSummary {
  const batch = stats.queue_batch ?? 0;
  return { depth: stats.queue_depth, state: batch > 0 ? "testing" : "idle", batchSize: batch > 0 ? batch : null };
}

/** The public strip's queue words, e.g. "12 in queue · landing batch of 8"; null when there's nothing to say. */
export function queueStripText(s: QueueSummary | null) {
  if (!s) return null;
  const parts: string[] = [];
  if (s.depth > 0) parts.push(`${s.depth} in queue`);
  if (s.batchSize && queueBusy(s.state)) parts.push(`${s.state} batch of ${s.batchSize}`);
  return parts.length ? parts.join(" · ") : null;
}

/**
 * Whether a queue event should play `deploying` on the shared mark: only while a batch lands, only
 * when the mark is idle, and at most once per `everyMs`, so a busy queue never flashes.
 */
export function shouldPlayLanding(e: Pick<QueueEvent, "state">, markState: string, now: number, last: number, everyMs: number) {
  return e.state === "landing" && markState === "idle" && now - last >= everyMs;
}

/** Audit actions in words. */
export function auditLabel(action: string) {
  switch (action) {
    case "queue_bypass":
      return "landed past the queue";
    case "queue_pause":
      return "paused the queue";
    case "queue_resume":
      return "resumed the queue";
    case "queue_lane":
      return "moved a change to another lane";
    case "approve_run":
      return "approved a run";
    case "review_bypass":
      return "landed past review";
    case "review_dismiss":
      return "dismissed a review";
    case "review_settings":
      return "changed review settings";
    case "review_app_create":
      return "registered a review app";
    case "review_app_update":
      return "updated a review app";
    case "review_app_rotate":
      return "rotated a review app's credentials";
    case "review_app_delete":
      return "deleted a review app";
    default:
      return action.replace(/_/g, " ");
  }
}
