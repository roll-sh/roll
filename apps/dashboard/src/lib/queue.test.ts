import { describe, expect, it } from "vitest";
import type { CodeInfo, LandResult, Queue, QueueBatch, QueueEntry } from "./code-api";
import {
  auditLabel,
  batchSegments,
  blockerCopy,
  canWrite,
  formatWait,
  isAdmin,
  landSummary,
  landVerb,
  queueBusy,
  queueHeadline,
  queueLanes,
  queueStripText,
  shouldPlayLanding,
  summaryFromEvent,
  summaryFromStats,
  waitSeconds,
} from "./queue";

const entry = (change: string, over: Partial<QueueEntry> = {}): QueueEntry => ({
  id: `qe_${change}`,
  change,
  number: Number(change.replace(/\D/g, "")) || 0,
  title: `change ${change}`,
  author: "agent",
  position: null,
  lane: "normal",
  state: "waiting",
  error: null,
  batch: null,
  enqueued_at: 100,
  enqueued_by: null,
  finished_at: null,
  ...over,
});

const batch = (changes: string[], state: QueueBatch["state"] = "testing", testing_prefix: number | null = null): QueueBatch => ({
  id: "qb_1",
  changes,
  tip: "cs_tip",
  started_at: 0,
  state,
  testing_prefix,
  checks: [],
});

const info = (required: boolean, kind: CodeInfo["auth"]["caller"]["kind"], owner: string | null = null) =>
  ({ auth: { required, caller: { kind, owner } } }) as Pick<CodeInfo, "auth">;

describe("permissions", () => {
  it("treats open mode as admin, like the server", () => {
    expect(isAdmin(info(false, "anonymous"))).toBe(true);
    expect(canWrite(info(false, "anonymous"), { owner: "acme" })).toBe(true);
  });

  it("lets admins and the repo owner write, nobody else", () => {
    expect(isAdmin(info(true, "admin"))).toBe(true);
    expect(isAdmin(info(true, "owner", "acme"))).toBe(false);
    expect(canWrite(info(true, "owner", "acme"), { owner: "acme" })).toBe(true);
    expect(canWrite(info(true, "owner", "other"), { owner: "acme" })).toBe(false);
    expect(canWrite(info(true, "anonymous"), { owner: "acme" })).toBe(false);
    expect(canWrite(null, { owner: "acme" })).toBe(false);
  });
});

describe("blockers", () => {
  it("gives every known kind actionable copy", () => {
    expect(blockerCopy({ kind: "draft", message: "change 3 is a draft" })).toMatchObject({ title: "Draft", action: "ready" });
    expect(blockerCopy({ kind: "untrusted", message: "from a fork" })).toMatchObject({ title: "Needs approval to run", action: "approve" });
    expect(blockerCopy({ kind: "checks", message: "test hasn't passed" })).toEqual({
      title: "Checks haven't passed",
      detail: "test hasn't passed",
      action: null,
    });
    expect(blockerCopy({ kind: "state", message: "change 3 is closed" })).toEqual({ title: "Not open", detail: "change 3 is closed", action: null });
  });

  it("gives review blockers copy, and sends approvals to the composer", () => {
    expect(blockerCopy({ kind: "needs_approval", message: "needs 1 more approval" })).toEqual({
      title: "Needs approval",
      detail: "needs 1 more approval",
      action: "review",
    });
    expect(blockerCopy({ kind: "needs_codeowner", message: "@acme/infra must approve crates/**" })).toMatchObject({
      title: "Needs a code owner's approval",
      action: "review",
    });
    for (const kind of ["changes_requested", "unresolved_threads", "review_pending", "review_required"]) {
      const copy = blockerCopy({ kind, message: `about ${kind}` });
      expect(copy.detail).toBe(`about ${kind}`);
      expect(copy.action).toBeNull();
      expect(copy.title).not.toBe(`about ${kind}`);
    }
  });

  it("reads unknown kinds by their message", () => {
    expect(blockerCopy({ kind: "something_new", message: "a new rule" })).toEqual({ title: "a new rule", detail: "", action: null });
  });
});

