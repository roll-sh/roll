/**
 * Completion for the search console (Kibana Dev Tools syntax: `METHOD /path` on the first line,
 * the JSON body below).
 *
 * Everything here is pure (tests: `search-console.test.ts`).
 *
 * - On the first line it completes the method, then the path one segment at a time: index names
 *   and top-level APIs first, then the APIs under an index or under `_cat`, `_cluster`, ….
 * - In the body a tolerant scanner ([`jsonContext`]) walks the JSON up to the cursor, keeping the
 *   chain of keys that leads there even when the text is unfinished. The chain picks the
 *   vocabulary: search body keys, query types where a query goes, the bool clauses, aggregation
 *   types under an aggregation's name, mapping parameters under `properties.<field>`, and the
 *   index's field names where a field goes (`match.<field>`, `"field": "<field>"`, `sort`).
 * - `_bulk` and `_msearch` bodies are one JSON document per line; each line is completed on its
 *   own, as an action/header or as a document/search body depending on its position.
 * - After `?` in the path it completes the URL query parameters of the API the path names (and
 *   the common ones: `pretty`, `filter_path`, …), then their allowed values after `=`. The table
 *   comes from Elasticsearch's REST API spec (`search-params.generated.ts`) and is passed in
 *   ([`CompletionSources.params`]), which keeps this module free of imports.
 */

export type SuggestionKind = "method" | "api" | "index" | "key" | "query" | "agg" | "field" | "value" | "param";

export interface Suggestion {
  /** What the list shows (and what the typed prefix is matched against). */
  label: string;
  kind: SuggestionKind;
  /** A short note shown beside it (a field's type, what an API does). */
  detail?: string;
  /** For keys: the value inserted after the colon, the cursor going inside it. */
  value?: "{}" | "[]" | '""' | "";
  /** Plain completions: the text inserted instead of the label (`size=` for a parameter). */
  insert?: string;
}

export interface Completion {
  /** The text replaced by the chosen suggestion: `[from, to)`. */
  from: number;
  to: number;
  /** What was typed so far (the filter). */
  prefix: string;
  items: Suggestion[];
  /** Keys are inserted quoted, followed by `: ` and their value (unless a colon follows). */
  mode: "plain" | "key" | "string";
}

export interface FieldInfo {
  name: string;
  type: string;
}

export interface CompletionSources {
  indices: string[];
  fields: FieldInfo[];
  /** URL query parameters per API (`search-params.generated.ts`); none: no parameter completion. */
  params?: ParamTable;
}

/** A query parameter: name, type and, for enums, the allowed values. */
export type ParamSpec = readonly [string, string] | readonly [string, string, readonly string[]];

/** The parameters of the REST APIs: `apis` are `[methods bitmask over CONSOLE_METHODS, path pattern, parameter indexes]`. */
export interface ParamTable {
  params: readonly ParamSpec[];
  common: readonly number[];
  apis: readonly (readonly [number, string, readonly number[]])[];
}

/**
 * The query parameters `method path` accepts: those of the API paths it matches best (most
 * literal segments equal; `{placeholders}` match any segment), plus the common ones. A method
 * no matching path allows still gets the path's parameters (the request will fail on its own).
 */
export function apiParams(method: string, path: string, table: ParamTable): ParamSpec[] {
  const segs = path.split("?")[0].split("/").filter(Boolean);
  const bit = 1 << Math.max(0, CONSOLE_METHODS.indexOf(method.toUpperCase() as ConsoleMethod));
  let best = -1;
  let bestWithMethod = false;
  let ids = new Set<number>();
  for (const [mask, pattern, pids] of table.apis) {
    const ps = pattern.split("/").filter(Boolean);
    if (ps.length !== segs.length) continue;
    let literal = 0;
    let ok = true;
    for (let i = 0; i < ps.length && ok; i++) {
      if (ps[i].startsWith("{")) continue;
      if (ps[i] === segs[i]) literal++;
      else ok = false;
    }
    if (!ok) continue;
    const withMethod = (mask & bit) !== 0;
    // Prefer paths that allow the method, then more literal segments.
    if (withMethod !== bestWithMethod ? withMethod : literal > best) {
      best = literal;
      bestWithMethod = withMethod;
      ids = new Set(pids);
    } else if (withMethod === bestWithMethod && literal === best) {
      for (const id of pids) ids.add(id);
    }
  }
  for (const id of table.common) ids.add(id);
  const seen = new Set<string>();
  const out: ParamSpec[] = [];
  for (const id of [...ids].sort((a, b) => a - b)) {
    const p = table.params[id];
    if (!p || seen.has(p[0])) continue;
    seen.add(p[0]);
    out.push(p);
  }
  return out;
}

const BOOLEAN_VALUES = ["true", "false"];

/** Completion after the `?` of a request line: parameter names, or a parameter's values after `=`. */
function completeQuery(method: string, line: string, pathStart: number, cursor: number, table: ParamTable): Completion | null {
  const typed = line.slice(pathStart, cursor);
  const q = typed.indexOf("?");
  const path = typed.slice(0, q);
  const query = typed.slice(q + 1);
  const tokenStart = pathStart + q + 1 + query.lastIndexOf("&") + 1;
  const token = line.slice(tokenStart, cursor);
  const specs = apiParams(method, path, table);
  const eq = token.indexOf("=");
  if (eq < 0) {
    const present = new Set(
      line
        .slice(pathStart + q + 1)
        .split(/[&\s]/)
        .map((kv) => kv.split("=")[0])
        .filter((n) => n && n !== token),
    );
    const items = filterItems(
      specs.filter((p) => !present.has(p[0])).map((p) => ({ label: p[0], kind: "param" as const, detail: p[1], insert: p[1] === "boolean" ? p[0] : `${p[0]}=` })),
      token,
    );
    if (!items.length) return null;
    let to = cursor;
    while (to < line.length && !/[\s&=]/.test(line[to])) to++;
    // Keep a value already typed after the name.
    return { from: tokenStart, to, prefix: token, items, mode: "plain" };
  }
  const name = token.slice(0, eq);
  const spec = specs.find((p) => p[0] === name);
  if (!spec) return null;
  const values = spec[1] === "boolean" ? BOOLEAN_VALUES : (spec[2] ?? []);
  if (!values.length) return null;
  // Enum lists (`expand_wildcards=open,hidden`): complete the value after the last comma.
  const valueStart = tokenStart + eq + 1 + token.slice(eq + 1).lastIndexOf(",") + 1;
  const prefix = line.slice(valueStart, cursor);
  const items = filterItems(
    values.map((v) => ({ label: v, kind: "value" as const })),
    prefix,
  );
  if (!items.length) return null;
  let to = cursor;
  while (to < line.length && !/[\s&,]/.test(line[to])) to++;
  return { from: valueStart, to, prefix, items, mode: "plain" };
}

