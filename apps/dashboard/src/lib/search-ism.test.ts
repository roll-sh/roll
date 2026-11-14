// Tests of an OpenSearch-flavor data stream's retention as an ISM policy (vitest).
import assert from "node:assert/strict";
import { test } from "vitest";
import { retentionOf, retentionPolicy, rolloverAge, streamPolicy } from "./search-ism";

test("a stream's retention is an ISM policy that never deletes the write index", () => {
  const p = retentionPolicy("app-logs", "7d");
  const [hot, del] = p.policy.states;
  // The write index rolls over first; a backing index is deleted only after its rollover ages out.
  assert.deepEqual(hot.actions, [{ rollover: { min_index_age: "1d", min_primary_shard_size: "50gb" } }]);
  assert.deepEqual(hot.transitions, [{ state_name: "delete", conditions: { min_rollover_age: "7d" } }]);
  assert.deepEqual(del.actions, [{ delete: {} }]);
  assert.deepEqual(p.policy.ism_template, [{ index_patterns: [".ds-app-logs-*"], priority: 100 }]);
  // Read back as OpenSearch prints it (`GET _plugins/_ism/policies/<id>`).
  assert.equal(retentionOf({ _id: "roll-retention-app-logs", policy: p.policy }), "7d");
  assert.equal(retentionOf({ policy: { states: [{ name: "hot", transitions: [] }] } }), null);
  assert.deepEqual(["12h", "7d", "30d", "365d"].map(rolloverAge), ["1h", "1d", "7d", "30d"]);
  assert.equal(streamPolicy({ ".ds-a-000001": { policy_id: "p" }, ".ds-a-000002": { policy_id: "p" }, total_managed_indices: 2 }), "p");
  assert.equal(streamPolicy({ ".ds-a-000001": { policy_id: "p" }, ".ds-a-000002": { "index.plugins.index_state_management.policy_id": null }, total_managed_indices: 1 }), null);
});
