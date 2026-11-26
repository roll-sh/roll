/**
 * Discover (the cluster page's `discover` tab): Kibana's and OpenSearch Dashboards' first screen
 * for logs. A view (an index pattern, alias or data stream), its time field, a range, a query in
 * one of the cluster's languages, the chosen columns. Everything here is pure: the requests the
 * tab sends, what it reads from their answers, the query edits of the field sidebar's + and −,
 * and the view as the URL keeps it.
 */
import { type MappedField, sortClause, type TableSort, timeFilter, type TimeRange } from "./search-docs";
import type { SavedSpec } from "./search-api";

/** The query languages: KQL and ES|QL on Elasticsearch-flavor clusters, DQL and PPL on OpenSearch ones. */
export type DiscoverMode = "kql" | "esql" | "dql" | "ppl";

export const MODES: Record<"elasticsearch" | "opensearch", { value: DiscoverMode; label: string }[]> = {
  elasticsearch: [
    { value: "kql", label: "KQL" },
    { value: "esql", label: "ES|QL" },
  ],
  opensearch: [
    { value: "dql", label: "DQL" },
    { value: "ppl", label: "PPL" },
  ],
};

/** Whether a mode's query is the search's filter (KQL, DQL), or a whole piped query (ES|QL, PPL). */
export const piped = (mode: DiscoverMode) => mode === "esql" || mode === "ppl";

/** A Discover view, as the URL holds it. */
export interface DiscoverState {
  view: string;
  /** The time field; `undefined` to pick one from the view's fields, `null` for none. */
  timeField: string | null | undefined;
  mode: DiscoverMode;
  query: string;
  range: TimeRange;
  columns: string[];
  sort: TableSort | null;
}

/** The documents a page of the table shows. */
export const DISCOVER_PAGE = 50;
/** The documents the field sidebar's top values are counted over (`sampler`'s `shard_size`). */
export const SAMPLE_SIZE = 500;
/** The rows a piped query (ES|QL, PPL) returns when it doesn't say. */
export const PIPED_LIMIT = 100;

export function initialState(flavor: "elasticsearch" | "opensearch"): DiscoverState {
  return { view: "", timeField: undefined, mode: MODES[flavor][0].value, query: "", range: { last: "24h" }, columns: [], sort: null };
}

/** The filter KQL and DQL texts become: roll serves the `kql` query on both flavors (DQL is KQL's
 * syntax, which OpenSearch Dashboards compiles in the browser). An empty query matches everything. */
function textQuery(query: string): Record<string, unknown> {
  return query.trim() ? { kql: { query: query.trim() } } : { match_all: {} };
}

/** The query of a KQL or DQL view: its text and the time range, both filters (no scoring). */
export function viewQuery(s: Pick<DiscoverState, "query" | "range" | "timeField">): Record<string, unknown> {
  const range = s.timeField ? timeFilter(s.timeField, s.range) : null;
  return { bool: { filter: [textQuery(s.query), ...(range ? [range] : [])] } };
}

/** `POST /<view>/_search` of a KQL or DQL view: a page newest first (or by a column), and on the
 * first page the documents over time (`auto_date_histogram`). */
export function searchBody(s: DiscoverState, from = 0, size = DISCOVER_PAGE): Record<string, unknown> {
  const sort = sortClause(s.sort) ?? (s.timeField ? [{ [s.timeField]: { order: "desc", unmapped_type: "date" } }] : undefined);
  return {
    query: viewQuery(s),
    from,
    size,
    track_total_hits: true,
    ...(sort ? { sort } : {}),
    ...(s.timeField ? { fields: timeValue(s.timeField) } : {}),
    ...(s.timeField && from === 0 ? { aggs: { over_time: { auto_date_histogram: { field: s.timeField, buckets: 60 } } } } : {}),
  };
}