export const CONSOLE_METHODS = ["GET", "POST", "PUT", "DELETE", "HEAD"] as const;
export type ConsoleMethod = (typeof CONSOLE_METHODS)[number];

/** A console request: the body is the text as typed (sent byte for byte, never re-encoded). */
export interface ConsoleInput {
  method: ConsoleMethod;
  path: string;
  body?: string;
  /** The body is NDJSON (`_bulk`, `_msearch`): one JSON document per line. */
  ndjson: boolean;
}

/**
 * Parses a console request in Kibana Dev Tools syntax: the first line is `METHOD /path`, the
 * rest is the body (JSON, or NDJSON for `_bulk`/`_msearch`). A JSON body is only validated:
 * re-encoding it would change it (`1.0` becomes `1`, which Elasticsearch's dynamic mapping maps
 * as `long`; integers above 2^53 are rounded; integer-like keys move first).
 */
export function parseConsole(text: string): ConsoleInput {
  const trimmed = text.replace(/^\s+/, "");
  const nl = trimmed.indexOf("\n");
  const first = (nl < 0 ? trimmed : trimmed.slice(0, nl)).trim();
  const rest = nl < 0 ? "" : trimmed.slice(nl + 1).trim();
  const m = /^([A-Za-z]+)\s+(\S+)$/.exec(first);
  if (!m) throw new Error("Start with a method and a path, e.g. GET /_cat/indices?v");
  const method = m[1].toUpperCase() as ConsoleMethod;
  if (!CONSOLE_METHODS.includes(method)) throw new Error(`Unsupported method ${m[1]}`);
  const path = m[2].startsWith("/") ? m[2] : `/${m[2]}`;
  const ndjson = /\/_(bulk|msearch)(\/template)?(\?|$)/.test(path);
  if (!rest) return { method, path, ndjson };
  if (ndjson) return { method, path, body: `${rest}\n`, ndjson };
  try {
    JSON.parse(rest);
  } catch (e) {
    throw new Error(`The body isn't valid JSON: ${e instanceof Error ? e.message : String(e)}`);
  }
  return { method, path, body: rest, ndjson };
}

const api = (label: string, detail?: string): Suggestion => ({ label, kind: "api", detail });

/** APIs at the root of the path (`/_search`, `/_cat/…`). */
const ROOT_APIS: Suggestion[] = [
  api("_search", "search every index"),
  api("_count"),
  api("_msearch"),
  api("_bulk"),
  api("_mget"),
  api("_cat/", "compact tables"),
  api("_cluster/"),
  api("_nodes/"),
  api("_mapping"),
  api("_settings"),
  api("_aliases"),
  api("_alias"),
  api("_analyze"),
  api("_refresh"),
  api("_stats"),
  api("_tasks"),
  api("_reindex"),
  api("_index_template/"),
  api("_component_template/"),
  api("_ingest/"),
  api("_scripts/"),
  api("_pit"),
  api("_field_caps"),
  api("_resolve/index/"),
  api("_data_stream/"),
  api("_snapshot/"),
  api("_license"),
  api("_xpack"),
];

/** APIs under an index (`/<index>/_search`). */
const INDEX_APIS: Suggestion[] = [
  api("_search"),
  api("_doc/"),
  api("_create/"),
  api("_update/"),
  api("_source/"),
  api("_bulk"),
  api("_count"),
  api("_msearch"),
  api("_mget"),
  api("_mapping"),
  api("_settings"),
  api("_refresh"),
  api("_flush"),
  api("_forcemerge"),
  api("_delete_by_query"),
  api("_update_by_query"),
  api("_explain/"),
  api("_termvectors/"),
  api("_analyze"),
  api("_stats"),
  api("_segments"),
  api("_alias/"),
  api("_close"),
  api("_open"),
  api("_clone/"),
  api("_shrink/"),
  api("_split/"),
  api("_rollover"),
  api("_validate/query"),
  api("_field_caps"),
  api("_block/write"),
  api("_pit"),
];

/** APIs under a second-level namespace. */
const NAMESPACE_APIS: Record<string, Suggestion[]> = {
  _cat: [
    "indices",
    "health",
    "count",
    "aliases",
    "shards",
    "segments",
    "nodes",
    "allocation",
    "templates",
    "component_templates",
    "fielddata",
    "recovery",
    "thread_pool",
    "plugins",
    "pending_tasks",
    "tasks",
    "master",
    "nodeattrs",
    "repositories",
    "snapshots",
  ].map((s) => api(s)),
  _cluster: ["health", "state", "stats", "settings", "pending_tasks", "allocation/explain", "reroute"].map((s) => api(s)),
  _nodes: ["stats", "info", "usage", "hot_threads"].map((s) => api(s)),
  _ingest: ["pipeline/", "processor/grok"].map((s) => api(s)),
  _resolve: ["index/"].map((s) => api(s)),
};

const key = (label: string, value: Suggestion["value"] = "", detail?: string, kind: SuggestionKind = "key"): Suggestion => ({
  label,
  kind,
  value,
  detail,
});

