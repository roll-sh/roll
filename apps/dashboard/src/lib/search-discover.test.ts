import { describe, expect, it } from "vitest";
import { esqlText, hashToState, initialState, pplText, sampleBody, searchBody, stateToHash, viewFields, valuesField, withFilter, type DiscoverState } from "./search-discover";

const NOW = Date.UTC(2026, 10, 26, 12, 0, 0);
const base: DiscoverState = { ...initialState("elasticsearch"), view: "logs-*", timeField: "@timestamp", query: "status >= 500", range: { last: "15m" } };

describe("Discover's requests", () => {
  it("filters by the KQL text and the range, newest first, with the histogram on the first page", () => {
    expect(searchBody(base)).toEqual({
      query: { bool: { filter: [{ kql: { query: "status >= 500" } }, { range: { "@timestamp": { gte: "now-15m" } } }] } },
      from: 0,
      size: 50,
      track_total_hits: true,
      sort: [{ "@timestamp": { order: "desc", unmapped_type: "date" } }],
      aggs: { over_time: { auto_date_histogram: { field: "@timestamp", buckets: 60 } } },
    });
    const page2 = searchBody({ ...base, query: "", timeField: null, sort: { column: "host", field: "host", order: "asc" } }, 50);
    expect(page2).toEqual({ query: { bool: { filter: [{ match_all: {} }] } }, from: 50, size: 50, track_total_hits: true, sort: [{ host: { order: "asc", missing: "_last" } }, "_doc"] });
  });

  it("writes the view and range out in ES|QL and PPL, the newest rows when nothing follows", () => {
    expect(esqlText({ ...base, query: "WHERE status >= 500" }, NOW)).toBe('FROM logs-*\n| WHERE `@timestamp` >= TO_DATETIME("2026-11-26T11:45:00.000Z")\n| WHERE status >= 500');
    expect(esqlText({ ...base, query: "", range: { gte: NOW - 60_000, lt: NOW } }, NOW)).toBe(
      'FROM logs-*\n| WHERE `@timestamp` >= TO_DATETIME("2026-11-26T11:59:00.000Z") AND `@timestamp` < TO_DATETIME("2026-11-26T12:00:00.000Z")\n| SORT `@timestamp` DESC\n| LIMIT 100',
    );
    expect(esqlText({ ...base, query: "| STATS n = COUNT(*)", range: { last: null } }, NOW)).toBe("FROM logs-*\n| STATS n = COUNT(*)");
    expect(pplText({ ...base, query: "" }, NOW)).toBe("source=logs-* | where `@timestamp` >= '2026-11-26 11:45:00' | sort - `@timestamp` | head 100");
  });

  it("samples about 500 documents of the view for a field's top 5 values", () => {
    expect(sampleBody(base, "host.keyword")).toEqual({
      size: 0,
      query: searchBody(base).query,
      aggs: { sample: { sampler: { shard_size: 500 }, aggs: { top: { terms: { field: "host.keyword", size: 5 } } } } },
    });
    const fields = viewFields({
      fields: {
        _id: { _id: { type: "_id", metadata_field: true, aggregatable: true } },
        message: { text: { type: "text", aggregatable: false } },
        "message.keyword": { keyword: { type: "keyword", aggregatable: true } },
        status: { long: { type: "long", aggregatable: true }, keyword: { type: "keyword", aggregatable: true } },
      },
    });
    expect(fields.map((f) => [f.name, f.type, f.conflict])).toEqual([
      ["message", "text", false],
      ["message.keyword", "keyword", false],
      ["status", "long", true],
    ]);
    expect([valuesField(fields, "message"), valuesField(fields, "status"), valuesField(fields, "nope")]).toEqual(["message.keyword", "status", null]);
  });

  it("adds a value in or out to the query, escaped", () => {
    expect(withFilter("kql", "", "host", "web-1", false)).toBe('host: "web-1"');
    expect(withFilter("kql", "a or b", "msg", 'say "hi" \\', true)).toBe('(a or b) and not msg: "say \\"hi\\" \\\\"');
    expect(withFilter("dql", "", "my field", 3, false)).toBe('my\\ field: "3"');
    expect(withFilter("esql", "", "status", 500, true)).toBe("WHERE `status` != 500");
    expect(withFilter("ppl", "where a > 1", "host", "it's", false)).toBe("where a > 1\n| where `host` = 'it\\'s'");
  });
});

describe("Discover's URL", () => {
  it("keeps the view, time field, mode, query, range, columns and sort", () => {
    const s: DiscoverState = { ...base, columns: ["host", "message"], sort: { column: "host", field: "host.keyword", order: "desc" }, range: { gte: NOW - 1000, lt: NOW } };
    const hash = stateToHash(s);
    expect(hash.startsWith("#discover?")).toBe(true);
    expect(hashToState(hash, initialState("elasticsearch"), ["kql", "esql"])).toEqual(s);
    expect(hashToState(stateToHash({ ...base, timeField: null, range: { last: null } }), initialState("elasticsearch"), ["kql", "esql"])).toEqual({ ...base, timeField: null, range: { last: null } });
  });

  it("falls back on what it doesn't hold, or holds wrongly", () => {
    const def = initialState("opensearch");
    expect(hashToState("#indices", def, ["dql", "ppl"])).toEqual(def);
    expect(hashToState("#discover?view=x&mode=esql&range=9-3", def, ["dql", "ppl"])).toEqual({ ...def, view: "x" });
  });
});
