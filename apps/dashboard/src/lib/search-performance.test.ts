import { describe, expect, it } from "vitest";
import { consoleText, formatLatency, formatRate, linePath, series, slowEntry, sourceCut, type KindSummary, type MetricsPoint, type SearchMetrics } from "./search-performance";

const k = (requests: number, p50: number | null = 1.5): KindSummary => ({
  requests,
  rate: requests / 60,
  "2xx": requests,
  "4xx": 0,
  "5xx": 0,
  rejected: 0,
  p50_ms: p50,
  p95_ms: p50,
  p99_ms: p50,
});
const point = (at: number, search: number, index: number): MetricsPoint => ({ at, all: k(search + index), search: k(search), index: k(index), other: k(0, null) });

describe("series", () => {
  it("fills every step of the range, zero where nothing ran", () => {
    const m: SearchMetrics = { range: "1h", step_s: 60, from: 600, to: 600 + 5 * 60, points: [point(660, 6, 0), point(840, 0, 3)], totals: point(600, 6, 3) };
    const s = series(m, "search");
    expect(s.at).toEqual([600, 660, 720, 780, 840]);
    expect(s.rate).toEqual([0, 0.1, 0, 0, 0]);
    // No latency where nothing ran: the line breaks.
    expect(s.p50).toEqual([null, 1.5, null, null, 1.5]);
    expect(series(m, "index").rate).toEqual([0, 0, 0, 0, 0.05]);
  });

  it("draws lines broken at gaps", () => {
    expect(linePath([1, 2, null, 4], (i) => i * 10, (v) => v)).toBe("M0.0,1.0L10.0,2.0M30.0,4.0");
    expect(linePath([null, null], (i) => i, (v) => v)).toBe("");
  });
});

describe("slow log entries", () => {
  const search = {
    "@timestamp": "2026-11-17T10:00:00.000Z",
    "log.level": "WARN",
    "log.logger": "index.search.slowlog.query",
    "elasticsearch.index.name": "logs",
    "elasticsearch.shard.id": 0,
    "elasticsearch.slowlog.took": "12.3ms",
    "elasticsearch.slowlog.took_millis": 12,
    "elasticsearch.slowlog.total_hits": "5 hits",
    "elasticsearch.slowlog.source": '{"query":{"match":{"msg":"timeout"}}}',
    "elasticsearch.slowlog.id": "trace-7",
  };

  it("reads Elasticsearch's fields", () => {
    const e = slowEntry(search);
    expect(e).toMatchObject({ phase: "query", level: "WARN", index: "logs", shard: 0, tookMs: 12, took: "12.3ms", id: "trace-7", totalHits: "5 hits" });
    expect(e.at).toBe(Date.parse("2026-11-17T10:00:00.000Z"));
    expect(slowEntry({ ...search, "log.logger": "index.search.slowlog.fetch" }).phase).toBe("fetch");
  });

  it("opens a search in the console as the request it was", () => {
    expect(consoleText(slowEntry(search))).toBe('GET /logs/_search\n{\n  "query": {\n    "match": {\n      "msg": "timeout"\n    }\n  }\n}');
    // A source cut at its limit goes as kept.
    const cut = slowEntry({ ...search, "elasticsearch.slowlog.source": '{"query":{"match":{"msg":"time' });
    expect(sourceCut(cut)).toBe(true);
    expect(sourceCut(slowEntry(search))).toBe(false);
    expect(consoleText(cut)).toBe('GET /logs/_search\n{"query":{"match":{"msg":"time');
  });

  it("opens a write as the document it indexed", () => {
    const e = slowEntry({
      "log.logger": "index.indexing.slowlog.index",
      "elasticsearch.index.name": "logs",
      "elasticsearch.slowlog.id": "a b",
      "elasticsearch.slowlog.routing": "u1",
      "elasticsearch.slowlog.source": '{"n":1}',
    });
    expect(e.phase).toBe("index");
    expect(consoleText(e)).toBe('PUT /logs/_doc/a%20b?routing=u1\n{\n  "n": 1\n}');
  });
});

describe("formatting", () => {
  it("formats rates and latencies", () => {
    expect([0, 0.004, 1.234, 42.4, 12_345].map(formatRate)).toEqual(["0", "<0.01", "1.23", "42", "12.3k"]);
    expect([null, 0.25, 3.21, 450.4, 2500].map(formatLatency)).toEqual(["—", "250 µs", "3.2 ms", "450 ms", "2.50 s"]);
  });
});