const QUERY_TYPES: Suggestion[] = [
  ["match", "full text"],
  ["match_all", ""],
  ["match_none", ""],
  ["match_phrase", "full text"],
  ["match_phrase_prefix", "full text"],
  ["match_bool_prefix", "full text"],
  ["multi_match", "full text, many fields"],
  ["combined_fields", "full text, many fields"],
  ["query_string", "Lucene syntax"],
  ["simple_query_string", "Lucene syntax"],
  ["term", "exact value"],
  ["terms", "exact values"],
  ["terms_set", ""],
  ["range", ""],
  ["exists", ""],
  ["prefix", ""],
  ["wildcard", ""],
  ["regexp", ""],
  ["fuzzy", ""],
  ["ids", ""],
  ["bool", "compound"],
  ["boosting", "compound"],
  ["constant_score", "compound"],
  ["dis_max", "compound"],
  ["function_score", "compound"],
  ["script_score", ""],
  ["nested", ""],
  ["has_child", ""],
  ["has_parent", ""],
  ["parent_id", ""],
  ["geo_distance", "geo"],
  ["geo_bounding_box", "geo"],
  ["geo_shape", "geo"],
  ["geo_polygon", "geo"],
  ["script", ""],
  ["more_like_this", ""],
  ["intervals", ""],
  ["knn", "vectors"],
  ["distance_feature", ""],
  ["rank_feature", ""],
  ["span_term", "span"],
  ["span_near", "span"],
  ["span_or", "span"],
  ["wrapper", ""],
].map(([q, d]) => key(q, "{}", d || undefined, "query"));

const AGG_TYPES: Suggestion[] = [
  ["terms", "bucket"],
  ["multi_terms", "bucket"],
  ["significant_terms", "bucket"],
  ["rare_terms", "bucket"],
  ["histogram", "bucket"],
  ["date_histogram", "bucket"],
  ["auto_date_histogram", "bucket"],
  ["variable_width_histogram", "bucket"],
  ["range", "bucket"],
  ["date_range", "bucket"],
  ["ip_range", "bucket"],
  ["filter", "bucket"],
  ["filters", "bucket"],
  ["composite", "bucket"],
  ["nested", "bucket"],
  ["reverse_nested", "bucket"],
  ["global", "bucket"],
  ["missing", "bucket"],
  ["sampler", "bucket"],
  ["random_sampler", "bucket"],
  ["geo_distance", "bucket"],
  ["geohash_grid", "bucket"],
  ["geotile_grid", "bucket"],
  ["avg", "metric"],
  ["sum", "metric"],
  ["min", "metric"],
  ["max", "metric"],
  ["value_count", "metric"],
  ["cardinality", "metric"],
  ["stats", "metric"],
  ["extended_stats", "metric"],
  ["percentiles", "metric"],
  ["percentile_ranks", "metric"],
  ["median_absolute_deviation", "metric"],
  ["weighted_avg", "metric"],
  ["top_hits", "metric"],
  ["top_metrics", "metric"],
  ["string_stats", "metric"],
  ["boxplot", "metric"],
  ["geo_bounds", "metric"],
  ["geo_centroid", "metric"],
  ["scripted_metric", "metric"],
  ["bucket_script", "pipeline"],
  ["bucket_selector", "pipeline"],
  ["bucket_sort", "pipeline"],
  ["derivative", "pipeline"],
  ["cumulative_sum", "pipeline"],
  ["moving_fn", "pipeline"],
  ["avg_bucket", "pipeline"],
  ["sum_bucket", "pipeline"],
  ["max_bucket", "pipeline"],
  ["min_bucket", "pipeline"],
  ["stats_bucket", "pipeline"],
].map(([a, d]) => key(a, "{}", d, "agg"));

const SEARCH_BODY: Suggestion[] = [
  key("query", "{}"),
  key("aggs", "{}"),
  key("size", ""),
  key("from", ""),
  key("sort", "[]"),
  key("_source", ""),
  key("track_total_hits", ""),
  key("highlight", "{}"),
  key("post_filter", "{}"),
  key("fields", "[]"),
  key("docvalue_fields", "[]"),
  key("stored_fields", "[]"),
  key("script_fields", "{}"),
  key("runtime_mappings", "{}"),
  key("search_after", "[]"),
  key("collapse", "{}"),
  key("suggest", "{}"),
  key("knn", "{}"),
  key("rescore", "{}"),
  key("pit", "{}"),
  key("min_score", ""),
  key("timeout", '""'),
  key("terminate_after", ""),
  key("explain", ""),
  key("version", ""),
  key("seq_no_primary_term", ""),
  key("indices_boost", "[]"),
  key("profile", ""),
  key("aggregations", "{}"),
];

const BOOL: Suggestion[] = [
  key("must", "[]"),
  key("filter", "[]"),
  key("should", "[]"),
  key("must_not", "[]"),
  key("minimum_should_match", ""),
  key("boost", ""),
];

/** Parameters of a query type, under `<type>` (or `<type>.<field>` for field-keyed ones). */
const QUERY_PARAMS: Record<string, Suggestion[]> = {
  match: ["query", "operator", "fuzziness", "analyzer", "minimum_should_match", "zero_terms_query", "lenient", "boost"].map((k) =>
    key(k, k === "query" || k === "operator" || k === "analyzer" || k === "zero_terms_query" ? '""' : ""),
  ),
  match_phrase: ["query", "slop", "analyzer", "boost"].map((k) => key(k, k === "query" || k === "analyzer" ? '""' : "")),
  match_phrase_prefix: ["query", "slop", "max_expansions", "analyzer"].map((k) => key(k, k === "query" || k === "analyzer" ? '""' : "")),
  term: [key("value", '""'), key("boost", ""), key("case_insensitive", "")],
  range: [key("gte"), key("gt"), key("lte"), key("lt"), key("format", '""'), key("time_zone", '""'), key("relation", '""'), key("boost")],
  prefix: [key("value", '""'), key("case_insensitive"), key("boost")],
  wildcard: [key("value", '""'), key("case_insensitive"), key("boost")],
  regexp: [key("value", '""'), key("flags", '""'), key("case_insensitive"), key("boost")],
  fuzzy: [key("value", '""'), key("fuzziness"), key("prefix_length"), key("max_expansions"), key("transpositions")],
  multi_match: [key("query", '""'), key("fields", "[]"), key("type", '""'), key("operator", '""'), key("fuzziness"), key("tie_breaker"), key("minimum_should_match")],
  combined_fields: [key("query", '""'), key("fields", "[]"), key("operator", '""'), key("minimum_should_match")],
  query_string: [
    key("query", '""'),
    key("default_field", '""'),
    key("fields", "[]"),
    key("default_operator", '""'),
    key("analyze_wildcard"),
    key("allow_leading_wildcard"),
    key("lenient"),
    key("fuzziness"),
  ],
  simple_query_string: [key("query", '""'), key("fields", "[]"), key("default_operator", '""'), key("flags", '""'), key("analyze_wildcard")],
  exists: [key("field", '""')],
  ids: [key("values", "[]")],
  terms_set: [key("terms", "[]"), key("minimum_should_match_field", '""'), key("minimum_should_match_script", "{}")],
  bool: BOOL,
  constant_score: [key("filter", "{}"), key("boost")],
  boosting: [key("positive", "{}"), key("negative", "{}"), key("negative_boost")],
  dis_max: [key("queries", "[]"), key("tie_breaker")],
  function_score: [key("query", "{}"), key("functions", "[]"), key("score_mode", '""'), key("boost_mode", '""'), key("max_boost"), key("min_score")],
  script_score: [key("query", "{}"), key("script", "{}"), key("min_score")],
  nested: [key("path", '""'), key("query", "{}"), key("score_mode", '""'), key("ignore_unmapped"), key("inner_hits", "{}")],
  has_child: [key("type", '""'), key("query", "{}"), key("score_mode", '""'), key("min_children"), key("max_children"), key("inner_hits", "{}")],
  has_parent: [key("parent_type", '""'), key("query", "{}"), key("score"), key("inner_hits", "{}")],
  geo_distance: [key("distance", '""'), key("distance_type", '""'), key("validation_method", '""')],
  knn: [key("field", '""'), key("query_vector", "[]"), key("k"), key("num_candidates"), key("filter", "{}"), key("similarity")],
  more_like_this: [key("fields", "[]"), key("like", '""'), key("min_term_freq"), key("max_query_terms"), key("min_doc_freq")],
  script: [key("script", "{}")],
};
QUERY_PARAMS.match_bool_prefix = QUERY_PARAMS.match;

