import { describe, expect, it } from "vitest";
import {
  addFilter,
  anchorBody,
  chartBody,
  chartData,
  chartOf,
  edited,
  esqlText,
  fromSpec,
  hashSaved,
  hashToState,
  initialState,
  localTime,
  pplText,
  sampleBody,
  searchBody,
  stateToHash,
  surroundingBodies,
  toSpec,
  viewFields,
  valuesField,
  withFilter,
  type DiscoverState,
} from "./search-discover";

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
      fields: [{ field: "@timestamp", format: "epoch_millis" }],
      aggs: { over_time: { auto_date_histogram: { field: "@timestamp", buckets: 60, time_zone: "UTC" } } },
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

  it("adds a value in or out as a filter pill in KQL and DQL, or a WHERE line in a piped query", () => {
    const host = { field: "host", value: "web-1", negate: false };
    const filters = addFilter(addFilter([], host), { field: "status", value: 404, negate: true });
    expect(searchBody({ ...base, filters }).query).toEqual({
      bool: {
        filter: [{ kql: { query: "status >= 500" } }, { range: { "@timestamp": { gte: "now-15m" } } }, { match_phrase: { host: "web-1" } }],
        must_not: [{ match_phrase: { status: 404 } }],
      },
    });
    // The same value again flips the pill instead of adding one.
    expect(addFilter(filters, { ...host, negate: true })).toEqual([{ ...host, negate: true }, filters[1]]);
    expect(withFilter("kql", "", "host", "web-1", false)).toBe('host: "web-1"');
    expect(withFilter("kql", "a or b", "msg", 'say "hi" \\', true)).toBe('(a or b) and not msg: "say \\"hi\\" \\\\"');
    expect(withFilter("dql", "", "my field", 3, false)).toBe('my\\ field: "3"');
    expect(withFilter("esql", "", "status", 500, true)).toBe("WHERE `status` != 500");
    expect(withFilter("ppl", "where a > 1", "host", "it's", false)).toBe("where a > 1\n| where `host` = 'it\\'s'");
  });
});

describe("Discover's URL", () => {
  it("keeps the view, time field, mode, query, filters, range, columns and sort, and the saved search", () => {
    const s: DiscoverState = { ...base, filters: [{ field: "a:b", value: 'x "y"', negate: true }], columns: ["host", "message"], sort: { column: "host", field: "host.keyword", order: "desc" }, range: { gte: NOW - 1000, lt: NOW } };
    const hash = stateToHash(s, "ss_1");
    expect(hash.startsWith("#discover?saved=ss_1&")).toBe(true);
    expect(hashToState(hash, initialState("elasticsearch"), ["kql", "esql"])).toEqual(s);
    expect([hashSaved(hash), hashSaved("#discover?saved=ss_1"), hashSaved(stateToHash(s))]).toEqual([{ id: "ss_1", ownView: true }, { id: "ss_1", ownView: false }, null]);
    expect(hashToState(stateToHash({ ...base, timeField: null, range: { last: null } }), initialState("elasticsearch"), ["kql", "esql"])).toEqual({ ...base, timeField: null, range: { last: null } });
  });

  it("falls back on what it doesn't hold, or holds wrongly", () => {
    const def = initialState("opensearch");
    expect(hashToState("#indices", def, ["dql", "ppl"])).toEqual(def);
    expect(hashToState("#discover?view=x&mode=esql&range=9-3&f=nope&f=%5B%22a%22%2C%7B%7D%2Cfalse%5D", def, ["dql", "ppl"])).toEqual({ ...def, view: "x" });
  });
});

describe("Discover's saved searches", () => {
  it("keep the view as the control plane checks it, and say when the view moved on", () => {
    const spec = toSpec({ ...base, columns: ["host"] });
    expect(spec).toEqual({ view: "logs-*", time_field: "@timestamp", mode: "kql", query: "status >= 500", filters: [], range: { last: "15m" }, columns: ["host"], sort: null });
    expect(toSpec({ ...base, timeField: undefined }).time_field).toBeNull();
    expect(fromSpec(spec, ["kql", "esql"])).toEqual({ ...base, columns: ["host"] });
    expect(fromSpec({ ...spec, mode: "ppl" }, ["kql", "esql"]).mode).toBe("kql");
    // The control plane answers with its keys in another order: still the same view.
    const stored = JSON.parse(JSON.stringify({ sort: null, view: "logs-*", columns: ["host"], range: { last: "15m" }, filters: [], query: "status >= 500", mode: "kql", time_field: "@timestamp" }));
    expect(edited({ ...base, columns: ["host"] }, stored)).toBe(false);
    expect(edited({ ...base, columns: ["host"], query: "status >= 400" }, stored)).toBe(true);
  });
});

