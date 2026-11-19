// Tests of search analytics' dashboard logic: enabled indices, the series and the prefills (vitest).
import assert from "node:assert/strict";
import { test } from "vitest";
import { analyticsQuery, enabledIndices, fillPoints, percent, rulePrefill, ruleIdFor, synonymPrefill } from "./search-analytics";
import { draftToRule } from "./search-relevance";

test("the indices with analytics on come from their flat settings", () => {
  const s = {
    products: { settings: { "index.roll.analytics.enabled": "true" } },
    logs: { settings: {} },
    docs: { settings: { "index.roll.analytics.enabled": "false" } },
    blog: { settings: { "index.roll.analytics.enabled": "TRUE" } },
  };
  assert.deepEqual(enabledIndices(s), ["blog", "products"]);
  assert.equal(analyticsQuery("my index", "7d"), "range=7d&index=my+index");
  assert.equal(analyticsQuery(null, "24h"), "range=24h");
});

test("the series has a point per step, zero where nothing was recorded", () => {
  const pts = fillPoints({ from: 3600, step_s: 3600, points: [{ at: 7200, searches: 4, no_hits: 1, clicks: 2 }] }, 3 * 3600 + 5);
  assert.deepEqual(
    pts.map((p) => [p.at, p.searches]),
    [
      [3600, 0],
      [7200, 4],
      [10800, 0],
    ],
  );
  assert.equal(percent(0.04), "4%");
  assert.equal(percent(0.6833), "68%");
  assert.equal(percent(0.004), "<1%");
  assert.equal(percent(0), "0%");
  assert.equal(percent(null), "—");
});

test("a query's actions prefill a synonym and an exact query rule", () => {
  assert.equal(synonymPrefill("laptop"), "laptop, ");
  assert.equal(ruleIdFor("  Red Shoes!! size 42 "), "query-red-shoes-size-42");
  assert.equal(ruleIdFor("???"), "query-rule");
  const rule = draftToRule({ ...rulePrefill("red shoes"), ids: "doc-1" });
  assert.deepEqual(rule.criteria, [{ type: "exact", metadata: "query_string", values: ["red shoes"] }]);
  assert.equal(rule.rule_id, "query-red-shoes");
});