/** Query types whose body is `{ <field>: … }`. */
const FIELD_KEYED = new Set([
  "match",
  "match_phrase",
  "match_phrase_prefix",
  "match_bool_prefix",
  "term",
  "terms",
  "range",
  "prefix",
  "wildcard",
  "regexp",
  "fuzzy",
  "geo_distance",
  "geo_bounding_box",
  "geo_shape",
  "span_term",
  "intervals",
]);

/** Keys whose value is a query. */
const QUERY_SLOTS = new Set(["query", "filter", "post_filter", "positive", "negative", "rescore_query"]);
/** Keys whose value is an array of queries. */
const QUERY_LIST_SLOTS = new Set(["must", "should", "must_not", "filter", "queries"]);

const AGG_COMMON: Suggestion[] = [key("field", '""'), key("script", "{}"), key("missing", "")];

const AGG_PARAMS: Record<string, Suggestion[]> = {
  terms: [key("field", '""'), key("size"), key("order", "{}"), key("min_doc_count"), key("shard_size"), key("missing"), key("include"), key("exclude"), key("show_term_doc_count_error")],
  histogram: [key("field", '""'), key("interval"), key("min_doc_count"), key("extended_bounds", "{}"), key("hard_bounds", "{}"), key("offset"), key("missing")],
  date_histogram: [
    key("field", '""'),
    key("calendar_interval", '""'),
    key("fixed_interval", '""'),
    key("format", '""'),
    key("time_zone", '""'),
    key("min_doc_count"),
    key("extended_bounds", "{}"),
    key("offset", '""'),
  ],
  range: [key("field", '""'), key("ranges", "[]"), key("keyed")],
  date_range: [key("field", '""'), key("ranges", "[]"), key("format", '""'), key("time_zone", '""')],
  filters: [key("filters", "{}"), key("other_bucket"), key("other_bucket_key", '""')],
  composite: [key("sources", "[]"), key("size"), key("after", "{}")],
  nested: [key("path", '""')],
  top_hits: [key("size"), key("sort", "[]"), key("_source"), key("from"), key("highlight", "{}")],
  percentiles: [key("field", '""'), key("percents", "[]"), key("keyed"), key("tdigest", "{}"), key("hdr", "{}")],
  cardinality: [key("field", '""'), key("precision_threshold"), key("missing")],
  bucket_script: [key("buckets_path", "{}"), key("script", '""')],
  bucket_selector: [key("buckets_path", "{}"), key("script", '""')],
  bucket_sort: [key("sort", "[]"), key("from"), key("size")],
};

const MAPPING_ROOT: Suggestion[] = [
  key("properties", "{}"),
  key("dynamic", ""),
  key("runtime", "{}"),
  key("dynamic_templates", "[]"),
  key("_source", "{}"),
  key("_routing", "{}"),
  key("_meta", "{}"),
];

const FIELD_PARAMS: Suggestion[] = [
  key("type", '""'),
  key("fields", "{}"),
  key("properties", "{}"),
  key("analyzer", '""'),
  key("search_analyzer", '""'),
  key("index"),
  key("doc_values"),
  key("store"),
  key("norms"),
  key("format", '""'),
  key("ignore_above"),
  key("null_value"),
  key("copy_to", '""'),
  key("dims"),
  key("similarity", '""'),
];

export const FIELD_TYPES = [
  "text",
  "keyword",
  "match_only_text",
  "long",
  "integer",
  "short",
  "byte",
  "double",
  "float",
  "half_float",
  "scaled_float",
  "unsigned_long",
  "date",
  "date_nanos",
  "boolean",
  "binary",
  "ip",
  "object",
  "nested",
  "flattened",
  "geo_point",
  "geo_shape",
  "point",
  "shape",
  "dense_vector",
  "sparse_vector",
  "rank_feature",
  "rank_features",
  "completion",
  "search_as_you_type",
  "wildcard",
  "constant_keyword",
  "version",
  "integer_range",
  "long_range",
  "float_range",
  "double_range",
  "date_range",
  "ip_range",
  "alias",
  "join",
  "percolator",
  "token_count",
];

const TEMPLATE_BODY: Suggestion[] = [
  key("index_patterns", "[]"),
  key("template", "{}"),
  key("composed_of", "[]"),
  key("priority"),
  key("version"),
  key("data_stream", "{}"),
  key("_meta", "{}"),
  key("allow_auto_create"),
];

const INDEX_BODY: Suggestion[] = [key("settings", "{}"), key("mappings", "{}"), key("aliases", "{}")];