describe("Discover's surrounding documents", () => {
  it("find the anchor by id, then search after its sort values both ways", () => {
    expect(anchorBody("@timestamp", "abc")).toEqual({
      size: 1,
      query: { ids: { values: ["abc"] } },
      sort: [{ "@timestamp": { order: "desc", unmapped_type: "date" } }, { _doc: { order: "desc" } }],
      fields: [{ field: "@timestamp", format: "epoch_millis" }],
      track_total_hits: false,
    });
    const filters = [{ field: "host", value: "web-1", negate: false }];
    const { newer, older } = surroundingBodies({ filters }, "@timestamp", [NOW, 7], { newer: 5, older: 10 });
    // As in Kibana: the filters, not the query or the range.
    expect(newer).toMatchObject({ size: 5, query: { bool: { filter: [{ match_phrase: { host: "web-1" } }] } }, search_after: [NOW, 7], sort: [{ "@timestamp": { order: "asc" } }, { _doc: { order: "asc" } }] });
    expect(older).toMatchObject({ size: 10, search_after: [NOW, 7], sort: [{ "@timestamp": { order: "desc" } }, { _doc: { order: "desc" } }] });
  });
});

describe("Discover's field charts", () => {
  const fields = viewFields({
    fields: {
      "@timestamp": { date: { type: "date", aggregatable: true } },
      bytes: { long: { type: "long", aggregatable: true } },
      message: { text: { type: "text", aggregatable: false } },
      "message.keyword": { keyword: { type: "keyword", aggregatable: true } },
      body: { text: { type: "text", aggregatable: false } },
    },
  });

  it("draw a keyword's top values, a number's spread or a date over time, in one request over the range", () => {
    expect([chartOf(fields, "message"), chartOf(fields, "bytes"), chartOf(fields, "@timestamp"), chartOf(fields, "body")]).toEqual([
      { kind: "terms", field: "message.keyword" },
      { kind: "number", field: "bytes" },
      { kind: "date", field: "@timestamp" },
      null,
    ]);
    const stats = { distinct: { cardinality: { field: "bytes" } }, missing: { missing: { field: "bytes" } } };
    expect(chartBody(base, "number", "bytes")).toEqual({
      size: 0,
      track_total_hits: true,
      query: searchBody(base).query,
      aggs: { bars: { variable_width_histogram: { field: "bytes", buckets: 30 } }, ...stats, low: { min: { field: "bytes" } }, high: { max: { field: "bytes" } }, mean: { avg: { field: "bytes" } } },
    });
    expect(chartBody(base, "terms", "host").aggs).toEqual({ bars: { terms: { field: "host", size: 10 } }, distinct: { cardinality: { field: "host" } }, missing: { missing: { field: "host" } } });
  });

  it("read the bars and the whole range's statistics from the answer", () => {
    const terms = chartData("terms", { hits: { total: { value: 90 } }, aggregations: { bars: { sum_other_doc_count: 5, buckets: [{ key: "web-1", doc_count: 85 }] }, distinct: { value: 2 }, missing: { doc_count: 0 } } });
    expect(terms).toEqual({ bars: [{ key: "web-1", count: 85 }], total: 90, other: 5, distinct: 2, missing: 0, min: null, max: null, avg: null });
    const numbers = chartData("number", { aggregations: { bars: { buckets: [{ key: 15, min: 10, max: 20, doc_count: 3 }] }, low: { value: 10 }, high: { value: 20 } } });
    expect(numbers.bars).toEqual([{ key: 15, from: 10, to: 20, count: 3 }]);
    const dates = chartData("date", { aggregations: { bars: { buckets: [{ key: 0, doc_count: 1 }, { key: 60_000, doc_count: 2 }] } } });
    expect(dates.bars.map((b) => [b.from, b.to])).toEqual([[0, 60_000], [60_000, 120_000]]);
  });
});

describe("Discover's times", () => {
  it("are the viewer's zone's, to the millisecond, from milliseconds or ISO text (UTC without an offset)", () => {
    expect(localTime(Date.UTC(2026, 10, 26, 12, 0, 0, 5), "Europe/Budapest")).toBe("2026-11-26 13:00:00.005");
    expect(localTime("2026-11-26T12:00:00", "America/New_York")).toBe("2026-11-26 07:00:00.000");
    expect(localTime("2026-07-01T00:00:00.250+02:00", "UTC")).toBe("2026-06-30 22:00:00.250");
    expect(localTime("1764158400000", "UTC")).toBe("2025-11-26 12:00:00.000");
    expect(localTime("not a date", "UTC")).toBe("not a date");
  });
});
