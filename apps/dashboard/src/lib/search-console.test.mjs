// Tests of the console completion (`node --test`; Node 23.6+ runs the TypeScript source as is).
// Run: node --test apps/dashboard/src/lib/search-console.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";
import { applyCompletion, bodyKind, complete, jsonContext, mappingFields, ndjsonRole, parseConsole, requestIndex } from "./search-console.ts";

const FIELDS = [
  { name: "price", type: "float" },
  { name: "tags", type: "keyword" },
  { name: "title", type: "text" },
  { name: "title.keyword", type: "keyword" },
  { name: "user", type: "nested" },
  { name: "user.name", type: "keyword" },
];
const SRC = { indices: ["products", "logs-2026", ".internal"], fields: FIELDS };

/** Completes at the `|` in `t`. */
function at(t, src = SRC) {
  const i = t.indexOf("|");
  return { text: t.slice(0, i) + t.slice(i + 1), cursor: i, c: complete(t.slice(0, i) + t.slice(i + 1), i, src) };
}
const labels = (c) => (c ? c.items.map((s) => s.label) : []);

/** Applies the first suggestion named `label` and returns the text with `|` at the cursor. */
function choose(t, label) {
  const { text, c } = at(t);
  assert.ok(c, `no completion at ${t}`);
  const item = c.items.find((s) => s.label === label);
  assert.ok(item, `${label} not in ${labels(c).join(",")}`);
  const r = applyCompletion(text, c, item);
  return r.text.slice(0, r.cursor) + "|" + r.text.slice(r.cursor);
}

test("json context", () => {
  assert.deepEqual(jsonContext("", 0).path, []);
  const c = jsonContext('{"query":{"bool":{"must":[{"ma', 32);
  assert.deepEqual([c.path, c.inKey, c.prefix, c.quoted], [["query", "bool", "must", "[]"], true, "ma", true]);
  // Closed containers and commas reset the key.
  const d = jsonContext('{"query":{"match_all":{}},"si', 29);
  assert.deepEqual([d.path, d.inKey, d.prefix], [[], true, "si"]);
  // A value string knows its key.
  const e = jsonContext('{"aggs":{"t":{"terms":{"field":"ti', 34);
  assert.deepEqual([e.inString, e.valueOf, e.prefix, e.path], [true, "field", "ti", ["aggs", "t", "terms"]]);
  // Escapes and braces inside strings don't confuse it.
  const f = jsonContext('{"q":"a\\"{[b","', 15);
  assert.deepEqual([f.path, f.inKey, f.prefix], [[], true, ""]);
  // Unquoted words.
  const g = jsonContext("{\n  qu", 6);
  assert.deepEqual([g.inKey, g.quoted, g.prefix, g.tokenStart], [true, false, "qu", 4]);
  // Garbage never throws.
  jsonContext("}}]]::,,{", 9);
});

test("first line: methods and paths", () => {
  assert.deepEqual(labels(at("G|").c), ["GET"]);
  assert.equal(choose("po|", "POST"), "POST|");
  // Indices and root APIs; hidden indices only when asked for.
  const root = labels(at("GET /|").c);
  assert.ok(root.includes("products") && root.includes("_search") && !root.includes(".internal"));
  assert.deepEqual(labels(at("GET /.|").c), [".internal"]);
  assert.deepEqual(labels(at("GET /pro|").c), ["products"]);
  assert.equal(choose("GET /pro|", "products"), "GET /products|");
  // Under an index.
  assert.ok(labels(at("GET /products/_se|").c).includes("_search"));
  assert.equal(choose("GET /products/_se|?q=x", "_search"), "GET /products/_search|?q=x");
  // Namespaces.
  assert.equal(choose("GET /_ca|", "_cat/"), "GET /_cat/|");
  assert.ok(labels(at("GET /_cat/|").c).includes("indices"));
  assert.deepEqual(labels(at("GET /_cluster/he|").c), ["health"]);
  // The query string and deeper paths have nothing.
  assert.equal(at("GET /_search?|").c, null);
  assert.equal(at("GET /products/_doc/|").c, null);
  // Replaces the whole segment under the cursor.
  assert.equal(choose("GET /_cat/ind|ices", "indices"), "GET /_cat/indices|");
  // Fuzzy (contains) matches come after prefix matches.
  assert.ok(labels(at("GET /_cat/al|").c).includes("allocation"));
});

