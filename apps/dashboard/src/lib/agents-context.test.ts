import { describe, expect, it } from "vitest";
import { formatContextTokens, formatPercent, readContext } from "./agents-context";

describe("context window", () => {
  it("formats tokens and percentages like T3", () => {
    expect([999, 1_500, 9_960, 45_000, 258_400, 999_499, 999_500, 1_000_000].map(formatContextTokens)).toEqual(["999", "1.5k", "10k", "45k", "258k", "999k", "1M", "1M"]);
    expect([4.48, 5, 9.96, 62.4, 100].map(formatPercent)).toEqual(["4.5%", "5%", "10%", "62%", "100%"]);
  });

  it("reads a percentage only with a window, and nearly full past 90 %", () => {
    const known = readContext({ used_tokens: 124_000, max_tokens: 200_000, total_processed_tokens: 410_000 });
    expect(known).toMatchObject({ percent: 62, full: false, label: "62% of 200k", tokens: "124k/200k", aria: "Context window 62% used", processed: 410_000 });
    expect(readContext({ used_tokens: 190_000, max_tokens: 200_000, total_processed_tokens: null }).full).toBe(true);
    expect(readContext({ used_tokens: 180_000, max_tokens: 200_000, total_processed_tokens: null }).full).toBe(false);
    // Over the window (a provider that reports past it) still reads 100 %.
    expect(readContext({ used_tokens: 260_000, max_tokens: 258_400, total_processed_tokens: null }).percent).toBe(100);
    const unknown = readContext({ used_tokens: 8_500, max_tokens: null, total_processed_tokens: 0 });
    expect(unknown).toMatchObject({ percent: null, full: false, percentLabel: null, label: "8.5k tokens", tokens: "8.5k", aria: "Context window 8.5k tokens used", processed: null });
    expect(readContext({ used_tokens: 10, max_tokens: 0, total_processed_tokens: null }).percent).toBeNull();
  });
});
