/**
 * roll search's Performance tab: request metrics (`GET /v1/search/clusters/{id}/metrics`) as
 * chart series, and slow log entries (`…/slowlog`, Elasticsearch's JSON lines) as rows.
 */

export type MetricsRange = "1h" | "24h" | "14d";
export const METRICS_RANGES: { value: MetricsRange; label: string }[] = [
  { value: "1h", label: "hour" },
  { value: "24h", label: "day" },
  { value: "14d", label: "14 days" },
];

export type RequestKind = "all" | "search" | "index" | "other";
export const REQUEST_KINDS: { value: RequestKind; label: string }[] = [
  { value: "all", label: "all" },
  { value: "search", label: "search" },
  { value: "index", label: "index" },
  { value: "other", label: "other" },
];

/** One kind's requests over a point's step (or the whole range, in `totals`). */
export interface KindSummary {
  requests: number;
  /** Requests per second over the step. */
  rate: number;
  "2xx": number;
  "4xx": number;
  "5xx": number;
  /** 429s: the cluster's limits or thread pools refused them. */
  rejected: number;
  /** Null when nothing ran (only refusals, or no request). */
  p50_ms: number | null;
  p95_ms: number | null;
  p99_ms: number | null;
}

export interface MetricsPoint extends Record<RequestKind, KindSummary> {
  /** Unix seconds: the start of the point's step. */
  at: number;
}

export interface SearchMetrics {
  range: MetricsRange;
  step_s: number;
  /** Unix seconds: the first point's start and the end of the last. */
  from: number;
  to: number;
  /** Only steps with requests, oldest first. */
  points: MetricsPoint[];
  totals: MetricsPoint;
}

/** What the charts draw: one value per step of the range, gaps filled. */
export interface Series {
  /** Unix seconds, one per step. */
  at: number[];
  rate: number[];
  /** Null where nothing ran: the line breaks there. */
  p50: (number | null)[];
  p95: (number | null)[];
  p99: (number | null)[];
  client: number[];
  server: number[];
  rejected: number[];
}

/** Every step of the range for one kind: steps without requests are zero (asleep, or idle). */
export function series(m: SearchMetrics, kind: RequestKind): Series {
  const out: Series = { at: [], rate: [], p50: [], p95: [], p99: [], client: [], server: [], rejected: [] };
  const byAt = new Map(m.points.map((p) => [p.at, p[kind]]));
  for (let at = m.from; at < m.to; at += m.step_s) {
    const k = byAt.get(at);
    out.at.push(at);
    out.rate.push(k?.rate ?? 0);
    out.p50.push(k?.p50_ms ?? null);
    out.p95.push(k?.p95_ms ?? null);
    out.p99.push(k?.p99_ms ?? null);
    out.client.push(k?.["4xx"] ?? 0);
    out.server.push(k?.["5xx"] ?? 0);
    out.rejected.push(k?.rejected ?? 0);
  }
  return out;
}

/** An SVG path through `values` on `x`/`y`, broken at nulls. */
export function linePath(values: (number | null)[], x: (i: number) => number, y: (v: number) => number): string {
  let d = "";
  let pen = false;
  values.forEach((v, i) => {
    if (v == null) {
      pen = false;
      return;
    }
    d += `${pen ? "L" : "M"}${x(i).toFixed(1)},${y(v).toFixed(1)}`;
    pen = true;
  });
  return d;
}

export function formatRate(r: number): string {
  if (r === 0) return "0";
  if (r < 0.01) return "<0.01";
  if (r < 10) return r.toFixed(2);
  if (r < 1000) return r.toFixed(0);
  return `${(r / 1000).toFixed(1)}k`;
}

export function formatLatency(ms: number | null | undefined): string {
  if (ms == null) return "—";
  if (ms < 1) return `${Math.round(ms * 1000)} µs`;
  if (ms < 100) return `${ms.toFixed(1)} ms`;
  if (ms < 1000) return `${Math.round(ms)} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

/** A slow log line as the API returns it: Elasticsearch's JSON slow log fields. */
export type SlowLogLine = Record<string, unknown>;

export interface SearchSlowLog {
  entries: SlowLogLine[];
  max_entries: number;
}

export type SlowPhase = "query" | "fetch" | "index";

/** A slow log line, read. */
export interface SlowEntry {
  /** Unix ms. */
  at: number;
  level: string;
  phase: SlowPhase;
  index: string;
  shard: number | null;
  tookMs: number;
  /** Elasticsearch's own spelling (`12.3ms`). */
  took: string;
  source: string;
  /** Search entries: the client's `X-Opaque-Id`. Indexing entries: the document's id. */
  id: string | null;
  totalHits: string | null;
  routing: string | null;
}

const str = (v: unknown) => (typeof v === "string" ? v : v == null ? null : String(v));

export function slowEntry(l: SlowLogLine): SlowEntry {
  const logger = str(l["log.logger"]) ?? "";
  const phase: SlowPhase = logger.endsWith(".fetch") ? "fetch" : logger.startsWith("index.indexing") ? "index" : "query";
  const shard = l["elasticsearch.shard.id"];
  return {
    at: Date.parse(str(l["@timestamp"]) ?? "") || 0,
    level: str(l["log.level"]) ?? "",
    phase,
    index: str(l["elasticsearch.index.name"]) ?? "",
    shard: typeof shard === "number" ? shard : null,
    tookMs: Number(l["elasticsearch.slowlog.took_millis"] ?? 0),
    took: str(l["elasticsearch.slowlog.took"]) ?? "",
    source: str(l["elasticsearch.slowlog.source"]) ?? "",
    id: str(l["elasticsearch.slowlog.id"]),
    totalHits: str(l["elasticsearch.slowlog.total_hits"]),
    routing: str(l["elasticsearch.slowlog.routing"]),
  };
}

/** Whether an entry's source was cut at its limit (it no longer parses as JSON). */
export function sourceCut(e: SlowEntry): boolean {
  if (!e.source) return false;
  try {
    JSON.parse(e.source);
    return false;
  } catch {
    return true;
  }
}

/** The source indented when it parses, else as kept. */
export function prettySource(source: string): string {
  try {
    return JSON.stringify(JSON.parse(source), null, 2);
  } catch {
    return source;
  }
}

/** The request the entry came from, for the console: a search of its index with its body, or
 * the document it indexed. */
export function consoleText(e: SlowEntry): string {
  const index = encodeURIComponent(e.index);
  const body = e.source ? `\n${prettySource(e.source)}` : "";
  if (e.phase === "index") {
    const route = e.routing ? `?routing=${encodeURIComponent(e.routing)}` : "";
    return e.id ? `PUT /${index}/_doc/${encodeURIComponent(e.id)}${route}${body}` : `POST /${index}/_doc${route}${body}`;
  }
  return `GET /${index}/_search${body}`;
}