test("search bodies", () => {
  assert.ok(labels(at('GET /products/_search\n{\n  "|').c).includes("query"));
  assert.equal(choose('GET /products/_search\n{\n  "qu|', "query"), 'GET /products/_search\n{\n  "query": {|}');
  // Unquoted.
  assert.equal(choose("GET /products/_search\n{ qu|", "query"), 'GET /products/_search\n{ "query": {|}');
  // Query types where a query goes.
  assert.ok(labels(at('GET /_search\n{"query":{"|').c).includes("bool"));
  assert.ok(labels(at('GET /_search\n{"query":{"bool":{"must":[{"|').c).includes("match"));
  assert.ok(labels(at('GET /_search\n{"query":{"bool":{"filter":{"te|').c).includes("term"));
  assert.ok(labels(at('GET /_search\n{"query":{"bool":{"|').c).includes("must_not"));
  // Fields under field-keyed queries, then their parameters.
  assert.deepEqual(labels(at('GET /_search\n{"query":{"match":{"ti|').c), ["title", "title.keyword"]);
  assert.ok(labels(at('GET /_search\n{"query":{"match":{"title":{"|').c).includes("operator"));
  assert.ok(labels(at('GET /_search\n{"query":{"range":{"price":{"g|').c).includes("gte"));
  // Aggregations: types under a name, parameters under a type, fields as values.
  assert.ok(labels(at('GET /_search\n{"aggs":{"by_tag":{"te|').c).includes("terms"));
  assert.ok(labels(at('GET /_search\n{"aggs":{"by_tag":{"terms":{"|').c).includes("field"));
  assert.deepEqual(labels(at('GET /_search\n{"aggs":{"by_tag":{"terms":{"field":"ta|').c), ["tags"]);
  assert.ok(labels(at('GET /_search\n{"aggs":{"a":{"terms":{"field":"tags"},"aggs":{"b":{"av|').c).includes("avg"));
  // The filter aggregation's body is a query.
  assert.ok(labels(at('GET /_search\n{"aggs":{"f":{"filter":{"te|').c).includes("term"));
  // Sort: fields, then options, then values.
  assert.ok(labels(at('GET /_search\n{"sort":[{"pr|').c).includes("price"));
  assert.ok(labels(at('GET /_search\n{"sort":[{"price":{"|').c).includes("order"));
  assert.deepEqual(labels(at('GET /_search\n{"sort":[{"price":{"order":"|').c), ["asc", "desc"]);
  assert.equal(choose('GET /_search\n{"sort":[{"price":{"order":"d|"}}]}', "desc"), 'GET /_search\n{"sort":[{"price":{"order":"desc"|}}]}');
  // Field lists.
  assert.deepEqual(labels(at('GET /_search\n{"_source":["us|').c), ["user", "user.name"]);
  assert.ok(labels(at('GET /_search\n{"query":{"multi_match":{"fields":["t|').c).includes("title"));
  // Nested paths only offer nested fields.
  assert.deepEqual(labels(at('GET /_search\n{"query":{"nested":{"path":"|').c), ["user"]);
  // Booleans.
  assert.equal(choose('GET /_search\n{"track_total_hits": tr|', "true"), 'GET /_search\n{"track_total_hits": true|');
  // Renaming a key keeps its value.
  assert.equal(choose('GET /_search\n{"si|": 10}', "size"), 'GET /_search\n{"size"|: 10}');
});

