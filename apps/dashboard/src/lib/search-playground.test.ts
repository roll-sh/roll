// Tests of the search playground's requests, rank movement and copied code (vitest).
import assert from "node:assert/strict";
import { test } from "vitest";
import {
  buildRequest,
  copyAs,
  defaultConfig,
  explainQueries,
  explainRequest,
  explanationOf,
  movementLabel,
  offerOf,
  rankMovement,
  rawFrom,
  semanticTargets,
  type Config,
  type Inputs,
} from "./search-playground";

const offer = offerOf([
  { name: "body", type: "text" },
  { name: "category", type: "keyword" },
  { name: "summary", type: "semantic_text" },
  { name: "title", type: "text" },
  { name: "title.keyword", type: "keyword" },
  { name: "vec", type: "dense_vector" },
]);
const targets = semanticTargets(offer, ["e5"]);
const q: Inputs = { query: "forgot password", size: 10, vectors: { e5: [0.5, -0.25] } };

function body(cfg: Config, inputs = q) {
  const b = buildRequest(cfg, "help center", inputs);
  assert.ok(b.ok, b.ok ? "" : b.reason);
  assert.equal(b.request.path, "/help%20center/_search");
  return JSON.parse(b.request.body);
}

test("the mapping decides what each mode offers", () => {
  assert.deepEqual(offer, { text: ["body", "title"], semanticText: ["summary"], denseVector: ["vec"] });
  assert.deepEqual(targets, [
    { kind: "semantic", field: "summary" },
    { kind: "knn", field: "vec", inferenceId: "e5" },
  ]);
  // A dense_vector field without an endpoint to embed the query is no semantic target.
  assert.deepEqual(semanticTargets(offer, []), [{ kind: "semantic", field: "summary" }]);
});

test("each mode builds its request", () => {
  const base = defaultConfig("keyword", offer, targets);
  assert.deepEqual(body(base), {
    size: 10,
    query: { multi_match: { query: "forgot password", fields: ["body", "title"] } },
    highlight: { fields: { body: {}, title: {} } },
  });
  assert.deepEqual(body({ ...base, mode: "semantic" }), {
    size: 10,
    query: { semantic: { field: "summary", query: "forgot password" } },
    highlight: { fields: { summary: { type: "semantic", number_of_fragments: 2, order: "score" } } },
  });
  // A dense_vector field searches with the query's embedding, in the top-level knn section.
  assert.deepEqual(body({ ...base, mode: "semantic", semantic: targets[1] }), {
    size: 10,
    knn: { field: "vec", k: 10, num_candidates: 100, query_vector: [0.5, -0.25] },
  });
  const rrf = body({ ...base, mode: "hybrid" });
  assert.deepEqual(rrf.retriever, {
    rrf: {
      retrievers: [
        { standard: { query: { multi_match: { query: "forgot password", fields: ["body", "title"] } } } },
        { standard: { query: { semantic: { field: "summary", query: "forgot password" } } } },
      ],
      rank_window_size: 50,
    },
  });
  const linear = body({ ...base, mode: "hybrid", fusion: "linear", weights: [0.3, 2], semantic: targets[1] });
  assert.deepEqual(
    linear.retriever.linear.retrievers.map((r: { weight: number; normalizer: string }) => [r.weight, r.normalizer]),
    [
      [0.3, "minmax"],
      [2, "minmax"],
    ],
  );
  assert.deepEqual(linear.retriever.linear.retrievers[1].retriever, { knn: { field: "vec", k: 50, num_candidates: 500, query_vector: [0.5, -0.25] } });
});

test("a side that can't search yet says why", () => {
  const base = defaultConfig("keyword", offer, targets);
  assert.deepEqual(buildRequest({ ...base, fields: [] }, "x", q), { ok: false, reason: "Pick at least one text field." });
  assert.deepEqual(buildRequest({ ...base, mode: "semantic", semantic: null }, "x", q), { ok: false, reason: "Pick a semantic field." });
  assert.deepEqual(buildRequest({ ...base, mode: "semantic", semantic: targets[1] }, "x", { query: "q", size: 10 }), {
    ok: false,
    reason: "The query isn't embedded by e5 yet.",
  });
});

test("a raw body goes out as typed, with the query in place of {{query}}", () => {
  const raw = 'POST /docs/_search\n{"size": 3, "min_score": 1.0, "query": {"match": {"title": "{{query}}"}}}';
  const cfg: Config = { ...defaultConfig("raw", offer, targets), raw };
  const b = buildRequest(cfg, "ignored", { query: 'say "hi"', size: 10 });
  assert.deepEqual(b, { ok: true, request: { method: "POST", path: "/docs/_search", body: '{"size": 3, "min_score": 1.0, "query": {"match": {"title": "say \\"hi\\""}}}' } });
  assert.equal(buildRequest({ ...cfg, raw: "POST /docs/_search\n{nope" }, "x", q).ok, false);
  // Switching to Raw starts from the side's own request, with the placeholder.
  const seeded = rawFrom(defaultConfig("keyword", offer, targets), "docs", q);
  assert.match(seeded, /^POST \/docs\/_search\n/);
  assert.match(seeded, /"query": "\{\{query\}\}"/);
});

