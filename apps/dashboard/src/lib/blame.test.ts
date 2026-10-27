import { describe, expect, it } from "vitest";
import type { BlameRange } from "./code-api";
import { blameChangesets, blameNote, blameRows, isBlameKey } from "./blame";

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

  it("explains binary files and truncated history", () => {
    expect(blameNote({ binary: true, complete: true })).toMatch(/binary/);
    expect(blameNote({ binary: false, complete: false })).toMatch(/oldest changeset/);
    expect(blameNote({ binary: false, complete: true })).toBeNull();
  });

  it("toggles on b, not while typing or with modifiers", () => {
    const k = (key: string, mods: Partial<Record<"metaKey" | "ctrlKey" | "altKey", boolean>> = {}) => ({
      key,
      metaKey: false,
      ctrlKey: false,
      altKey: false,
      ...mods,
    });
    expect(isBlameKey(k("b"), { tagName: "BODY" })).toBe(true);
    expect(isBlameKey(k("b"), null)).toBe(true);
    expect(isBlameKey(k("B"), null)).toBe(false);
    expect(isBlameKey(k("b", { metaKey: true }), null)).toBe(false);
    expect(isBlameKey(k("b"), { tagName: "INPUT" })).toBe(false);
    expect(isBlameKey(k("b"), { tagName: "TEXTAREA" })).toBe(false);
    expect(isBlameKey(k("b"), { tagName: "DIV", isContentEditable: true })).toBe(false);
  });
});
