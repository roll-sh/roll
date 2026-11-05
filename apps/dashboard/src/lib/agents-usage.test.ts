import { describe, expect, it } from "vitest";
import type { UsageLimits, UsageWindow } from "./agents-api";
import { aheadOfPace, exhausted, pace, percentLeft, planLabel, resetsIn, tightest, usageSummary } from "./agents-usage";

const NOW = 1_780_000_000_000;
const MIN = 60_000;
const w = (id: string, used: number, extra: Partial<UsageWindow> = {}): UsageWindow => ({
  id,
  kind: "session",
  label: id === "secondary" ? "Weekly" : "Session",
  used_percent: used,
  ...extra,
});

describe("usage limits", () => {
  it("reads percent left and reset times like T3", () => {
    expect(percentLeft(w("primary", 61.4))).toBe(39);
    expect(percentLeft(w("primary", 130))).toBe(0);
    expect(resetsIn(w("p", 1), NOW)).toBeNull();
    expect(resetsIn(w("p", 1, { resets_at: NOW - MIN }), NOW)).toBe("resets soon");
    expect(resetsIn(w("p", 1, { resets_at: NOW + 45 * MIN }), NOW)).toBe("resets in 45m");
    expect(resetsIn(w("p", 1, { resets_at: NOW + 130 * MIN }), NOW)).toBe("resets in 2h 10m");
    expect(resetsIn(w("p", 1, { resets_at: NOW + 3 * 24 * 60 * MIN }), NOW)).toBe("resets in 3d");
  });

  it("marks a window used faster than its pace", () => {
    // A 5-hour window, 2.5 hours in: the pace is 50%.
    const half = { resets_at: NOW + 150 * MIN, window_minutes: 300 };
    expect(pace(w("p", 0, half), NOW)).toBe(50);
    expect(aheadOfPace(w("p", 54, half), NOW)).toBe(false);
    expect(aheadOfPace(w("p", 56, half), NOW)).toBe(true);
    expect(pace(w("p", 0), NOW)).toBeNull();
  });

  it("summarizes the tightest window, or the one that ran out", () => {
    const limits: UsageLimits = { checked_at: NOW, windows: [w("primary", 20), w("secondary", 70, { kind: "weekly" })] };
    expect(tightest(limits)?.id).toBe("secondary");
    expect(usageSummary({ usage_limits: limits }, NOW)).toBe("30% left · weekly");
    const out: UsageLimits = {
      checked_at: NOW,
      windows: [w("primary", 100, { resets_at: NOW + 30 * MIN }), w("secondary", 100, { kind: "weekly", resets_at: NOW + 2 * 24 * 60 * MIN })],
    };
    expect(exhausted(out)?.id).toBe("secondary");
    expect(usageSummary({ usage_limits: out }, NOW)).toBe("Weekly limit reached · resets in 2d");
    expect(usageSummary({}, NOW)).toBeNull();
    expect(usageSummary({ usage_limits: { checked_at: NOW, windows: [] } }, NOW)).toBeNull();
    expect(planLabel("plus")).toBe("Plus");
    expect(planLabel(undefined)).toBeNull();
  });
});