const INDEX_SETTINGS: Suggestion[] = [
  key("number_of_shards"),
  key("number_of_replicas"),
  key("refresh_interval", '""'),
  key("analysis", "{}"),
  key("max_result_window"),
  key("default_pipeline", '""'),
  key("mapping", "{}"),
  key("similarity", "{}"),
  key("index", "{}"),
];

const ANALYZE_BODY: Suggestion[] = [
  key("analyzer", '""'),
  key("text", '""'),
  key("tokenizer", '""'),
  key("filter", "[]"),
  key("char_filter", "[]"),
  key("field", '""'),
  key("normalizer", '""'),
  key("explain"),
];

const ANALYZERS = ["standard", "simple", "whitespace", "keyword", "stop", "pattern", "fingerprint", "english", "german", "french", "spanish"];

const SIMPLE_BODIES: Record<string, Suggestion[]> = {
  _count: [key("query", "{}")],
  _delete_by_query: [key("query", "{}"), key("max_docs"), key("slice", "{}")],
  _update_by_query: [key("query", "{}"), key("script", "{}"), key("max_docs"), key("slice", "{}")],
  _update: [key("doc", "{}"), key("script", "{}"), key("upsert", "{}"), key("doc_as_upsert"), key("scripted_upsert"), key("detect_noop")],
  _reindex: [key("source", "{}"), key("dest", "{}"), key("script", "{}"), key("conflicts", '""'), key("max_docs")],
  _mget: [key("docs", "[]"), key("ids", "[]")],
  _analyze: ANALYZE_BODY,
  _aliases: [key("actions", "[]")],
  _explain: [key("query", "{}")],
  _validate: [key("query", "{}")],
};

const BULK_ACTIONS: Suggestion[] = [key("index", "{}"), key("create", "{}"), key("update", "{}"), key("delete", "{}")];
const BULK_META: Suggestion[] = [key("_index", '""'), key("_id", '""'), key("routing", '""'), key("if_seq_no"), key("if_primary_term"), key("pipeline", '""'), key("require_alias")];
const MSEARCH_HEADER: Suggestion[] = [key("index", '""'), key("preference", '""'), key("routing", '""'), key("search_type", '""'), key("allow_no_indices")];

/** Keys whose value is a list of field names. */
const FIELD_LISTS = new Set(["fields", "_source", "docvalue_fields", "stored_fields", "includes", "excludes", "copy_to"]);

/** Values offered inside a string, by the key it belongs to. */
const STRING_VALUES: Record<string, string[]> = {
  order: ["asc", "desc"],
  operator: ["and", "or"],
  default_operator: ["AND", "OR"],
  zero_terms_query: ["none", "all"],
  score_mode: ["avg", "max", "min", "sum", "none", "multiply", "first"],
  boost_mode: ["multiply", "replace", "sum", "avg", "max", "min"],
  calendar_interval: ["minute", "hour", "day", "week", "month", "quarter", "year"],
  conflicts: ["abort", "proceed"],
  dynamic: ["true", "false", "strict", "runtime"],
  analyzer: ANALYZERS,
  search_analyzer: ANALYZERS,
  tokenizer: ["standard", "whitespace", "keyword", "letter", "lowercase", "ngram", "edge_ngram", "pattern", "classic", "uax_url_email"],
};

// --- the JSON scanner -------------------------------------------------------------------------

export interface JsonContext {
  /** Keys from the document root to the cursor's container; `[]` marks an array element. */
  path: string[];
  /** The cursor is where a key goes (or inside a key's quotes). */
  inKey: boolean;
  /** The cursor is inside a string value. */
  inString: boolean;
  /** The key a value at the cursor belongs to (`"field": "|"`). */
  valueOf: string | null;
  /** Typed so far: a key or string's content, or a bare word. */
  prefix: string;
  /** Where the token being typed starts (its opening quote, if any). */
  tokenStart: number;
  /** The token is quoted. */
  quoted: boolean;
}

interface Frame {
  kind: "obj" | "arr";
  /** Object: the key whose value is being written, once its colon was seen. */
  key: string | null;
  /** Object: a key string just closed, waiting for its colon. */
  pending: string | null;
  expectKey: boolean;
}

/**
 * Where the cursor is in an unfinished JSON text: the keys leading to it and whether a key or a
 * value goes there. Tolerant: unbalanced or invalid text gives the best guess, never throws.
 */
export function jsonContext(text: string, cursor: number): JsonContext {
  const stack: Frame[] = [];
  let i = 0;
  let strStart = -1;
  let strIsKey = false;
  const end = Math.min(cursor, text.length);
  while (i < end) {
    const c = text[i];
    if (strStart >= 0) {
      if (c === "\\") {
        i += 2;
        continue;
      }
      if (c === '"') {
        const top = stack[stack.length - 1];
        if (strIsKey && top) top.pending = unescape(text.slice(strStart + 1, i));
        strStart = -1;
      }
      i++;
      continue;
    }
    const top = stack[stack.length - 1];
    switch (c) {
      case '"':
        strStart = i;
        strIsKey = !!top && top.kind === "obj" && top.expectKey;
        break;
      case "{":
        stack.push({ kind: "obj", key: null, pending: null, expectKey: true });
        break;
      case "[":
        stack.push({ kind: "arr", key: null, pending: null, expectKey: false });
        break;
      case "}":
      case "]":
        stack.pop();
        break;
      case ":":
        if (top?.kind === "obj") {
          top.key = top.pending;
          top.pending = null;
          top.expectKey = false;
        }
        break;
      case ",":
        if (top?.kind === "obj") {
          top.key = null;
          top.pending = null;
          top.expectKey = true;
        }
        break;
    }
    i++;
  }
  const path: string[] = [];
  for (let f = 0; f < stack.length - 1; f++) {
    const fr = stack[f];
    path.push(fr.kind === "arr" ? "[]" : (fr.key ?? ""));
  }
  const top = stack[stack.length - 1];
  const keyPos = top?.kind === "obj" && top.expectKey;
  if (strStart >= 0) {
    const prefix = unescape(text.slice(strStart + 1, end));
    if (keyPos && strIsKey) return { path, inKey: true, inString: false, valueOf: null, prefix, tokenStart: strStart, quoted: true };
    const valueOf = top?.kind === "obj" ? top.key : top?.kind === "arr" ? "[]" : null;
    if (top?.kind === "arr") path.push("[]");
    return { path, inKey: false, inString: true, valueOf, prefix, tokenStart: strStart, quoted: true };
  }
  // A bare word being typed (no quote yet).
  let s = end;
  while (s > 0 && /[A-Za-z0-9_.\-]/.test(text[s - 1])) s--;
  const prefix = text.slice(s, end);
  if (top?.kind === "arr") path.push("[]");
  else if (top?.kind === "obj" && !keyPos) path.push(top.key ?? "");
  return { path, inKey: !!keyPos, inString: false, valueOf: top?.kind === "obj" && !keyPos ? top.key : null, prefix, tokenStart: s, quoted: false };
}