describe("landing", () => {
  it("names the action by policy, legacy name included", () => {
    expect(landVerb("post_merge")).toBe("Land");
    expect(landVerb("queue")).toBe("Add to queue");
    expect(landVerb("required_checks")).toBe("Add to queue");
  });

  it("summarises every land answer", () => {
    const cases: [LandResult, string, string][] = [
      [{ status: "landed", changeset: `cs_${"a".repeat(64)}`, git_commit: null, ref: "main", generation: 3 }, "ok", `Landed as cs_${"a".repeat(12)}`],
      [{ status: "queued", position: 4, queue_id: "qe_1", change: {} as never }, "accent", "Queued at position 4"],
      [{ status: "queued", position: null, queue_id: "qe_1", change: {} as never }, "accent", "Queued"],
      [{ status: "conflict", error: null, paths: ["a"], head: "cs_h" }, "danger", "Conflicts in 1 file"],
      [{ status: "conflict", error: null, paths: ["a", "b"], head: "cs_h" }, "danger", "Conflicts in 2 files"],
      [{ status: "blocked", error: "can't queue", blockers: [{ kind: "draft", message: "draft" }] }, "danger", "Can't queue yet: 1 blocker"],
      [{ status: "blocked", error: "can't queue", blockers: [] }, "danger", "can't queue"],
    ];
    for (const [r, tone, text] of cases) expect(landSummary(r), text).toEqual({ tone, text });
  });
});

describe("queue view", () => {
  it("splits the batch from the lanes and keeps landing order", () => {
    const q = {
      batch: batch(["chg_2", "chg_1"]),
      entries: [entry("chg_1"), entry("chg_2"), entry("chg_5", { lane: "urgent" }), entry("chg_3"), entry("chg_4", { lane: "urgent" })],
    };
    const lanes = queueLanes(q);
    expect(lanes.batch.map((e) => e.change)).toEqual(["chg_2", "chg_1"]);
    expect(lanes.urgent.map((e) => e.change)).toEqual(["chg_5", "chg_4"]);
    expect(lanes.normal.map((e) => e.change)).toEqual(["chg_3"]);
    expect(queueLanes({ batch: null, entries: [] })).toEqual({ batch: [], urgent: [], normal: [] });
  });

  it("measures waits until now, or until the entry left", () => {
    expect(waitSeconds({ enqueued_at: 100, finished_at: null }, 190)).toBe(90);
    expect(waitSeconds({ enqueued_at: 100, finished_at: 130 }, 999)).toBe(30);
    expect(waitSeconds({ enqueued_at: 100, finished_at: null }, 50)).toBe(0);
    expect([0, 59, 60, 3599, 3600, 3660, 7200, -1, Number.NaN].map(formatWait)).toEqual(["0s", "59s", "1m", "59m", "1h", "1h 1m", "2h", "—", "—"]);
  });

  it("headlines every state", () => {
    const stats = (depth: number) => ({ depth, landed_1h: 0, avg_wait_s: 0, batch_size: 4, max_batch_size: 8 });
    expect(queueHeadline({ state: "idle", paused: false, batch: null, stats: stats(0) })).toBe("Idle — nothing waiting");
    expect(queueHeadline({ state: "idle", paused: false, batch: null, stats: stats(3) })).toBe("3 waiting");
    expect(queueHeadline({ state: "paused", paused: true, batch: null, stats: stats(2) })).toBe("Paused · 2 waiting");
    expect(queueHeadline({ state: "paused", paused: true, batch: null, stats: stats(0) })).toBe("Paused");
    expect(queueHeadline({ state: "testing", paused: false, batch: batch(["a", "b"]), stats: stats(5) })).toBe("Testing a batch of 2 · 3 waiting");
    expect(queueHeadline({ state: "bisecting", paused: false, batch: batch(["a", "b"], "bisecting"), stats: stats(2) })).toBe("Bisecting a batch of 2");
    expect(queueHeadline({ state: "landing", paused: true, batch: batch(["a"], "landing"), stats: stats(1) })).toBe(
      "Landing a batch of 1 · pausing after it",
    );
  });

  it("knows which states are busy", () => {
    expect(["idle", "paused", "testing", "bisecting", "landing"].map((s) => queueBusy(s as never))).toEqual([false, false, true, true, true]);
  });

  it("splits a bisecting batch exactly where the tested prefix ends", () => {
    const ids = ["chg_1", "chg_2", "chg_3", "chg_4"];
    const entries = ids.map((id) => entry(id));
    const testing = batchSegments(batch(ids, "testing", 4), entries);
    expect(testing.split).toBeNull();
    expect(testing.segments.every((s) => s.tested)).toBe(true);
    const bisect = batchSegments(batch(ids, "bisecting", 1), entries);
    expect(bisect.split).toBe(1);
    expect(bisect.segments.map((s) => [s.change, s.number, s.tested])).toEqual([
      ["chg_1", 1, true],
      ["chg_2", 2, false],
      ["chg_3", 3, false],
      ["chg_4", 4, false],
    ]);
    expect(batchSegments(batch(ids, "bisecting", 3), entries).split).toBe(3);
  });

  it("draws no split without a usable prefix", () => {
    const ids = ["chg_1", "chg_2"];
    // No prefix reported, the whole batch, or none of it: no divider, nothing dimmed.
    for (const prefix of [null, 2, 0]) {
      const b = batchSegments(batch(ids, "bisecting", prefix), []);
      expect(b.split).toBeNull();
      expect(b.segments.every((s) => s.tested)).toBe(true);
    }
    // Landing never splits, whatever the prefix says.
    expect(batchSegments(batch(ids, "landing", 1), []).split).toBeNull();
    // A change the entries don't know yet still shows, by id.
    expect(batchSegments(batch(["chg_9"], "bisecting", 1), []).segments).toEqual([{ change: "chg_9", number: null, title: "chg_9", tested: true }]);
  });
});