test("explain uses _explain, or a search limited to the doc for a semantic query", () => {
  const base = defaultConfig("keyword", offer, targets);
  const parts = explainQueries({ ...base, mode: "hybrid" }, q);
  assert.ok(Array.isArray(parts));
  assert.deepEqual(
    parts.map((p) => p.label),
    ["keyword", "semantic"],
  );
  const kw = explainRequest(".ds-logs-1", "a/b", parts[0].query);
  assert.equal(kw.path, "/.ds-logs-1/_explain/a%2Fb");
  assert.deepEqual(JSON.parse(kw.body), { query: parts[0].query });
  const sem = explainRequest("docs", "a", parts[1].query);
  assert.equal(sem.path, "/docs/_search");
  assert.deepEqual(JSON.parse(sem.body).query.bool.filter, [{ ids: { values: ["a"] } }]);
  assert.equal(explainQueries({ ...base, mode: "raw", raw: 'POST /d/_search\n{"knn": {}}' }, q), "Explain accounts for a body's query; this body has none.");

  const tree = { value: 2, description: "weight(title:x)", details: [] };
  assert.deepEqual(explanationOf({ matched: true, explanation: tree }), tree);
  assert.equal(explanationOf({ matched: false, explanation: { value: 0, description: "no match" } }), null);
  // The search form's id filter is left out.
  const filter = { value: 0, description: "match on required clause, product of:", details: [{ value: 1, description: "_id:([61])" }] };
  assert.deepEqual(explanationOf({ hits: { hits: [{ _explanation: { value: 2, description: "sum of:", details: [tree, filter] } }] } }), tree);
});

test("rank movement between A and B", () => {
  const m = rankMovement(["a", "b", "c", "d"], ["c", "b", "e", "a"]);
  assert.deepEqual(
    [...m.b].map(([k, v]) => [k, movementLabel(v)]),
    [
      ["c", "↑2"],
      ["b", "="],
      ["e", "new"],
      ["a", "↓3"],
    ],
  );
  assert.deepEqual([...m.a].map(([k, v]) => [k, movementLabel(v)]), [["d", "gone"]]);
});

test("copy as curl, Python and JavaScript emits the exact request without a key", () => {
  const cfg: Config = { ...defaultConfig("keyword", offer, targets), fields: ["title"] };
  const b = buildRequest(cfg, "docs", { query: "it's", size: 5 });
  assert.ok(b.ok);
  const target = { url: "https://shop.search.roll.sh/", flavor: "elasticsearch" as const };
  assert.equal(
    copyAs("curl", b.request, target),
    `curl -X POST 'https://shop.search.roll.sh/docs/_search' \\
  -H "Authorization: ApiKey $ROLL_SEARCH_API_KEY" \\
  -H 'content-type: application/json' \\
  -d '{
  "size": 5,
  "query": {
    "multi_match": {
      "query": "it'\\''s",
      "fields": [
        "title"
      ]
    }
  },
  "highlight": {
    "fields": {
      "title": {}
    }
  }
}'`,
  );
  const py = copyAs("python", b.request, target);
  assert.match(py, /client = Elasticsearch\("https:\/\/shop.search.roll.sh", api_key=os.environ\["ROLL_SEARCH_API_KEY"\]\)/);
  assert.match(py, /resp = client.search\(\n {4}index="docs",\n {4}size=5,\n {4}query=\{\n {8}"multi_match": \{/);
  const js = copyAs("javascript", b.request, target);
  assert.match(js, /auth: \{ apiKey: process.env.ROLL_SEARCH_API_KEY \}/);
  assert.match(js, /client.search\(\{\n {2}index: "docs",\n {2}size: 5,\n {2}query: \{\n {4}multi_match: \{/);

  // Python literals, `from` as the client's keyword, and a raw body's numbers as typed in curl.
  const raw = buildRequest({ ...cfg, mode: "raw", raw: 'GET /docs/_search\n{"from": 10, "track_total_hits": true, "min_score": 1.0, "post_filter": null}' }, "docs", q);
  assert.ok(raw.ok);
  const rawPy = copyAs("python", raw.request, target);
  assert.match(rawPy, /from_=10,\n {4}track_total_hits=True,\n {4}min_score=1,\n {4}post_filter=None,/);
  assert.match(copyAs("curl", raw.request, target), /-d '\{"from": 10, "track_total_hits": true, "min_score": 1.0, "post_filter": null\}'$/);

  // OpenSearch-flavor clusters get OpenSearch's clients and basic auth from the environment.
  const os = { url: "http://127.0.0.1:9200", flavor: "opensearch" as const };
  assert.match(copyAs("curl", b.request, os), /-u "\$OPENSEARCH_USERNAME:\$OPENSEARCH_PASSWORD"/);
  assert.match(copyAs("python", b.request, os), /from opensearchpy import OpenSearch[\s\S]*index="docs",\n {4}body=\{/);
  assert.match(copyAs("javascript", b.request, os), /@opensearch-project\/opensearch[\s\S]*index: "docs",\n {2}body: \{/);
  for (const lang of ["curl", "python", "javascript"] as const) assert.doesNotMatch(copyAs(lang, b.request, target), /password|secret/i);
});