function unescape(s: string) {
  return s.replace(/\\(.)/g, "$1");
}

// --- what goes where ------------------------------------------------------------------------

/** The kind of body a request takes, from its method and path. */
export type BodyKind = "search" | "index" | "mapping" | "settings" | "doc" | "bulk" | "msearch" | string | null;

export function bodyKind(method: string, path: string): BodyKind {
  const p = path.split("?")[0].replace(/\/+$/, "");
  const segs = p.split("/").filter(Boolean);
  const apis = segs.filter((s) => s.startsWith("_"));
  const lastApi = apis[apis.length - 1];
  if (!lastApi) return method === "PUT" && segs.length === 1 ? "index" : null;
  if (lastApi === "_search" || lastApi === "_async_search") return "search";
  if (lastApi === "_bulk") return "bulk";
  if (lastApi === "_msearch") return "msearch";
  if (lastApi === "_mapping" || lastApi === "_mappings") return "mapping";
  if (lastApi === "_settings") return "settings";
  if (lastApi === "_doc" || lastApi === "_create") return "doc";
  if (lastApi === "_validate" || segs.includes("_validate")) return "_validate";
  return lastApi;
}

/** The index a request is about (its first path segment unless that is an API). */
export function requestIndex(path: string): string | null {
  const first = path.split("?")[0].split("/").filter(Boolean)[0];
  if (!first || first.startsWith("_")) return null;
  try {
    return decodeURIComponent(first);
  } catch {
    return first;
  }
}

function fieldSuggestions(fields: FieldInfo[], filter?: (f: FieldInfo) => boolean): Suggestion[] {
  return fields.filter((f) => !filter || filter(f)).map((f) => ({ label: f.name, kind: "field" as const, detail: f.type, value: "{}" as const }));
}

/** Suggestions for a key at `path` in a search body (`path` relative to the body root). */
function searchKeys(path: string[], fields: FieldInfo[]): Suggestion[] {
  if (path.length === 0) return SEARCH_BODY;
  const last = path[path.length - 1];
  const parent = path[path.length - 2];
  // Aggregations: `aggs.<name>` holds a type (and sub-aggregations).
  if (parent === "aggs" || parent === "aggregations") return [...AGG_TYPES, key("aggs", "{}"), key("meta", "{}")];
  const aggType = parent && (path[path.length - 3] === "aggs" || path[path.length - 3] === "aggregations") ? last : null;
  if (aggType) {
    // `filter` under an aggregation's name is the filter aggregation: its body is a query.
    if (aggType === "filter") return QUERY_TYPES;
    return AGG_PARAMS[aggType] ?? AGG_COMMON;
  }
  if (last === "sort" || (last === "[]" && parent === "sort")) return fieldSuggestions(fields).map((f) => ({ ...f, value: "{}" as const }));
  if (parent === "sort" || (path[path.length - 3] === "sort" && parent === "[]")) {
    return [key("order", '""'), key("mode", '""'), key("missing", '""'), key("unmapped_type", '""'), key("format", '""'), key("nested", "{}")];
  }
  if (last === "highlight") return [key("fields", "{}"), key("pre_tags", "[]"), key("post_tags", "[]"), key("fragment_size"), key("number_of_fragments"), key("type", '""'), key("require_field_match")];
  if (last === "fields" && parent === "highlight") return fieldSuggestions(fields);
  if (last === "collapse") return [key("field", '""'), key("inner_hits", "{}"), key("max_concurrent_group_searches")];
  if (last === "knn") return QUERY_PARAMS.knn;
  if (last === "runtime_mappings") return [];
  // Where a query goes.
  if (QUERY_SLOTS.has(last) || (last === "[]" && parent && QUERY_LIST_SLOTS.has(parent))) {
    return QUERY_TYPES;
  }
  if (last && FIELD_KEYED.has(last)) {
    const own = QUERY_PARAMS[last] ?? [];
    // Shared parameters (`boost`) besides the field name.
    return [...fieldSuggestions(fields), ...own.filter((s) => s.label === "boost" || s.label === "_name")];
  }
  if (parent && FIELD_KEYED.has(parent)) return QUERY_PARAMS[parent] ?? [];
  if (last && QUERY_PARAMS[last]) return QUERY_PARAMS[last];
  return [];
}

function mappingKeys(path: string[]): Suggestion[] {
  if (path.length === 0) return MAPPING_ROOT;
  const last = path[path.length - 1];
  const parent = path[path.length - 2];
  // `properties.<name>` (and multi-fields `fields.<name>`) take field parameters.
  if (parent === "properties" || parent === "fields") return FIELD_PARAMS;
  if (last === "properties" || last === "fields") return [];
  if (last === "_source") return [key("enabled"), key("includes", "[]"), key("excludes", "[]")];
  return [];
}

function settingsKeys(path: string[]): Suggestion[] {
  if (path.length === 0) return [key("index", "{}"), ...INDEX_SETTINGS.filter((s) => s.label !== "index")];
  if (path[path.length - 1] === "index") return INDEX_SETTINGS.filter((s) => s.label !== "index");
  if (path[path.length - 1] === "analysis") return [key("analyzer", "{}"), key("tokenizer", "{}"), key("filter", "{}"), key("char_filter", "{}"), key("normalizer", "{}")];
  return [];
}