/** The top values of one field over a sample of the view's matching documents (about
 * [`SAMPLE_SIZE`] a shard: `sampler` keeps each shard's best, which are all equal under filters). */
export function sampleBody(s: Pick<DiscoverState, "query" | "range" | "timeField">, field: string, size = 5): Record<string, unknown> {
  return { size: 0, query: viewQuery(s), aggs: { sample: { sampler: { shard_size: SAMPLE_SIZE }, aggs: { top: { terms: { field, size } } } } } };
}

/** A field's top values from [`sampleBody`]'s answer, and how many documents were sampled. */
export function topValues(resp: unknown): { sampled: number; values: { value: string | number | boolean; count: number }[] } {
  type Bucket = { key: string | number | boolean; key_as_string?: string; doc_count: number };
  const sample = (resp as { aggregations?: { sample?: { doc_count?: number; top?: { buckets?: Bucket[] } } } } | null)?.aggregations?.sample;
  return {
    sampled: sample?.doc_count ?? 0,
    values: (sample?.top?.buckets ?? []).map((b) => ({ value: b.key_as_string ?? b.key, count: b.doc_count })),
  };
}

/** The bounds of a range at `now` (milliseconds), for languages that take times written out. */
export function rangeBounds(range: TimeRange, now: number): { gte?: number; lt?: number } {
  if ("gte" in range) return { gte: range.gte, lt: range.lt };
  if (!range.last) return {};
  const m = /^(\d+)([smhd])$/.exec(range.last);
  if (!m) return {};
  const unit = { s: 1_000, m: 60_000, h: 3_600_000, d: 86_400_000 }[m[2] as "s" | "m" | "h" | "d"];
  return { gte: now - Number(m[1]) * unit };
}

/** An ES|QL or PPL identifier, in backquotes (a backquote doubled). */
const quoted = (name: string) => `\`${name.replace(/`/g, "``")}\``;

/** The query after the view's own lines, without a leading `|`. */
const tail = (query: string) => query.trim().replace(/^\|\s*/, "");

/**
 * The ES|QL of a view: `FROM <view> | WHERE <time range> | <query>`, the query being what the
 * user typed after it (the newest [`PIPED_LIMIT`] rows when it's empty).
 */
export function esqlText(s: Pick<DiscoverState, "view" | "timeField" | "range" | "query">, now: number): string {
  const lines = [`FROM ${s.view}`];
  const b = s.timeField ? rangeBounds(s.range, now) : {};
  const iso = (ms: number) => `TO_DATETIME("${new Date(ms).toISOString()}")`;
  const conds = [b.gte !== undefined ? `${quoted(s.timeField!)} >= ${iso(b.gte)}` : null, b.lt !== undefined ? `${quoted(s.timeField!)} < ${iso(b.lt)}` : null].filter(Boolean);
  if (conds.length) lines.push(`WHERE ${conds.join(" AND ")}`);
  const rest = tail(s.query);
  if (rest) lines.push(rest);
  else lines.push(...(s.timeField ? [`SORT ${quoted(s.timeField)} DESC`] : []), `LIMIT ${PIPED_LIMIT}`);
  return lines.join("\n| ");
}

/** The PPL of a view: `source=<view> | where <time range> | <query>` (the newest
 * [`PIPED_LIMIT`] rows when the query is empty). PPL's timestamps are `yyyy-MM-dd HH:mm:ss`, UTC. */
export function pplText(s: Pick<DiscoverState, "view" | "timeField" | "range" | "query">, now: number): string {
  const parts = [`source=${s.view}`];
  const b = s.timeField ? rangeBounds(s.range, now) : {};
  const ts = (ms: number) => `'${new Date(ms).toISOString().replace("T", " ").slice(0, 19)}'`;
  const conds = [b.gte !== undefined ? `${quoted(s.timeField!)} >= ${ts(b.gte)}` : null, b.lt !== undefined ? `${quoted(s.timeField!)} < ${ts(b.lt)}` : null].filter(Boolean);
  if (conds.length) parts.push(`where ${conds.join(" and ")}`);
  const rest = tail(s.query);
  if (rest) parts.push(rest);
  else parts.push(...(s.timeField ? [`sort - ${quoted(s.timeField)}`] : []), `head ${PIPED_LIMIT}`);
  return parts.join(" | ");
}

