import { describe, expect, it } from "vitest";
import { mayPulse, measuredRate, prependBatch, prune, windowRate } from "./live-rate";

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

  it("averages short watches over the minimum span so a first landing isn't a spike", () => {
    expect(measuredRate(1, 0.25, 10, 3)).toBeCloseTo(1 / 3);
    expect(measuredRate(30, 6, 10, 3)).toBe(5);
    expect(measuredRate(2000, 120, 10, 3)).toBe(200);
  });

  it("computes window rates", () => {
    expect(windowRate(50, 10)).toBe(5);
    expect(windowRate(5, 0)).toBe(0);
  });

  it("pulses at most once per interval, however many land", () => {
    let last = 0;
    let pulses = 0;
    for (let now = 1_000; now <= 10_000; now += 250) {
      if (mayPulse(now, last, 1_200)) {
        pulses++;
        last = now;
      }
    }
    expect(pulses).toBe(8);
  });

  it("puts a batch on top of a newest-first feed", () => {
    expect(prependBatch(["c", "b"], ["d", "e", "f"], 4)).toEqual(["f", "e", "d", "c"]);
    expect(prependBatch([], [], 3)).toEqual([]);
  });
});
