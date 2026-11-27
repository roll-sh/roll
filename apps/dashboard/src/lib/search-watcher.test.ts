// Tests of the Watcher page's logic: starter watches and where a watch stands (vitest).
import assert from "node:assert/strict";
import { test } from "vitest";
import { watchFromTemplate, watchState } from "./search-watcher";

test("starter watches count in their window and fire as their template says", () => {
  const w = watchFromTemplate({ kind: "errors", index: "logs-*", threshold: 10, interval: "5m", query: "log.level:error", webhook: "https://hooks.example.com/x" }) as any;
  assert.deepEqual(w.trigger, { schedule: { interval: "5m" } });
  assert.deepEqual(w.input.search.request.body.query.bool.filter, [{ range: { "@timestamp": { gte: "now-5m" } } }, { query_string: { query: "log.level:error" } }]);
  assert.deepEqual(w.condition, { compare: { "ctx.payload.hits.total": { gt: 10 } } });
  assert.equal(w.actions.notify.webhook.url, "https://hooks.example.com/x");
  const quiet = watchFromTemplate({ kind: "silence", index: "logs-app", threshold: 0, interval: "1h", query: "ignored", webhook: "" }) as any;
  assert.deepEqual(quiet.condition, { compare: { "ctx.payload.hits.total": { eq: 0 } } });
  assert.equal(quiet.input.search.request.body.query.bool.filter.length, 1);
  assert.equal(quiet.actions.notify, undefined);
});

test("a watch's state reads from its status", () => {
  assert.equal(watchState({ state: { active: false } }), "paused");
  assert.equal(watchState({}), "not run yet");
  assert.equal(watchState({ last_checked: "t1", last_met_condition: "t1" }), "firing");
  assert.equal(watchState({ last_checked: "t2", last_met_condition: "t1" }), "ok");
  assert.equal(watchState({ last_checked: "t2", actions: { a: { last_execution: { successful: false } } } }), "failing");
});