/** The table of an ES|QL (`columns`, `values`) or PPL (`schema`, `datarows`) answer. */
export function pipedTable(resp: unknown): { columns: string[]; rows: unknown[][] } {
  const r = (resp ?? {}) as { columns?: { name: string }[]; values?: unknown[][]; schema?: { name: string }[]; datarows?: unknown[][] };
  const cols = r.columns ?? r.schema ?? [];
  return { columns: cols.map((c) => c.name), rows: r.values ?? r.datarows ?? [] };
}

/** The request a view sends, as the console takes it: method, path and body. */
export function viewRequest(s: DiscoverState, now: number, from = 0): { method: "POST"; path: string; body: unknown } {
  if (s.mode === "esql") return { method: "POST", path: "/_query", body: { query: esqlText(s, now) } };
  if (s.mode === "ppl") return { method: "POST", path: "/_plugins/_ppl", body: { query: pplText(s, now) } };
  return { method: "POST", path: `/${encodeURIComponent(s.view)}/_search`, body: searchBody(s, from) };
}

/** A KQL (and DQL) field name, its special characters escaped. */
function kqlField(name: string): string {
  return name.replace(/[\\():<>"*{}\s]/g, (c) => `\\${c}`);
}

/** A KQL (and DQL) quoted value. */
function kqlValue(v: string | number | boolean): string {
  return `"${String(v).replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

/**
 * The field sidebar's + and −: the query with documents whose `field` is `value` kept (or left
 * out, `exclude`). KQL and DQL get `and field: "value"` (`not` before it to leave out), the
 * query so far in parentheses; ES|QL and PPL get a `WHERE`/`where` line.
 */
export function withFilter(mode: DiscoverMode, query: string, field: string, value: string | number | boolean, exclude: boolean): string {
  const q = query.trim();
  if (mode === "esql" || mode === "ppl") {
    const v = typeof value === "string" ? `"${value.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"` : String(value);
    const pplV = typeof value === "string" ? `'${value.replace(/\\/g, "\\\\").replace(/'/g, "\\'")}'` : String(value);
    const line = mode === "esql" ? `WHERE ${quoted(field)} ${exclude ? "!=" : "=="} ${v}` : `where ${quoted(field)} ${exclude ? "!=" : "="} ${pplV}`;
    return q ? `${q}\n| ${line}` : line;
  }
  const clause = `${exclude ? "not " : ""}${kqlField(field)}: ${kqlValue(value)}`;
  return q ? `(${q}) and ${clause}` : clause;
}

/** One field of the view, from `_field_caps`. */
export interface ViewField extends MappedField {
  aggregatable: boolean;
  /** The view's indices map it as more than one type. */
  conflict: boolean;
}

/** The view's fields from `_field_caps` (metadata fields left out), sorted by name. A field mapped
 * as several types takes the first and is marked. */
export function viewFields(caps: unknown): ViewField[] {
  const fields = (caps as { fields?: Record<string, Record<string, { type?: string; aggregatable?: boolean; metadata_field?: boolean }>> } | null)?.fields ?? {};
  const out: ViewField[] = [];
  for (const [name, byType] of Object.entries(fields)) {
    const types = Object.entries(byType).filter(([t, c]) => !c.metadata_field && t !== "object" && t !== "nested" && t !== "unmapped");
    if (name.startsWith("_") || types.length === 0) continue;
    out.push({ name, type: types[0][0], aggregatable: types.every(([, c]) => c.aggregatable === true), conflict: types.length > 1 });
  }
  return out.sort((a, b) => a.name.localeCompare(b.name));
}