function keysFor(kind: BodyKind, path: string[], fields: FieldInfo[]): Suggestion[] {
  switch (kind) {
    case "search":
      return searchKeys(path, fields);
    case "index": {
      if (path.length === 0) return INDEX_BODY;
      if (path[0] === "mappings") return mappingKeys(path.slice(1));
      if (path[0] === "settings") return settingsKeys(path.slice(1));
      return [];
    }
    case "mapping":
      return mappingKeys(path);
    case "_index_template":
      if (path.length === 0) return TEMPLATE_BODY;
      if (path[0] === "template") return keysFor("index", path.slice(1), fields);
      return [];
    case "_component_template":
      if (path.length === 0) return [key("template", "{}"), key("version"), key("_meta", "{}")];
      if (path[0] === "template") return keysFor("index", path.slice(1), fields);
      return [];
    case "settings":
      return settingsKeys(path);
    case "doc":
      return path.length === 0 ? fieldSuggestions(fields, (f) => !f.name.includes(".")).map((f) => ({ ...f, value: "" as const })) : [];
    case "_update":
      if (path.length === 0) return SIMPLE_BODIES._update;
      if (path[0] === "doc" || path[0] === "upsert") return path.length === 1 ? fieldSuggestions(fields, (f) => !f.name.includes(".")).map((f) => ({ ...f, value: "" as const })) : [];
      return [];
    case "_count":
    case "_delete_by_query":
    case "_update_by_query":
    case "_explain":
    case "_validate":
      if (path.length === 0) return SIMPLE_BODIES[kind];
      return searchKeys(path, fields);
    case "_reindex":
      if (path.length === 0) return SIMPLE_BODIES._reindex;
      if (path.length === 1 && path[0] === "source") return [key("index", '""'), key("query", "{}"), key("size"), key("_source", "[]"), key("remote", "{}")];
      if (path.length === 1 && path[0] === "dest") return [key("index", '""'), key("op_type", '""'), key("pipeline", '""'), key("version_type", '""')];
      if (path[0] === "source") return searchKeys(path.slice(1), fields);
      return [];
    default:
      if (kind && SIMPLE_BODIES[kind] && path.length === 0) return SIMPLE_BODIES[kind];
      return [];
  }
}

function valuesFor(kind: BodyKind, path: string[], valueOf: string | null, fields: FieldInfo[]): Suggestion[] {
  const val = (v: string, detail?: string): Suggestion => ({ label: v, kind: "value", detail });
  if (valueOf === "type" && path.some((p) => p === "properties" || p === "fields") && kind !== "search") return FIELD_TYPES.map((t) => val(t));
  const arrayOf = valueOf === "[]" ? path[path.length - 2] : null;
  if (valueOf === "field" || valueOf === "default_field" || valueOf === "path" || (arrayOf && FIELD_LISTS.has(arrayOf))) {
    const nestedOnly = valueOf === "path";
    return fields.filter((f) => !nestedOnly || f.type === "nested").map((f) => val(f.name, f.type));
  }
  if (valueOf === "_index" || (valueOf === "index" && (kind === "msearch" || kind === "_reindex"))) return [];
  if (valueOf && STRING_VALUES[valueOf]) return STRING_VALUES[valueOf].map((v) => val(v));
  return [];
}

// --- entry point ----------------------------------------------------------------------------

function filterItems(items: Suggestion[], prefix: string): Suggestion[] {
  const p = prefix.toLowerCase();
  const seen = new Set<string>();
  const starts: Suggestion[] = [];
  const contains: Suggestion[] = [];
  for (const s of items) {
    if (seen.has(s.label)) continue;
    seen.add(s.label);
    const l = s.label.toLowerCase();
    if (l === p) continue;
    if (l.startsWith(p)) starts.push(s);
    else if (p.length >= 2 && l.includes(p)) contains.push(s);
  }
  return [...starts, ...contains].slice(0, 50);
}

