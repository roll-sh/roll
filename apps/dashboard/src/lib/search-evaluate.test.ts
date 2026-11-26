// Tests of offline evaluation's dashboard logic: judged queries from JSON or CSV (vitest).
import assert from "node:assert/strict";
import { test } from "vitest";
import { parseConfiguration, parseJudgments } from "./search-evaluate";

test("judged queries come from CSV grouped by query, or from JSON as the route takes them", () => {
  const csv = "Query,doc_id,Rating\nred shoes,1,3\nblue,9,0\nred shoes,7,1\n";
  assert.deepEqual(parseJudgments(csv), [
    { query: "red shoes", ratings: [{ _id: "1", rating: 3 }, { _id: "7", rating: 1 }] },
    { query: "blue", ratings: [{ _id: "9", rating: 0 }] },
  ]);
  assert.throws(() => parseJudgments("query,_id\nx,1\n"), /rating/);
  assert.throws(() => parseJudgments("query,_id,rating\nx,1,high\n"), /Line 2/);
  assert.deepEqual(parseJudgments('{"judgments":[{"query":"x","ratings":[]}]}'), [{ query: "x", ratings: [] }]);
  // A bare search body is the request.
  assert.deepEqual(parseConfiguration("a", '{"query":{"match":{"t":"{{query}}"}}}'), { name: "a", request: { query: { match: { t: "{{query}}" } } } });
});
