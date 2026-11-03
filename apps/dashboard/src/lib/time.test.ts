import { describe, expect, it } from "vitest";
import { ago, formatDate, formatDateTime } from "./time";

describe("time", () => {
  it("says how long ago", () => {
    const now = 1_760_000_000;
    expect([0, 44, 60, 90, 3599, 3600, 86_399, 86_400, 29 * 86_400, 45 * 86_400, 400 * 86_400].map((d) => ago(now - d, now))).toEqual([
      "just now",
      "just now",
      "1m ago",
      "2m ago",
      "60m ago",
      "1h ago",
      "24h ago",
      "1d ago",
      "29d ago",
      "2mo ago",
      "1y ago",
    ]);
    expect(ago(now + 100, now)).toBe("just now");
  });

  it("formats dates in UTC", () => {
    expect(formatDate(0)).toBe("Jan 1, 1970");
    expect(formatDateTime(1_700_000_000)).toBe("Nov 14, 2023, 22:13");
  });
});