test("other bodies", () => {
  assert.deepEqual(labels(at('PUT /products\n{"|').c).sort(), ["aliases", "mappings", "settings"]);
  assert.ok(labels(at('PUT /products\n{"mappings":{"properties":{"title":{"|').c).includes("type"));
  assert.ok(labels(at('PUT /products\n{"mappings":{"properties":{"title":{"type":"key|').c).includes("keyword"));
  assert.ok(labels(at('PUT /products/_mapping\n{"properties":{"t":{"fields":{"raw":{"type":"k|').c).includes("keyword"));
  assert.ok(labels(at('PUT /products\n{"settings":{"number|').c).includes("number_of_shards"));
  assert.ok(labels(at('PUT /_index_template/t\n{"|').c).includes("index_patterns"));
  assert.ok(labels(at('PUT /_index_template/t\n{"template":{"mappings":{"properties":{"a":{"ty|').c).includes("type"));
  // Documents: top-level field names.
  assert.deepEqual(labels(at('PUT /products/_doc/1\n{"ti|').c), ["title"]);
  assert.ok(labels(at('POST /products/_update/1\n{"|').c).includes("doc"));
  assert.ok(labels(at('POST /products/_update/1\n{"doc":{"pr|').c).includes("price"));
  assert.ok(labels(at('POST /products/_delete_by_query\n{"query":{"|').c).includes("match_all"));
  assert.ok(labels(at('POST /_reindex\n{"source":{"|').c).includes("index"));
  assert.ok(labels(at('POST /_analyze\n{"analyzer":"st|').c).includes("standard"));
  // A first line that isn't a request: nothing.
  assert.equal(at('hello\n{"|').c, null);
});

test("ndjson bodies", () => {
  const bulk = 'POST /_bulk\n{"index":{"_index":"products"}}\n{"title":"a"}\n{"delete":{"_id":"1"}}\n';
  assert.ok(labels(at(`${bulk}{"|`).c).includes("index"));
  assert.ok(labels(at(`${bulk}{"update":{"_|`).c).includes("_id"));
  assert.deepEqual(labels(at(`${bulk}{"update":{"_id":"2"}}\n{"|`).c).slice(0, 2), ["doc", "script"]);
  assert.deepEqual(labels(at('POST /products/_bulk\n{"index":{}}\n{"pri|').c), ["price"]);
  assert.ok(labels(at('GET /_msearch\n{"|').c).includes("index"));
  assert.ok(labels(at('GET /_msearch\n{}\n{"|').c).includes("query"));
  assert.equal(ndjsonRole("bulk", ['{"delete":{}}', '{"index":{}}'], 2), "doc");
  assert.equal(ndjsonRole("bulk", ['{"create":{}}', "{}"], 2), "action");
  assert.equal(ndjsonRole("msearch", [], 3), "search");
});

test("helpers", () => {
  assert.equal(bodyKind("PUT", "/products"), "index");
  assert.equal(bodyKind("GET", "/products"), null);
  assert.equal(bodyKind("POST", "/a,b/_search?size=1"), "search");
  assert.equal(bodyKind("PUT", "/products/_doc/1"), "doc");
  assert.equal(bodyKind("POST", "/_bulk"), "bulk");
  assert.equal(requestIndex("/products/_search"), "products");
  assert.equal(requestIndex("/logs%2A/_search"), "logs*");
  assert.equal(requestIndex("/_search"), null);
  assert.equal(requestIndex("/"), null);
  const fields = mappingFields({
    a: { mappings: { properties: { title: { type: "text", fields: { keyword: { type: "keyword" } } }, user: { properties: { name: { type: "keyword" } } } }, runtime: { day: { type: "keyword" } } } },
    b: { mappings: { properties: { title: { type: "text" }, n: { type: "long" } } } },
    c: { mappings: {} },
  });
  assert.deepEqual(fields, [
    { name: "day", type: "keyword" },
    { name: "n", type: "long" },
    { name: "title", type: "text" },
    { name: "title.keyword", type: "keyword" },
    { name: "user", type: "object" },
    { name: "user.name", type: "keyword" },
  ]);
  assert.deepEqual(mappingFields(null), []);
});