/** What `_bulk`/`_msearch` line `n` (0-based, among non-empty lines) is. */
export function ndjsonRole(kind: "bulk" | "msearch", lines: string[], n: number): "action" | "doc" | "update" | "header" | "search" {
  if (kind === "msearch") return n % 2 === 0 ? "header" : "search";
  let expectAction = true;
  let update = false;
  for (let i = 0; i < n; i++) {
    if (!expectAction) {
      expectAction = true;
      continue;
    }
    // A delete has no source line; an update's is an `_update` body.
    expectAction = /^\s*\{\s*"delete"/.test(lines[i]);
    update = /^\s*\{\s*"update"/.test(lines[i]);
  }
  return expectAction ? "action" : update ? "update" : "doc";
}

/**
 * Suggestions at `cursor` in a console request. `null` when there is nothing to offer (the
 * caller hides its list).
 */
export function complete(text: string, cursor: number, src: CompletionSources): Completion | null {
  const firstNl = text.indexOf("\n");
  const firstEnd = firstNl < 0 ? text.length : firstNl;
  if (cursor <= firstEnd) return completeFirstLine(text.slice(0, firstEnd), cursor, src);
  const first = text.slice(0, firstEnd).trim();
  const m = /^([A-Za-z]+)\s+(\S+)/.exec(first);
  if (!m) return null;
  const method = m[1].toUpperCase();
  const path = m[2].startsWith("/") ? m[2] : `/${m[2]}`;
  const kind = bodyKind(method, path);
  const bodyStart = firstEnd + 1;
  let ctx: JsonContext;
  let effective: BodyKind = kind;
  let offset = bodyStart;
  if (kind === "bulk" || kind === "msearch") {
    // Each line on its own.
    const body = text.slice(bodyStart);
    const rel = cursor - bodyStart;
    const lineStart = body.lastIndexOf("\n", rel - 1) + 1;
    const before = body.slice(0, lineStart).split("\n").filter((l) => l.trim());
    const role = ndjsonRole(kind, before, before.length);
    const lineEnd = body.indexOf("\n", rel);
    const line = body.slice(lineStart, lineEnd < 0 ? body.length : lineEnd);
    offset = bodyStart + lineStart;
    ctx = jsonContext(line, rel - lineStart);
    if (role === "action") {
      const items = ctx.inKey ? (ctx.path.length === 0 ? BULK_ACTIONS : ctx.path.length === 1 ? BULK_META : []) : [];
      return finish(items, ctx, offset, text);
    }
    if (role === "header") return finish(ctx.inKey && ctx.path.length === 0 ? MSEARCH_HEADER : [], ctx, offset, text);
    effective = role === "search" ? "search" : role === "update" ? "_update" : "doc";
  } else {
    ctx = jsonContext(text.slice(bodyStart), cursor - bodyStart);
  }
  if (ctx.inKey) return finish(keysFor(effective, ctx.path, src.fields), ctx, offset, text);
  if (ctx.inString) return finish(valuesFor(effective, ctx.path, ctx.valueOf, src.fields), ctx, offset, text);
  // A bare value: booleans where they are common.
  if (ctx.valueOf && ctx.prefix && /^(t|f|tr|fa|tru|fal|fals)$/.test(ctx.prefix)) {
    return finish([{ label: "true", kind: "value" }, { label: "false", kind: "value" }], ctx, offset, text);
  }
  return null;
}

function finish(items: Suggestion[], ctx: JsonContext, offset: number, text: string): Completion | null {
  const filtered = filterItems(items, ctx.prefix);
  if (!filtered.length) return null;
  const from = offset + ctx.tokenStart;
  let to = offset + ctx.tokenStart + (ctx.quoted ? 1 : 0) + ctx.prefix.length;
  // Typed with escapes the prefix is shorter than the source: find the real end of the token.
  if (ctx.quoted) {
    while (to < text.length && text[to] !== '"' && text[to] !== "\n" && /[A-Za-z0-9_.\-]/.test(text[to])) to++;
    if (text[to] === '"') to++;
  } else {
    while (to < text.length && /[A-Za-z0-9_.\-]/.test(text[to])) to++;
  }
  return { from, to, prefix: ctx.prefix, items: filtered, mode: ctx.inKey ? "key" : ctx.quoted ? "string" : "plain" };
}

function completeFirstLine(line: string, cursor: number, src: CompletionSources): Completion | null {
  const lead = line.length - line.trimStart().length;
  const sp = line.slice(lead).search(/\s/);
  if (sp < 0 || cursor <= lead + sp) {
    const prefix = line.slice(lead, cursor);
    const items = filterItems(
      CONSOLE_METHODS.map((m) => ({ label: m, kind: "method" as const })),
      prefix,
    );
    if (!items.length || !prefix) return null;
    return { from: lead, to: lead + (sp < 0 ? line.length - lead : sp), prefix, items, mode: "plain" };
  }
  let pathStart = lead + sp;
  while (pathStart < line.length && /\s/.test(line[pathStart])) pathStart++;
  if (cursor < pathStart) return null;
  const typed = line.slice(pathStart, cursor);
  if (typed.includes("?")) {
    if (!src.params) return null;
    const method = line.slice(lead, lead + sp);
    return completeQuery(method, line, pathStart, cursor, src.params);
  }
  const segStart = typed.lastIndexOf("/") + 1;
  const prefix = typed.slice(segStart);
  const before = typed.slice(0, segStart).split("/").filter(Boolean);
  let items: Suggestion[];
  if (before.length === 0) {
    items = [...src.indices.map((i) => ({ label: i, kind: "index" as const })), ...ROOT_APIS];
    // Hidden and system indices only once asked for.
    if (!prefix.startsWith(".")) items = items.filter((s) => !s.label.startsWith("."));
  } else if (before.length === 1 && !before[0].startsWith("_")) {
    items = INDEX_APIS;
  } else if (before.length === 1 && NAMESPACE_APIS[before[0]]) {
    items = NAMESPACE_APIS[before[0]];
  } else {
    return null;
  }
  const filtered = filterItems(items, prefix);
  if (!filtered.length) return null;
  let to = cursor;
  while (to < line.length && !/[\s/?]/.test(line[to])) to++;
  return { from: pathStart + segStart, to, prefix, items: filtered, mode: "plain" };
}

/** The text after choosing `item`, and where the cursor goes. */
export function applyCompletion(text: string, c: Completion, item: Suggestion): { text: string; cursor: number } {
  const after = text.slice(c.to);
  if (c.mode === "plain") {
    const ins = item.insert ?? item.label;
    return { text: text.slice(0, c.from) + ins + after, cursor: c.from + ins.length };
  }
  if (c.mode === "string") {
    const ins = JSON.stringify(item.label);
    return { text: text.slice(0, c.from) + ins + after, cursor: c.from + ins.length };
  }
  const k = JSON.stringify(item.label);
  // Renaming a key that already has its value: replace the key only.
  if (/^\s*:/.test(after)) return { text: text.slice(0, c.from) + k + after, cursor: c.from + k.length };
  const value = item.value ?? "";
  let ins = `${k}: ${value}`;
  let cursor = c.from + k.length + 2;
  if (value === "{}" || value === "[]" || value === '""') cursor += 1;
  else if (value === "") ins = `${k}: `;
  const res = text.slice(0, c.from) + ins + after;
  return { text: res, cursor: value ? cursor : c.from + ins.length };
}

/**
 * The fields of a `GET <index>/_mapping` response, flattened (`user.name`, multi-fields
 * `title.keyword`, runtime fields), merged over every index it lists.
 */
export function mappingFields(resp: unknown): FieldInfo[] {
  const out = new Map<string, string>();
  const walk = (props: unknown, prefix: string) => {
    if (!props || typeof props !== "object") return;
    for (const [name, def] of Object.entries(props as Record<string, unknown>)) {
      if (!def || typeof def !== "object") continue;
      const d = def as { type?: string; properties?: unknown; fields?: unknown };
      const full = prefix + name;
      const type = d.type ?? (d.properties ? "object" : "object");
      if (!out.has(full)) out.set(full, type);
      if (d.properties) walk(d.properties, `${full}.`);
      if (d.fields) walk(d.fields, `${full}.`);
    }
  };
  if (resp && typeof resp === "object") {
    for (const idx of Object.values(resp as Record<string, unknown>)) {
      const m = (idx as { mappings?: { properties?: unknown; runtime?: unknown } } | null)?.mappings;
      if (!m) continue;
      walk(m.properties, "");
      walk(m.runtime, "");
    }
  }
  return [...out.entries()].map(([name, type]) => ({ name, type })).sort((a, b) => a.name.localeCompare(b.name));
}