describe("live summaries", () => {
  it("summarises queue events and words them for the strip", () => {
    const s = summaryFromEvent({ state: "landing", paused: false, depth: 12, batch: batch(new Array(8).fill(0).map((_, i) => `chg_${i}`), "landing") });
    expect(s).toEqual({ depth: 12, state: "landing", batchSize: 8 });
    expect(queueStripText(s)).toBe("12 in queue · landing batch of 8");
    expect(queueStripText({ depth: 3, state: "idle", batchSize: null })).toBe("3 in queue");
    expect(queueStripText({ depth: 0, state: "testing", batchSize: 2 })).toBe("testing batch of 2");
    expect(queueStripText({ depth: 0, state: "idle", batchSize: null })).toBeNull();
    expect(queueStripText({ depth: 2, state: "paused", batchSize: 2 })).toBe("2 in queue");
    expect(queueStripText(null)).toBeNull();
    expect(summaryFromEvent({ state: "idle", paused: false, depth: 0, batch: null }).batchSize).toBeNull();
  });

  it("seeds the strip from the repo's stats before any event", () => {
    expect(summaryFromStats({ queue_depth: 12, queue_batch: 8 })).toEqual({ depth: 12, state: "testing", batchSize: 8 });
    expect(queueStripText(summaryFromStats({ queue_depth: 12, queue_batch: 8 }))).toBe("12 in queue · testing batch of 8");
    expect(summaryFromStats({ queue_depth: 3, queue_batch: 0 })).toEqual({ depth: 3, state: "idle", batchSize: null });
    // Servers that don't send queue_batch yet.
    expect(summaryFromStats({ queue_depth: 0 })).toEqual({ depth: 0, state: "idle", batchSize: null });
  });

  it("plays the mark for landings only, from idle, throttled", () => {
    expect(shouldPlayLanding({ state: "landing" }, "idle", 20_000, 0, 12_000)).toBe(true);
    expect(shouldPlayLanding({ state: "landing" }, "idle", 20_000, 10_000, 12_000)).toBe(false);
    expect(shouldPlayLanding({ state: "landing" }, "deploying", 20_000, 0, 12_000)).toBe(false);
    expect(shouldPlayLanding({ state: "testing" }, "idle", 20_000, 0, 12_000)).toBe(false);
  });

  it("words audit actions", () => {
    expect(auditLabel("queue_bypass")).toBe("landed past the queue");
    expect(auditLabel("queue_pause")).toBe("paused the queue");
    expect(auditLabel("queue_resume")).toBe("resumed the queue");
    expect(auditLabel("approve_run")).toBe("approved a run");
    expect(auditLabel("queue_lane")).toBe("moved a change to another lane");
    expect(auditLabel("review_bypass")).toBe("landed past review");
    expect(auditLabel("review_dismiss")).toBe("dismissed a review");
    expect(auditLabel("review_settings")).toBe("changed review settings");
    expect(auditLabel("review_app_create")).toBe("registered a review app");
    expect(auditLabel("review_app_rotate")).toBe("rotated a review app's credentials");
    expect(auditLabel("something_new")).toBe("something new");
  });
});

// Keeps the Queue type honest against the view helpers' inputs.
export const _typecheck = (q: Queue) => [queueLanes(q), queueHeadline(q)];
