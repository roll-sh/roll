import { describe, expect, it } from "vitest";
import type { Metrics } from "@/lib/apps-api";
import { rowOf, worstFirst } from "./project-metrics";

const m = (requests: number[], errors: number[], p95: number[]) => ({ minutes: requests.map((_, i) => i), requests, errors, p95_ms: p95 }) as unknown as Metrics;

describe("project metrics", () => {
  it("sums a service's range and puts the worst first", () => {
    const api = rowOf({ id: "svc_a", name: "api" }, m([10, 30], [1, 3], [120, 480]), [{ state: "open" }, { state: "resolved" }]);
    expect(api).toMatchObject({ requests: 40, errorPct: 10, p95: 480, openErrors: 1 });
    // A minute with no requests has no p95 to count.
    expect(rowOf({ id: "svc_b", name: "web" }, m([5, 0], [0, 0], [90, 900]), []).p95).toBe(90);
    const quiet = rowOf({ id: "svc_c", name: "docs" }, m([0], [0], [0]), []);
    expect(worstFirst([quiet, rowOf({ id: "svc_b", name: "web" }, m([5], [0], [90]), []), api]).map((r) => r.name)).toEqual(["api", "web", "docs"]);
  });
});