/** The field a field's top values are counted on: itself when it aggregates, else its keyword
 * multi-field (`message.keyword`), else `null`. */
export function valuesField(fields: ViewField[], name: string): string | null {
  const own = fields.find((f) => f.name === name);
  if (own?.aggregatable) return name;
  const sub = fields.find((f) => f.name === `${name}.keyword` && f.aggregatable);
  return sub ? sub.name : null;
}

/** The time fields a view may be browsed by: its date fields. */
export const dateFields = (fields: ViewField[]) => fields.filter((f) => f.type === "date" || f.type === "date_nanos").map((f) => f.name);

/** The view as the URL's hash keeps it: `#discover?view=…&tf=…&mode=…&q=…&range=…&col=…&sort=…`. */
export function stateToHash(s: DiscoverState): string {
  const p = new URLSearchParams();
  if (s.view) p.set("view", s.view);
  if (s.timeField !== undefined) p.set("tf", s.timeField ?? "-");
  p.set("mode", s.mode);
  if (s.query) p.set("q", s.query);
  p.set("range", "gte" in s.range ? `${s.range.gte}-${s.range.lt}` : (s.range.last ?? "all"));
  for (const c of s.columns) p.append("col", c);
  if (s.sort) p.set("sort", `${s.sort.column}:${s.sort.field}:${s.sort.order}`);
  return `#discover?${p.toString()}`;
}

/** [`stateToHash`] read back, what it doesn't hold (or holds wrongly) taken from `base`. */
export function hashToState(hash: string, base: DiscoverState, modes: DiscoverMode[]): DiscoverState {
  const at = hash.indexOf("?");
  if (!hash.startsWith("#discover") || at < 0) return base;
  const p = new URLSearchParams(hash.slice(at + 1));
  const tf = p.get("tf");
  const mode = p.get("mode") as DiscoverMode | null;
  const r = p.get("range");
  let range: TimeRange = base.range;
  const abs = r ? /^(\d+)-(\d+)$/.exec(r) : null;
  if (abs && Number(abs[1]) < Number(abs[2])) range = { gte: Number(abs[1]), lt: Number(abs[2]) };
  else if (r === "all") range = { last: null };
  else if (r && /^\d+[smhd]$/.test(r)) range = { last: r };
  const sort = p.get("sort")?.split(":");
  return {
    view: p.get("view") ?? base.view,
    timeField: tf === null ? base.timeField : tf === "-" ? null : tf,
    mode: mode && modes.includes(mode) ? mode : base.mode,
    query: p.get("q") ?? base.query,
    range,
    columns: p.getAll("col").length ? p.getAll("col") : base.columns,
    sort: sort && sort.length === 3 && (sort[2] === "asc" || sort[2] === "desc") ? { column: sort[0], field: sort[1], order: sort[2] } : base.sort,
  };
}

// ---- Saved searches (kept by roll's control plane: `crates/search-control/src/saved.rs`).

/** A view as a saved search keeps it (an undecided time field is none). */
export function toSpec(s: DiscoverState): SavedSpec {
  return { view: s.view, time_field: s.timeField ?? null, mode: s.mode, query: s.query, range: s.range, columns: s.columns, sort: s.sort };
}

/** A saved search opened as a view; a mode this cluster's flavor hasn't falls back on its first. */
export function fromSpec(spec: SavedSpec, modes: DiscoverMode[]): DiscoverState {
  return {
    view: spec.view,
    timeField: spec.time_field,
    mode: modes.includes(spec.mode) ? spec.mode : modes[0],
    query: spec.query,
    range: spec.range,
    columns: spec.columns,
    sort: spec.sort,
  };
}

