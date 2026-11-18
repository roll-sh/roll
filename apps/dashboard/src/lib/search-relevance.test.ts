// Tests of the Relevance tab's logic: synonym rule lines and query-rule sentences and tests (vitest).
import assert from "node:assert/strict";
import { test } from "vitest";
import {
  draftToRule,
  linesToRules,
  matchedActions,
  moveRule,
  reloadSummary,
  ruleProblem,
  ruleSentence,
  ruleTestRequest,
  ruleToDraft,
  rulesToLines,
  sentenceText,
  synonymSetBody,
  type QueryRule,
} from "./search-relevance";

test("synonym rules go to lines and back, keeping the ids of the rules kept", () => {
  const existing = [
    { id: "r1", synonyms: "laptop, notebook" },
    { id: "r2", synonyms: "tv => television" },
    { id: "r3", synonyms: "phone, mobile" },
  ];
  assert.equal(rulesToLines(existing), "laptop, notebook\ntv => television\nphone, mobile");
  // r2 edited, r3 removed, a comment, a blank line and a new rule.
  const text = "laptop, notebook\n\n# kept as is above\ntv => television set\n  sofa, couch  \n";
  const rules = linesToRules(text, existing);
  assert.deepEqual(rules, [{ id: "r1", synonyms: "laptop, notebook" }, { synonyms: "tv => television set" }, { synonyms: "sofa, couch" }]);
  assert.deepEqual(synonymSetBody(rules), { synonyms_set: rules });
  // The same text twice keeps one id once.
  assert.deepEqual(linesToRules("a, b\na, b", [{ id: "x", synonyms: "a, b" }]), [{ id: "x", synonyms: "a, b" }, { synonyms: "a, b" }]);
  assert.equal(ruleProblem("a, b => c"), null);
  assert.equal(ruleProblem("a => b => c"), "Only one => per rule.");
  assert.equal(ruleProblem("a, , b"), "A term is empty: check the commas.");
  assert.equal(ruleProblem("a\\, b, c"), null);
});

test("a synonyms update's reload report reads as a sentence", () => {
  const s = reloadSummary("products", {
    result: "updated",
    reload_analyzers_details: {
      _shards: { total: 2, successful: 1, failed: 0 },
      reload_details: [{ index: "shop", reloaded_analyzers: ["shop_search"], reloaded_node_ids: ["n1"] }],
    },
  });
  assert.deepEqual(s.indices, [{ index: "shop", analyzers: ["shop_search"] }]);
  assert.equal(s.text, "Saved products and reloaded the analyzer using it: shop_search in shop. Searches use the new rules now.");
  const none = reloadSummary("products", { result: "created", reload_analyzers_details: { _shards: { total: 0, successful: 0, failed: 0 }, reload_details: [] } });
  assert.equal(none.text, "Created products. No index's analyzers use it yet, so there was nothing to reload.");
});

const pin: QueryRule = {
  rule_id: "iphone",
  type: "pinned",
  criteria: [{ type: "exact", metadata: "query_string", values: ["iphone", "iphone 15"] }],
  actions: { ids: ["doc-1", "doc-7"] },
};

test("a query rule reads as a sentence and round-trips through the editor", () => {
  assert.equal(sentenceText(ruleSentence(pin)), "When query_string is exactly 'iphone' or 'iphone 15', pin doc-1, doc-7");
  const hide: QueryRule = {
    rule_id: "cheap",
    type: "exclude",
    criteria: [
      { type: "lt", metadata: "price", values: [10] },
      { type: "prefix", metadata: "query_string", values: ["case"] },
    ],
    actions: { docs: [{ _index: "shop", _id: "d9" }] },
    priority: 3,
  };
  assert.equal(sentenceText(ruleSentence(hide)), "When price is less than 10 and query_string starts with 'case', hide shop/d9");
  assert.equal(sentenceText(ruleSentence({ ...pin, criteria: [{ type: "always" }] })), "Always pin doc-1, doc-7");
  assert.deepEqual(draftToRule(ruleToDraft(pin)), pin);
  assert.deepEqual(draftToRule(ruleToDraft(hide)), hide);
  assert.throws(() => draftToRule({ ...ruleToDraft(pin), priority: "-1" }), /whole number/);
});

test("moving a rule renumbers priorities in the new order", () => {
  const rules = ["a", "b", "c"].map((id) => ({ ...pin, rule_id: id }));
  assert.deepEqual(
    moveRule(rules, 2, 0).map((r) => [r.rule_id, r.priority]),
    [["c", 0], ["a", 1], ["b", 2]],
  );
});

test("a rule test runs a rule retriever over a plain query and marks the matched rules' pins", () => {
  const t = { index: "shop", ruleset: "promos", query: "iphone", criteria: [{ key: "query_string", value: "iphone" }, { key: " ", value: "x" }] };
  assert.deepEqual(ruleTestRequest(t), {
    path: "/shop/_search",
    body: {
      size: 10,
      retriever: {
        rule: {
          ruleset_ids: ["promos"],
          match_criteria: { query_string: "iphone" },
          retriever: { standard: { query: { simple_query_string: { query: "iphone" } } } },
        },
      },
    },
  });
  assert.deepEqual((ruleTestRequest({ ...t, query: " " }).body as { retriever: { rule: { retriever: unknown } } }).retriever.rule.retriever, { standard: { query: { match_all: {} } } });
  const hide: QueryRule = { rule_id: "h", type: "exclude", criteria: [{ type: "always" }], actions: { ids: ["doc-7"] } };
  assert.deepEqual(matchedActions([pin, hide], ["iphone", "h"]), { pinned: [{ id: "doc-1" }], hidden: [{ id: "doc-7" }] });
  assert.deepEqual(matchedActions([pin, hide], []), { pinned: [], hidden: [] });
});
