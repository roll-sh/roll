import { describe, expect, it } from "vitest";
import type { BlameRange } from "./code-api";
import { blameChangesets, blameRows } from "./blame";

const range = (start: number, end: number, changeset: string, time = 100): BlameRange => ({
  start,
  end,
  changeset,
  git_commit: null,
  author: { name: "ana", email: "ana@example.com", time },
  time,
  message: `msg ${changeset}`,
});

describe("blame rows", () => {
  it("marks where each run starts and alternates bands", () => {
    const rows = blameRows({ ranges: [range(1, 2, "cs_a"), range(3, 3, "cs_b"), range(4, 5, "cs_a")] }, 5);
    expect(rows.map((r) => r.first)).toEqual([true, false, true, true, false]);
    expect(rows.map((r) => r.band)).toEqual([0, 0, 1, 0, 0]);
    expect(rows.map((r) => r.range?.changeset)).toEqual(["cs_a", "cs_a", "cs_b", "cs_a", "cs_a"]);
  });

  it("reads adjacent ranges of one changeset as one run", () => {
    const rows = blameRows({ ranges: [range(1, 2, "cs_a"), range(3, 4, "cs_a")] }, 4);
    expect(rows.filter((r) => r.first)).toHaveLength(1);
    expect(new Set(rows.map((r) => r.band)).size).toBe(1);
  });

  it("sorts out-of-order ranges and lets the first claim win on overlap", () => {
    const rows = blameRows({ ranges: [range(3, 4, "cs_b"), range(1, 3, "cs_a")] }, 4);
    expect(rows.map((r) => r.range?.changeset)).toEqual(["cs_a", "cs_a", "cs_a", "cs_b"]);
  });

  it("leaves uncovered lines empty and ignores ranges past the end", () => {
    const rows = blameRows({ ranges: [range(2, 9, "cs_a")] }, 3);
    expect(rows).toHaveLength(3);
    expect(rows[0].range).toBeNull();
    expect(rows[0].first).toBe(true);
    expect(rows[1].first).toBe(true);
    expect(rows[2].first).toBe(false);
  });

  it("handles an empty file", () => {
    expect(blameRows({ ranges: [] }, 0)).toEqual([]);
  });
});

describe("blame extras", () => {
  it("lists each changeset once, newest first", () => {
    const list = blameChangesets({ ranges: [range(1, 1, "cs_a", 10), range(2, 2, "cs_b", 30), range(3, 3, "cs_a", 10)] });
    expect(list.map((r) => r.changeset)).toEqual(["cs_b", "cs_a"]);
  });

});