/** JSON with every object's keys sorted, so two specs compare by what they hold. */
const canonical = (v: unknown): string =>
  Array.isArray(v)
    ? `[${v.map(canonical).join(",")}]`
    : v && typeof v === "object"
      ? `{${Object.keys(v)
          .sort()
          .map((k) => `${JSON.stringify(k)}:${canonical((v as Record<string, unknown>)[k])}`)
          .join(",")}}`
      : JSON.stringify(v ?? null);

/** Whether a view differs from the saved search it was opened from. */
export const edited = (s: DiscoverState, spec: SavedSpec) => canonical(toSpec(s)) !== canonical(spec);

// ---- Surrounding documents (Kibana's "View surrounding documents").

/** The order around an anchor: the time field, then `_doc` (Kibana's default tiebreaker), both
 * the same way. Sort values of `_doc` are per shard, so documents of one millisecond on different
 * shards keep a stable but arbitrary order. */
const contextSort = (tf: string, order: "asc" | "desc") => [{ [tf]: { order, unmapped_type: "date" } }, { _doc: { order } }];

/** The time field as epoch milliseconds in each hit's `fields`, whatever its format in `_source`. */
export const timeValue = (tf: string) => [{ field: tf, format: "epoch_millis" }];

/** `POST /<index>/_search` for the anchor itself (by id on its own index): its source, its time,
 * and its sort values for `search_after`. */
export function anchorBody(tf: string, id: string): Record<string, unknown> {
  return { size: 1, query: { ids: { values: [id] } }, sort: contextSort(tf, "desc"), fields: timeValue(tf), track_total_hits: false };
}

/**
 * `POST /<view>/_search` for the documents around an anchor: `newer` (ascending after its sort
 * values; shown reversed above it) and `older` (descending after them). The range doesn't apply;
 * the view's query does when `keepQuery`.
 */
export function surroundingBodies(s: Pick<DiscoverState, "query">, tf: string, anchorSort: unknown[], counts: { newer: number; older: number }, keepQuery: boolean): { newer: Record<string, unknown>; older: Record<string, unknown> } {
  const query = { bool: { filter: [textQuery(keepQuery ? s.query : "")] } };
  const one = (order: "asc" | "desc", size: number) => ({ size, query, sort: contextSort(tf, order), search_after: anchorSort, fields: timeValue(tf), track_total_hits: false });
  return { newer: one("asc", counts.newer), older: one("desc", counts.older) };
}

/** How many more surrounding documents a click loads. */
export const CONTEXT_STEP = 5;

// ---- Visualizing a field.

/** How a field is drawn: its top values, its numbers' spread, or its dates over time. */
export type FieldChart = "terms" | "number" | "date";

const NUMERIC = new Set(["long", "integer", "short", "byte", "double", "float", "half_float", "scaled_float", "unsigned_long"]);

/** The chart a field gets (on the field the values come from), or `null` when it doesn't aggregate. */
export function chartOf(fields: ViewField[], name: string): { kind: FieldChart; field: string } | null {
  const f = fields.find((x) => x.name === name);
  if (!f) return null;
  if (f.type === "date" || f.type === "date_nanos") return f.aggregatable ? { kind: "date", field: name } : null;
  if (NUMERIC.has(f.type)) return f.aggregatable ? { kind: "number", field: name } : null;
  const on = valuesField(fields, name);
  return on ? { kind: "terms", field: on } : null;
}

/** The bars a chart draws. */
export const CHART_BARS = { terms: 10, number: 30, date: 40 } as const;

/**
 * One request for a field's chart over the view's whole query and range (not the sidebar's
 * sample): the bars (`terms`, `variable_width_histogram` for numbers, `auto_date_histogram` for
 * dates), with the distinct values (`cardinality`, approximate past 3,000), the documents
 * without a value (`missing`) and, for numbers and dates, `min` and `max`.
 */
