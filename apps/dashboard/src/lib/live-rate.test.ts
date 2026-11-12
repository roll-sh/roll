import { describe, expect, it } from "vitest";
import { measuredRate, prune, windowRate } from "./live-rate";

describe("live rate", () => {
  it("prunes timestamps outside the window, in place", () => {
    const t = [1_000, 5_000, 9_000, 10_500];
    expect(prune(t, 11_000, 5)).toBe(t);
    expect(t).toEqual([9_000, 10_500]);
    expect(prune([], 0, 10)).toEqual([]);
  });

  it("waits for a landing or a full window before measuring", () => {
    expect(measuredRate(0, 2, 10, 3)).toBeNull();
    expect(measuredRate(0, 10, 10, 3)).toBe(0);
  });

  it("computes window rates", () => {
    expect(windowRate(50, 10)).toBe(5);
    expect(windowRate(5, 0)).toBe(0);
  });

});
