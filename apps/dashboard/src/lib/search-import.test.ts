// Tests of the dashboard's file import (vitest).
import assert from "node:assert/strict";
import { test } from "vitest";
import { BulkBatcher, bulkFailures, bulkLines, columnTypes, csvDocument, csvHeader, CsvReader, detectFormat, indexNameFrom, indexNameProblem, jsonDocuments, NdjsonReader } from "./search-import";

/** Feeds `text` to a reader in chunks of `n` characters. */
function chunked<T>(text: string, n: number, push: (t: string) => T[], end: () => T[]): T[] {
  const out: T[] = [];
  for (let i = 0; i < text.length; i += n) out.push(...push(text.slice(i, i + n)));
  return [...out, ...end()];
}

test("files read in chunks give the same documents as read whole", () => {
  const csv = '﻿sku;name;price;in_stock;zip\r\nA-1;"Shoe; ""trail""\nrunner";89.5;true;007\r\nA-2;Sock;3;false;10115\r\nA-3;;12;;\r\n';
  for (const n of [1, 2, 3, 7, csv.length]) {
    const r = new CsvReader();
    const recs = chunked(csv, n, (t) => r.push(t), () => r.end());
    assert.equal(r.delimiter, ";");
    assert.deepEqual(
      recs.map((x) => x.at),
      [1, 2, 4, 5],
      `chunks of ${n}`,
    );
    const header = csvHeader(recs[0].cells);
    const rows = recs.slice(1).map((x) => x.cells);
    const types = columnTypes(rows, header.length);
    assert.deepEqual(types, ["string", "string", "double", "boolean", "string"]);
    assert.deepEqual(csvDocument(header, types, rows[0]), { sku: "A-1", name: 'Shoe; "trail"\nrunner', price: 89.5, in_stock: true, zip: "007" });
    assert.deepEqual(csvDocument(header, types, rows[2]), { sku: "A-3", price: 12 });
  }
  assert.deepEqual(csvHeader(["a", "", "a"]), ["a", "column_2", "a_2"]);

  const nd = '{"a":1}\n\n{"b":"x\\ny"}\nnot json\n[1]\n{"c":true}';
  for (const n of [1, 5, nd.length]) {
    const r = new NdjsonReader();
    assert.deepEqual(
      chunked(nd, n, (t) => r.push(t), () => r.end()),
      [{ a: 1 }, { b: "x\ny" }, { c: true }],
    );
    assert.deepEqual(
      r.errors.map((e) => e.at),
      [4, 5],
    );
    assert.equal(r.errorCount, 2);
  }

  assert.deepEqual(jsonDocuments('{"products":[{"a":1},{"a":2}],"total":2}').docs, [{ a: 1 }, { a: 2 }]);
  assert.deepEqual(jsonDocuments('[{"a":1},2]').errors, [{ at: 2, message: "not a JSON object" }]);
  assert.equal(detectFormat("x.json", '{"a":1}\n{"a":2}'), "ndjson");
  assert.equal(detectFormat("x.json", '{\n  "a": 1\n}'), "json");
  assert.equal(detectFormat("data.txt", "a,b\n1,2"), "csv");
});

test("bulk batches stay under the byte limit and report failures by document", () => {
  const b = new BulkBatcher(130);
  const lines = bulkLines({ id: 7, t: "x".repeat(20) }, "id");
  assert.equal(lines.split("\n")[0], '{"index":{"_id":"7"}}');
  assert.equal(bulkLines({ t: 1 }, "id").split("\n")[0], '{"index":{}}');
  const batches = [b.add(lines), b.add(lines), b.add(lines), b.take(), b.take()];
  assert.deepEqual(
    batches.map((x) => x?.docs ?? 0),
    [0, 0, 2, 1, 0],
  );
  assert.ok(batches.every((x) => !x || new TextEncoder().encode(x.body).length <= 130));
  assert.deepEqual(bulkFailures({ errors: true, items: [{ index: { status: 201 } }, { index: { status: 400, error: { type: "document_parsing_exception", reason: "bad" } } }] }), [{ at: 1, status: 400, reason: "document_parsing_exception: bad" }]);

  assert.equal(indexNameFrom("My Products (2026).CSV"), "my-products-(2026)");
  assert.equal(indexNameProblem("my-products-(2026)"), null);
  assert.equal(indexNameFrom("_.json"), "import");
  assert.notEqual(indexNameProblem("Products"), null);
  assert.notEqual(indexNameProblem("_x"), null);
});
