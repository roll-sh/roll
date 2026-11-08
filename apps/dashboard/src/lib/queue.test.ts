import { describe, expect, it } from "vitest";
import type { CodeInfo, Queue, QueueBatch, QueueEntry } from "./code-api";
import { batchSegments, canWrite, isAdmin, queueBusy, queueHeadline, queueLanes } from "./queue";

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

// Keeps the Queue type honest against the view helpers' inputs.
export const _typecheck = (q: Queue) => [queueLanes(q), queueHeadline(q)];