export function chartBody(s: Pick<DiscoverState, "query" | "range" | "timeField">, kind: FieldChart, field: string): Record<string, unknown> {
  const bars =
    kind === "terms"
      ? { terms: { field, size: CHART_BARS.terms } }
      : kind === "number"
        ? { variable_width_histogram: { field, buckets: CHART_BARS.number } }
        : { auto_date_histogram: { field, buckets: CHART_BARS.date } };
  const extent = kind === "terms" ? {} : { low: { min: { field } }, high: { max: { field } } };
  return { size: 0, track_total_hits: true, query: viewQuery(s), aggs: { bars, distinct: { cardinality: { field } }, missing: { missing: { field } }, ...extent } };
}

/** A chart's bar: its label (a value, or where a range of numbers or dates starts), its bounds
 * for numbers and dates, and its documents. */
export interface ChartBar {
  key: string | number | boolean;
  from?: number;
  to?: number;
  count: number;
}

/** What [`chartBody`]'s answer says. */
export interface ChartData {
  bars: ChartBar[];
  /** The documents the query and range match. */
  total: number;
  /** In the top values (`terms` only): the documents with another value. */
  other: number;
  distinct: number | null;
  missing: number | null;
  min: number | null;
  max: number | null;
}

export function chartData(kind: FieldChart, resp: unknown): ChartData {
  type B = { key: string | number | boolean; key_as_string?: string; doc_count: number; min?: number; max?: number };
  type A = { buckets?: B[]; sum_other_doc_count?: number; value?: number | null; doc_count?: number; interval?: string };
  const r = (resp ?? {}) as { hits?: { total?: { value?: number } }; aggregations?: Record<string, A> };
  const a = r.aggregations ?? {};
  const buckets = a.bars?.buckets ?? [];
  const step = kind === "date" && buckets.length > 1 ? Number(buckets[1].key) - Number(buckets[0].key) : 0;
  const bars = buckets.map((b, i): ChartBar => {
    if (kind === "terms") return { key: b.key_as_string ?? b.key, count: b.doc_count };
    if (kind === "number") return { key: b.key, from: b.min ?? Number(b.key), to: b.max ?? Number(b.key), count: b.doc_count };
    const from = Number(b.key);
    return { key: from, from, to: Number(buckets[i + 1]?.key ?? from + (step || 1)), count: b.doc_count };
  });
  return {
    bars,
    total: r.hits?.total?.value ?? 0,
    other: a.bars?.sum_other_doc_count ?? 0,
    distinct: a.distinct?.value ?? null,
    missing: a.missing?.doc_count ?? null,
    min: a.low?.value ?? null,
    max: a.high?.value ?? null,
  };
}

// ---- Times in the viewer's zone.

/** The viewer's time zone, shown next to the times (`Europe/Budapest`). */
export function viewerZone(): string {
  return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
}

/** A time (epoch milliseconds, or ISO text: without an offset it's UTC, as Elasticsearch reads it)
 * in `zone`, to the millisecond: `2026-11-26 13:59:00.000`. Anything else is shown as it is. */
export function localTime(v: unknown, zone = viewerZone()): string {
  const iso = (t: string) => (/T\d\d:\d\d(:\d\d(\.\d+)?)?$/.test(t) ? `${t}Z` : t);
  const ms = typeof v === "number" ? v : typeof v === "string" && v.trim() ? (/^-?\d+(\.\d+)?$/.test(v.trim()) ? Number(v) : Date.parse(iso(v.trim()))) : NaN;
  if (!Number.isFinite(ms) || Math.abs(ms) > 8.64e15) return v == null ? "" : String(v);
  const p = Object.fromEntries(
    new Intl.DateTimeFormat("en-CA", { timeZone: zone, year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit", hourCycle: "h23" })
      .formatToParts(new Date(ms))
      .map((x) => [x.type, x.value]),
  );
  const milli = String(Math.floor(((ms % 1000) + 1000) % 1000)).padStart(3, "0");
  return `${p.year}-${p.month}-${p.day} ${p.hour}:${p.minute}:${p.second}.${milli}`;
}
