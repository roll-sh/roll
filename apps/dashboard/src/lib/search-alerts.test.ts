import { describe, expect, it } from "vitest";
import { chartOf, formatAlertValue, ruleText } from "./search-alerts";

describe("health alerts", () => {
  it("reads a rule as the Performance tab shows it, beside the chart it watches", () => {
    expect(ruleText({ metric: "p99_ms", op: ">", threshold: 500, window: "5m" })).toBe("p99 > 500 ms over 5m");
    expect(ruleText({ metric: "month_cost_usd", op: ">=", threshold: 50, window: null })).toBe("this month's cost >= $50.00");
    expect([chartOf("throttle_rate"), chartOf("month_cost_usd")]).toEqual(["rejections", null]);
    expect([formatAlertValue("error_rate", 12.345), formatAlertValue("request_rate", null)]).toEqual(["12.3%", "—"]);
  });
});
