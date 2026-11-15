import { describe, expect, it } from "vitest";
import type { Deployment } from "@/lib/apps-api";
import { deployMarks } from "./metrics";

const dep = (id: string, status: Deployment["status"], finishedMin: number | null, extra: Partial<Deployment> = {}): Deployment =>
  ({ id, service: "svc_a", environment: "production", status, trigger: "push", actor: null, revision: null, message: "", image: null, plan: null, rollback_of: null, error: null, created_at: 0, started_at: null, finished_at: finishedMin === null ? null : finishedMin * 60_000, ready_ms: null, ...extra }) as Deployment;

describe("deploy marks on the metrics charts", () => {
  it("places each deployment that went live in the window at its point, and leaves out the rest", () => {
    // Points 10 minutes apart from minute 100: 100, 110, 120.
    const marks = deployMarks(
      [
        dep("dep_now", "active", 125.5, { revision: "a1b2c3d4e5", message: "fix login\nbody", actor: "ada" }),
        dep("dep_was", "inactive", 103, { rollback_of: "dep_x" }),
        dep("dep_failed", "failed", 111),
        dep("dep_queued", "queued", null),
        dep("dep_old", "inactive", 99),
        dep("dep_after", "inactive", 130),
      ],
      [100, 110, 120],
      10,
    );
    expect(marks.map((k) => [k.i, k.short, k.label])).toEqual([
      [0, "dep_was", "dep_was · rollback"],
      [2, "a1b2c3d", "a1b2c3d · fix login · ada"],
    ]);
  });
});