test("console requests keep their body text", () => {
  // Verified on Elasticsearch 9.5.5: {"price":1.0} maps price as float, {"price":1} as long.
  const doc = '{"title":"x","price":1.0,"sku":12345678901234567890,"b":1,"10":2,"2":3}';
  assert.deepEqual(parseConsole(`PUT /shop/_doc/1\n${doc}\n`), { method: "PUT", path: "/shop/_doc/1", body: doc, ndjson: false });
  assert.deepEqual(parseConsole("  get _cat/indices?v"), { method: "GET", path: "/_cat/indices?v", ndjson: false });
  const bulk = '{"index":{"_index":"a"}}\n{"n":1.0}';
  assert.deepEqual(parseConsole(`POST /_bulk\n${bulk}`), { method: "POST", path: "/_bulk", body: `${bulk}\n`, ndjson: true });
  assert.equal(parseConsole("POST /a/_msearch/template?x=1\n{}\n{}").ndjson, true);
  assert.throws(() => parseConsole("PUT /a\n{nope"), /isn't valid JSON/);
  assert.throws(() => parseConsole("FETCH /a"), /Unsupported method/);
  assert.throws(() => parseConsole("/a"), /Start with a method/);
});

test("URL query parameters", async () => {
  const { complete, applyCompletion, apiParams } = await import("./search-console.ts");
  const { PARAMS, COMMON_PARAMS, API_PATHS } = await import("./search-params.generated.ts");
  const params = { params: PARAMS, common: COMMON_PARAMS, apis: API_PATHS };
  const src = { indices: ["products"], fields: [], params };
  const names = (c) => c.items.map((i) => i.label);
  // The search API's parameters and the common ones.
  const search = apiParams("GET", "/products/_search", params).map((p) => p[0]);
  for (const n of ["size", "from", "q", "track_total_hits", "pretty", "filter_path", "expand_wildcards"]) assert.ok(search.includes(n), n);
  assert.ok(!search.includes("op_type"));
  // A document write: refresh, routing, op_type.
  const put = apiParams("PUT", "/products/_doc/1", params).map((p) => p[0]);
  for (const n of ["refresh", "routing", "op_type", "if_seq_no"]) assert.ok(put.includes(n), n);
  // _cat APIs: v, h, s, format.
  const cat = apiParams("GET", "/_cat/indices", params).map((p) => p[0]);
  for (const n of ["v", "h", "s", "format", "health"]) assert.ok(cat.includes(n), n);
  // An unknown path still offers the common parameters.
  assert.deepEqual(apiParams("GET", "/nope/_nothing/a/b/c", params).map((p) => p[0]).sort(), ["error_trace", "filter_path", "human", "pretty", "source"]);

  let line = "GET /products/_search?si";
  let c = complete(line, line.length, src);
  assert.ok(names(c).includes("size"), names(c).join());
  let r = applyCompletion(line, c, c.items.find((i) => i.label === "size"));
  assert.equal(r.text, "GET /products/_search?size=");
  // Boolean parameters go in bare (Elasticsearch reads an empty value as true).
  line = "GET /products/_search?size=5&pre";
  c = complete(line, line.length, src);
  r = applyCompletion(line, c, c.items.find((i) => i.label === "pretty"));
  assert.equal(r.text, "GET /products/_search?size=5&pretty");
  // Parameters already given aren't offered again.
  line = "GET /products/_search?size=5&";
  c = complete(line, line.length, src);
  assert.ok(!names(c).includes("size") && names(c).includes("from"));
  // Values: enums and booleans.
  line = "PUT /products/_doc/1?refresh=";
  c = complete(line, line.length, src);
  assert.deepEqual(names(c).sort(), ["false", "true", "wait_for"]);
  line = "GET /products/_search?expand_wildcards=open,hi";
  c = complete(line, line.length, src);
  assert.deepEqual(names(c), ["hidden"]);
  r = applyCompletion(line, c, c.items[0]);
  assert.equal(r.text, "GET /products/_search?expand_wildcards=open,hidden");
  line = "GET /products/_search?track_scores=";
  c = complete(line, line.length, src);
  assert.deepEqual(names(c), ["true", "false"]);
  // Free-form values get nothing; neither does a body line.
  line = "GET /products/_search?q=";
  assert.equal(complete(line, line.length, src), null);
  // In the middle of the line: replaces the name only.
  line = "GET /products/_search?siz&from=1 ";
  c = complete(line, "GET /products/_search?siz".length, src);
  r = applyCompletion(line, c, c.items.find((i) => i.label === "size"));
  assert.equal(r.text, "GET /products/_search?size=&from=1 ");
  // Without a table, nothing (the old behaviour).
  assert.equal(complete("GET /_search?si", 15, { indices: [], fields: [] }), null);
});
