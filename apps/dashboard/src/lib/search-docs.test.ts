// Tests of the index browser's document helpers (vitest).
import assert from "node:assert/strict";
import { test } from "vitest";
import { cellText, columnsOf, deleteRequest, flatten, formatJson, hitSources, jsonSpans, parseDocument, sourceCells, tableColumns, writeRequest } from "./search-docs";

test("flatten", () => {
  assert.deepEqual([...flatten({ a: 1, u: { name: "x", geo: { lat: 1 } }, tags: ["a", "b"], e: {}, n: null })], [
    ["a", 1],
    ["u.name", "x"],
    ["u.geo.lat", 1],
    ["tags", ["a", "b"]],
    ["e", {}],
    ["n", null],
  ]);
  assert.deepEqual([...flatten(undefined)], []);
  assert.deepEqual([...flatten("scalar")], []);
});

test("columns", () => {
  const docs = [{ title: "a", price: 1 }, { title: "b", tags: ["x"] }, { title: "c", price: 2, user: { name: "u" } }];
  assert.deepEqual(tableColumns(docs), ["title", "price", "tags", "user.name"]);
  assert.deepEqual(tableColumns(docs, 2), ["title", "price"]);
  assert.deepEqual(tableColumns([]), []);
  assert.deepEqual(tableColumns([undefined, null]), []);
});

test("cells", () => {
  assert.equal(cellText("plain"), "plain");
  assert.equal(cellText(3.5), "3.5");
  assert.equal(cellText(["a", 1]), '["a",1]');
  assert.equal(cellText(null), "null");
  assert.equal(cellText(undefined), "");
  assert.equal(cellText("x".repeat(10), 4), "xxxx…");
});

test("documents typed by hand", () => {
  // The text is sent as typed: parsing would turn 1.0 into 1 (mapped as long, not float), round
  // the long and move the integer-like keys first.
  const typed = '{"title":"x","price":1.0,"sku":12345678901234567890,"b":1,"10":2,"2":3}';
  assert.equal(parseDocument(` ${typed}\n`), typed);
  assert.equal(parseDocument(' { "a": 1 } '), '{ "a": 1 }');
  assert.throws(() => parseDocument("[1]"), /JSON object/);
  assert.throws(() => parseDocument("null"), /JSON object/);
  assert.throws(() => parseDocument("{"), /Not valid JSON/);
});

test("write requests", () => {
  assert.deepEqual(writeRequest("books", null), { method: "POST", path: "/books/_doc?refresh=wait_for" });
  assert.deepEqual(writeRequest("books", ""), { method: "POST", path: "/books/_doc?refresh=wait_for" });
  assert.deepEqual(writeRequest("books", "a/b c", { create: true }), { method: "PUT", path: "/books/_doc/a%2Fb%20c?refresh=wait_for&op_type=create" });
  assert.deepEqual(writeRequest("books", "1", { routing: "r1" }), { method: "PUT", path: "/books/_doc/1?refresh=wait_for&routing=r1" });
  assert.deepEqual(deleteRequest("my index", "1"), { method: "DELETE", path: "/my%20index/_doc/1?refresh=wait_for" });
  assert.deepEqual(deleteRequest("b", "1", "r"), { method: "DELETE", path: "/b/_doc/1?refresh=wait_for&routing=r" });
});

// Verified on Elasticsearch 9.5.5: {"price":1.0} maps price as float, {"price":1} as long; so
// every helper below keeps the text as written.
const LOSSY = '{"title":"x","price":1.0,"sku":12345678901234567890,"b":1,"10":2,"2":3,"e":-1.5e+300,"s":"q\\"[{,:"}';

test("json spans", () => {
  const root = jsonSpans(` ${LOSSY} `);
  assert.equal(root.kind, "object");
  assert.ok(root.entries);
  assert.deepEqual(root.entries.map(([k]) => k), ["title", "price", "sku", "b", "10", "2", "e", "s"]);
  const sku = root.entries[2][1];
  assert.equal(` ${LOSSY} `.slice(sku.start, sku.end), "12345678901234567890");
  assert.equal(root.entries[7][1].kind, "string");
  assert.deepEqual(jsonSpans("[]"), { start: 0, end: 2, kind: "array", items: [] });
  assert.equal(jsonSpans("true").kind, "literal");
  for (const bad of ["", "{", '{"a"}', "[1,]", "01", "{} x", '"open', "nul", '{"a":1,}']) {
    assert.throws(() => jsonSpans(bad), Error, bad);
  }
});

