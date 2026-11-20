// Tests of the Autocomplete section's logic: block lists, the completion request and answer,
// and which indices can have suggestions (vitest).
import assert from "node:assert/strict";
import { test } from "vitest";
import { blockUpdate, completionOptions, completionRequest, normalizeQuery, relative, span, suggestionIndices, unblockUpdate } from "./search-suggestions";

test("blocking a term adds it as analytics keep queries, once; unblocking the last resets the setting", () => {
  assert.equal(normalizeQuery("  Red   SHOES "), "red shoes");
  assert.equal(normalizeQuery("é".repeat(300)).length, 128);
  const cfg = { block: ["bad"], block_prefix: [] };
  assert.deepEqual(blockUpdate(cfg, " Red  Shoes"), { "index.roll.suggestions.block": ["bad", "red shoes"] });
  assert.deepEqual(blockUpdate(cfg, "bad"), { "index.roll.suggestions.block": ["bad"] });
  assert.deepEqual(blockUpdate(cfg, "Secret", true), { "index.roll.suggestions.block_prefix": ["secret"] });
  assert.deepEqual(unblockUpdate(cfg, "bad"), { "index.roll.suggestions.block": null });
  assert.deepEqual(unblockUpdate({ block: ["a", "b"], block_prefix: [] }, "a"), { "index.roll.suggestions.block": ["b"] });
});

test("a prefix is tried with the completion suggester, as clients' autocomplete asks", () => {
  assert.deepEqual(completionRequest("sho", 5).suggest.q, { prefix: "sho", completion: { field: "suggest", size: 5, skip_duplicates: true } });
  const answer = { suggest: { q: [{ text: "sho", options: [{ text: "shoes", _score: 12 }, { text: "short", _score: 3.0 }] }] } };
  assert.deepEqual(completionOptions(answer), [
    { text: "shoes", weight: 12 },
    { text: "short", weight: 3 },
  ]);
  assert.deepEqual(completionOptions({}), []);
});

test("only indices with analytics on are offered, never a suggestions index itself", () => {
  const s = {
    products: { settings: { "index.roll.analytics.enabled": "true", "index.roll.suggestions.enabled": "true" } },
    blog: { settings: { "index.roll.analytics.enabled": "true" } },
    logs: { settings: {} },
    "products-suggestions-1": { settings: { "index.roll.analytics.enabled": "true", "index.roll.suggestions.source": "products" } },
  };
  assert.deepEqual(suggestionIndices(s), [
    { index: "blog", suggestions: false },
    { index: "products", suggestions: true },
  ]);
});

test("times read as spans and relative to now", () => {
  assert.equal(span(30 * 86_400_000), "30d");
  assert.equal(span(36 * 3_600_000), "36h");
  assert.equal(span(90_000), "90s");
  assert.equal(relative(1_000 + 3 * 3_600_000, 1_000), "in 3h");
  assert.equal(relative(1_000, 1_000 + 5 * 60_000), "5m ago");
  assert.equal(relative(1_000, 1_000), "just now");
});