test("format json without re-encoding", () => {
  assert.equal(
    formatJson(LOSSY),
    '{\n  "title": "x",\n  "price": 1.0,\n  "sku": 12345678901234567890,\n  "b": 1,\n  "10": 2,\n  "2": 3,\n  "e": -1.5e+300,\n  "s": "q\\"[{,:"\n}',
  );
  assert.equal(formatJson(formatJson(LOSSY), 0), LOSSY);
  assert.equal(formatJson('{ "a" : [ 1 , { } , [ ] ] }', 0), '{"a":[1,{},[]]}');
  assert.equal(formatJson('{"a":[1,{"b":null}]}'), '{\n  "a": [\n    1,\n    {\n      "b": null\n    }\n  ]\n}');
  // Not JSON (text APIs, truncated bodies): unchanged.
  assert.equal(formatJson("health status index\n"), "health status index\n");
  assert.equal(formatJson('{"a":'), '{"a":');
});

test("hit sources are cut from the response text", () => {
  const resp = `{"took":1,"hits":{"total":{"value":3,"relation":"eq"},"hits":[{"_index":"i","_id":"1","_source":${LOSSY}},{"_index":"i","_id":"2"},{"_id":"3","_source" : { }}]}}`;
  assert.deepEqual(hitSources(resp), [LOSSY, undefined, "{ }"]);
  assert.deepEqual(hitSources('{"hits":{"hits":[]}}'), []);
  assert.deepEqual(hitSources("not json"), []);
  assert.deepEqual(hitSources('{"error":{}}'), []);
});

test("source cells are lossless", () => {
  const cells = sourceCells('{"t":"a\\nb","n":12345678901234567890,"f":1.0,"u":{"name":"x","e":{}},"tags":[1, 2.50],"z":null}');
  assert.deepEqual([...cells], [
    ["t", "a\nb"],
    ["n", "12345678901234567890"],
    ["f", "1.0"],
    ["u.name", "x"],
    ["u.e", "{}"],
    ["tags", "[1,2.50]"],
    ["z", "null"],
  ]);
  assert.deepEqual([...sourceCells(undefined)], []);
  assert.deepEqual([...sourceCells("{bad")], []);
  // Same paths as flatten() for what JSON.parse represents exactly.
  const doc = { a: 1, u: { name: "x", geo: { lat: 1 } }, tags: ["a"], e: {}, n: null };
  assert.deepEqual([...sourceCells(JSON.stringify(doc)).keys()], [...flatten(doc).keys()]);
  assert.deepEqual(columnsOf([sourceCells('{"a":1,"b":2}').keys(), sourceCells('{"b":3}').keys()]), ["b", "a"]);
});

test("sort fields", async () => {
  const { sortField } = await import("./search-docs");
  const fields = [
    { name: "title", type: "text" },
    { name: "title.keyword", type: "keyword" },
    { name: "body", type: "text" },
    { name: "price", type: "scaled_float" },
    { name: "at", type: "date" },
    { name: "user", type: "object" },
    { name: "user.name", type: "keyword" },
    { name: "loc", type: "geo_point" },
    { name: "tags", type: "flattened" },
  ];
  assert.equal(sortField("title", fields), "title.keyword");
  assert.equal(sortField("body", fields), null);
  assert.equal(sortField("price", fields), "price");
  assert.equal(sortField("at", fields), "at");
  assert.equal(sortField("user", fields), null);
  assert.equal(sortField("user.name", fields), "user.name");
  assert.equal(sortField("loc", fields), null);
  assert.equal(sortField("tags", fields), null);
  assert.equal(sortField("unmapped", fields), null);
});

test("table sorts cycle and build Elasticsearch's sort clause", async () => {
  const { nextSort, sortClause } = await import("./search-docs");
  const a = nextSort(null, "title", "title.keyword");
  assert.deepEqual(a, { column: "title", field: "title.keyword", order: "asc" });
  const d = nextSort(a, "title", "title.keyword");
  assert.ok(d);
  assert.equal(d.order, "desc");
  assert.equal(nextSort(d, "title", "title.keyword"), null);
  assert.deepEqual(nextSort(d, "price", "price"), { column: "price", field: "price", order: "asc" });
  assert.equal(sortClause(null), undefined);
  assert.deepEqual(sortClause(d), [{ "title.keyword": { order: "desc", missing: "_last" } }, "_doc"]);
});

test("column picker", async () => {
  const { visibleColumns, toggleColumn } = await import("./search-docs");
  const avail = ["a", "b", "c", "d"];
  assert.deepEqual(visibleColumns(null, avail, 2), ["a", "b"]);
  assert.deepEqual(visibleColumns([], avail, 2), ["a", "b"]);
  assert.deepEqual(visibleColumns(["d", "x"], avail), ["d", "x"]);
  assert.deepEqual(toggleColumn(["a", "b"], "b"), ["a"]);
  assert.deepEqual(toggleColumn(["a"], "c"), ["a", "c"]);
});
