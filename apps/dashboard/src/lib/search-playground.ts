/**
 * The search playground (the cluster page's Playground tab): one query against two
 * configurations, A and B, side by side, the way Elastic's Playground and OpenSearch's search
 * relevance "compare results" do. Everything here is pure: request bodies per mode, what the
 * mapping offers each mode, rank movement between A and B, `_explain` trees as rows, and the
 * request as curl, Python and JavaScript.
 */

import type { FieldInfo } from "./search-console";
import { parseConsole } from "./search-console";
import type { Flavor } from "./search-api";

export type Mode = "keyword" | "semantic" | "hybrid" | "raw";
export const MODES: Mode[] = ["keyword", "semantic", "hybrid", "raw"];

/** Where a semantic search looks: a `semantic_text` field, or a `dense_vector` field embedded
 * through an inference endpoint. */
export type SemanticTarget = { kind: "semantic"; field: string } | { kind: "knn"; field: string; inferenceId: string };

/** One side's configuration. Every mode's settings are kept, so switching modes loses nothing. */
export interface Config {
  mode: Mode;
  /** Keyword: the text fields `multi_match` searches. */
  fields: string[];
  semantic: SemanticTarget | null;
  fusion: "rrf" | "linear";
  /** `linear`: the keyword retriever's weight, then the semantic one's. */
  weights: [number, number];
  /** Raw: a console request (`POST /index/_search` and a body); `{{query}}` stands for the query. */
  raw: string;
}

/** The placeholder a raw body uses for the query typed once above both sides. */
export const QUERY_TOKEN = "{{query}}";

/** Field types `multi_match` searches as full text. */
const TEXT_TYPES = new Set(["text", "match_only_text", "search_as_you_type"]);

/** What an index's mapping offers each mode. */
export interface Offer {
  text: string[];
  semanticText: string[];
  denseVector: string[];
}

export function offerOf(fields: FieldInfo[]): Offer {
  return {
    text: fields.filter((f) => TEXT_TYPES.has(f.type)).map((f) => f.name),
    semanticText: fields.filter((f) => f.type === "semantic_text").map((f) => f.name),
    denseVector: fields.filter((f) => f.type === "dense_vector").map((f) => f.name),
  };
}

/** The semantic targets an index has: every `semantic_text` field, and each `dense_vector` field
 * with each text-embedding endpoint. */
export function semanticTargets(offer: Offer, endpoints: string[]): SemanticTarget[] {
  return [
    ...offer.semanticText.map((field): SemanticTarget => ({ kind: "semantic", field })),
    ...offer.denseVector.flatMap((field) => endpoints.map((inferenceId): SemanticTarget => ({ kind: "knn", field, inferenceId }))),
  ];
}

export function sameTarget(a: SemanticTarget | null, b: SemanticTarget | null) {
  if (!a || !b) return a === b;
  return a.kind === b.kind && a.field === b.field && (a.kind === "semantic" || (b.kind === "knn" && a.inferenceId === b.inferenceId));
}

export function defaultConfig(mode: Mode, offer: Offer, targets: SemanticTarget[]): Config {
  return { mode, fields: offer.text, semantic: targets[0] ?? null, fusion: "rrf", weights: [1, 1], raw: "" };
}

/** What both sides search with: the query typed once, the page size, and the query's embedding
 * when a side searches a `dense_vector` field (fetched from the endpoint first: roll's `knn` takes
 * a `query_vector`, not `query_vector_builder.text_embedding`). */
export interface Inputs {
  query: string;
  size: number;
  /** Per inference endpoint: the query's embedding. */
  vectors?: Record<string, number[]>;
}

/** The inference endpoint a side needs the query embedded by, if any. */
export function needsEmbedding(cfg: Config): string | null {
  return (cfg.mode === "semantic" || cfg.mode === "hybrid") && cfg.semantic?.kind === "knn" ? cfg.semantic.inferenceId : null;
}

/** The request one side sends: always a `_search` on the picked index or data stream. */
export interface PlaygroundRequest {
  method: string;
  path: string;
  /** The body as sent: built requests are `JSON.stringify`'d, raw ones go out as typed. */
  body: string;
}

export type Built = { ok: true; request: PlaygroundRequest } | { ok: false; reason: string };

const searchPath = (index: string) => `/${encodeURIComponent(index)}/_search`;

/** `knn`'s candidates per shard: Kibana's default ratio, never below `k`. */
const candidates = (k: number) => Math.max(100, k * 10);

function keywordQuery(fields: string[], query: string) {
  return { multi_match: { query, fields } };
}

function semanticQuery(t: SemanticTarget, q: Inputs, k: number) {
  if (t.kind === "semantic") return { semantic: { field: t.field, query: q.query } };
  return { knn: { field: t.field, k, num_candidates: candidates(k), query_vector: q.vectors?.[t.inferenceId] ?? [] } };
}

const NO_FIELDS = "Pick at least one text field.";
const NO_SEMANTIC = "Pick a semantic field.";

/** Why a side can't be searched yet, if it can't. */
function missing(cfg: Config, q: Inputs): string | null {
  const keyword = cfg.mode === "keyword" || cfg.mode === "hybrid";
  const semantic = cfg.mode === "semantic" || cfg.mode === "hybrid";
  if (keyword && !cfg.fields.length) return NO_FIELDS;
  if (semantic && !cfg.semantic) return NO_SEMANTIC;
  const id = needsEmbedding(cfg);
  if (id && !q.vectors?.[id]) return `The query isn't embedded by ${id} yet.`;
  return null;
}

/** Highlighting for what a side searches: the text fields' passages, and a `semantic_text`
 * field's best chunks (the `semantic` highlighter, its default). */
function highlight(fields: string[], semantic: SemanticTarget | null) {
  const out: Record<string, object> = {};
  for (const f of fields) out[f] = {};
  if (semantic?.kind === "semantic") out[semantic.field] = { type: "semantic", number_of_fragments: 2, order: "score" };
  return Object.keys(out).length ? { fields: out } : undefined;
}

/** A built mode's request body. */
function body(cfg: Config, q: Inputs): Record<string, unknown> {
  const size = q.size;
  const t = cfg.semantic as SemanticTarget;
  switch (cfg.mode) {
    case "semantic": {
      // A knn search goes in the top-level `knn` section, where `k` is the number of hits.
      const search = t.kind === "semantic" ? { query: semanticQuery(t, q, size) } : semanticQuery(t, q, size);
      return { size, ...search, highlight: highlight([], t) };
    }
    case "hybrid": {
      const window = Math.max(size, 50);
      const keyword = { standard: { query: keywordQuery(cfg.fields, q.query) } };
      // A knn retriever takes the same section as a knn search, its `k` the fused window.
      const semantic = t.kind === "semantic" ? { standard: { query: semanticQuery(t, q, size) } } : semanticQuery(t, q, window);
      const retriever =
        cfg.fusion === "rrf"
          ? { rrf: { retrievers: [keyword, semantic], rank_window_size: window } }
          : {
              linear: {
                retrievers: [
                  { retriever: keyword, weight: cfg.weights[0], normalizer: "minmax" },
                  { retriever: semantic, weight: cfg.weights[1], normalizer: "minmax" },
                ],
                rank_window_size: window,
              },
            };
      return { size, retriever, highlight: highlight(cfg.fields, t) };
    }
    default:
      return { size, query: keywordQuery(cfg.fields, q.query), highlight: highlight(cfg.fields, null) };
  }
}

/** JSON-escapes the query for the inside of a JSON string. */
const escapeQuery = (query: string) => JSON.stringify(query).slice(1, -1);

/** The exact request a side sends for `query` on `index`. */
export function buildRequest(cfg: Config, index: string, q: Inputs): Built {
  if (cfg.mode === "raw") {
    const text = cfg.raw.split(QUERY_TOKEN).join(escapeQuery(q.query));
    try {
      const p = parseConsole(text);
      if (p.body === undefined) return { ok: false, reason: "Add a body below the first line." };
      return { ok: true, request: { method: p.method, path: p.path, body: p.body } };
    } catch (e) {
      return { ok: false, reason: e instanceof Error ? e.message : String(e) };
    }
  }
  const why = missing(cfg, q);
  if (why) return { ok: false, reason: why };
  return { ok: true, request: { method: "POST", path: searchPath(index), body: JSON.stringify(body(cfg, q)) } };
}

/** A side's request as a console text with `{{query}}` in place of the query: what a switch to
 * Raw starts from. */
export function rawFrom(cfg: Config, index: string, q: Inputs): string {
  const from = { ...cfg, mode: cfg.mode === "raw" ? ("keyword" as const) : cfg.mode };
  const ph = { ...q, query: QUERY_TOKEN };
  const json = missing(from, ph) ? { size: q.size, query: { query_string: { query: QUERY_TOKEN } } } : body(from, ph);
  return `POST ${searchPath(index)}\n${JSON.stringify(json, null, 2)}`;
}

// --- Explain ------------------------------------------------------------------------------------

/** The queries to explain a side's hit with: one per scoring part. A fused (hybrid) ranking has
 * no single query, so each retriever's query is explained on its own. */
export function explainQueries(cfg: Config, q: Inputs): { label: string | null; query: unknown }[] | string {
  if (cfg.mode === "raw") {
    const built = buildRequest(cfg, "", q);
    if (!built.ok) return built.reason;
    const parsed = JSON.parse(built.request.body) as { query?: unknown };
    return parsed.query ? [{ label: null, query: parsed.query }] : "Explain accounts for a body's query; this body has none.";
  }
  const why = missing(cfg, q);
  if (why) return why;
  const keyword = { label: cfg.mode === "hybrid" ? "keyword" : null, query: keywordQuery(cfg.fields, q.query) };
  const semantic = () => ({ label: cfg.mode === "hybrid" ? "semantic" : null, query: semanticQuery(cfg.semantic as SemanticTarget, q, Math.max(q.size, 10)) });
  if (cfg.mode === "keyword") return [keyword];
  if (cfg.mode === "semantic") return [semantic()];
  return [keyword, semantic()];
}

function hasKey(v: unknown, key: string): boolean {
  if (!v || typeof v !== "object") return false;
  if (Array.isArray(v)) return v.some((x) => hasKey(x, key));
  return Object.entries(v).some(([k, x]) => k === key || hasKey(x, key));
}

/**
 * The request that explains how `query` scores document `id` of `index` (a hit's concrete
 * index): `_explain`, or, for a query with a `semantic` part (which needs the search's rewrite to
 * embed the query, so `_explain` refuses it), a `_search` with `explain` limited to that id.
 */
export function explainRequest(index: string, id: string, query: unknown): PlaygroundRequest {
  const ix = encodeURIComponent(index);
  if (hasKey(query, "semantic")) {
    const body = { size: 1, explain: true, _source: false, query: { bool: { must: [query], filter: [{ ids: { values: [id] } }] } } };
    return { method: "POST", path: `/${ix}/_search`, body: JSON.stringify(body) };
  }
  return { method: "POST", path: `/${ix}/_explain/${encodeURIComponent(id)}`, body: JSON.stringify({ query }) };
}

/** The explanation in either answer [`explainRequest`] gets; null when the document doesn't match. */
export function explanationOf(answer: unknown): Explanation | null {
  const a = answer as { matched?: boolean; explanation?: Explanation; hits?: { hits?: { _explanation?: Explanation }[] } } | null;
  if (a?.explanation) return a.matched === false ? null : a.explanation;
  const e = a?.hits?.hits?.[0]?._explanation;
  if (!e) return null;
  // The id filter adds a zero "match on required clause" next to the query's own tree.
  const own = (e.details ?? []).filter((d) => !(d.description.startsWith("match on required clause") && JSON.stringify(d).includes('"_id:')));
  return own.length === 1 && own.length !== e.details?.length ? own[0] : e;
}

/** An `_explain` answer's tree. */
export interface Explanation {
  value: number;
  description: string;
  details?: Explanation[];
}

export interface ExplainRow {
  depth: number;
  value: number;
  description: string;
}

/** The tree as indented rows, depth first. */
export function explainRows(e: Explanation, depth = 0, out: ExplainRow[] = []): ExplainRow[] {
  out.push({ depth, value: e.value, description: e.description });
  for (const d of e.details ?? []) explainRows(d, depth + 1, out);
  return out;
}

/** A score as Elasticsearch prints floats, kept short. */
export function formatScore(n: number | null | undefined): string {
  if (n == null) return "—";
  if (n === 0) return "0";
  const a = Math.abs(n);
  if (a >= 1000 || a < 0.0001) return n.toExponential(3);
  return Number(n.toPrecision(4)).toString();
}

// --- Rank movement ------------------------------------------------------------------------------

/** A hit's identity across both sides: its concrete index and `_id`. */
export const hitKey = (h: { _index: string; _id: string }) => `${h._index}\u0000${h._id}`;

export type Movement = { kind: "up" | "down"; by: number } | { kind: "same" } | { kind: "new" } | { kind: "gone" };

/**
 * How each side's hits moved against the other side: B's hits against their rank in A
 * (`up`, `down`, `same`, `new` when A didn't return them), and A's hits that B didn't return
 * (`gone`). Keys are [`hitKey`]s, in rank order.
 */
export function rankMovement(a: string[], b: string[]): { a: Map<string, Movement>; b: Map<string, Movement> } {
  const rankA = new Map(a.map((k, i) => [k, i]));
  const rankB = new Map(b.map((k, i) => [k, i]));
  const inB = new Map<string, Movement>();
  b.forEach((k, i) => {
    const was = rankA.get(k);
    inB.set(k, was === undefined ? { kind: "new" } : was === i ? { kind: "same" } : was > i ? { kind: "up", by: was - i } : { kind: "down", by: i - was });
  });
  const inA = new Map<string, Movement>();
  for (const k of a) if (!rankB.has(k)) inA.set(k, { kind: "gone" });
  return { a: inA, b: inB };
}

export function movementLabel(m: Movement): string {
  switch (m.kind) {
    case "up":
      return `↑${m.by}`;
    case "down":
      return `↓${m.by}`;
    case "same":
      return "=";
    case "new":
      return "new";
    case "gone":
      return "gone";
  }
}

// --- Highlights ---------------------------------------------------------------------------------

/** A highlighted passage split into plain and `<em>` parts (rendered as text, never as HTML). */
export function emParts(passage: string): { text: string; em: boolean }[] {
  const out: { text: string; em: boolean }[] = [];
  const re = /<em>([\s\S]*?)<\/em>/g;
  let at = 0;
  for (let m = re.exec(passage); m; m = re.exec(passage)) {
    if (m.index > at) out.push({ text: passage.slice(at, m.index), em: false });
    out.push({ text: m[1], em: true });
    at = m.index + m[0].length;
  }
  if (at < passage.length) out.push({ text: passage.slice(at), em: false });
  return out;
}

// --- Copy as code -------------------------------------------------------------------------------

export type Language = "curl" | "python" | "javascript";
export const LANGUAGES: Language[] = ["curl", "python", "javascript"];

/** The environment variable copied code reads the key from: never a real key. */
export const KEY_ENV = "ELASTICSEARCH_API_KEY";

const IDENT = /^[A-Za-z_$][\w$]*$/;
const PY_KEYWORDS = new Set(["from", "and", "as", "class", "def", "global", "import", "in", "is", "lambda", "not", "or", "pass", "return", "with", "yield"]);

/** A JSON value as a Python or JavaScript literal, indented like `JSON.stringify(v, null, 4|2)`. */
export function literal(v: unknown, lang: "python" | "javascript", indent = 0): string {
  const step = lang === "python" ? 4 : 2;
  const pad = (n: number) => " ".repeat(n);
  if (v === null || v === undefined) return lang === "python" ? "None" : "null";
  if (typeof v === "boolean") return lang === "python" ? (v ? "True" : "False") : String(v);
  if (typeof v === "number" || typeof v === "string") return JSON.stringify(v);
  if (Array.isArray(v)) {
    if (!v.length) return "[]";
    // Short lists of scalars stay on one line, as a person would write them.
    if (v.every((x) => x === null || typeof x !== "object")) {
      const flat = `[${v.map((x) => literal(x, lang)).join(", ")}]`;
      if (flat.length <= 60) return flat;
    }
    return `[\n${v.map((x) => pad(indent + step) + literal(x, lang, indent + step)).join(",\n")},\n${pad(indent)}]`;
  }
  const entries = Object.entries(v as Record<string, unknown>).filter(([, x]) => x !== undefined);
  if (!entries.length) return "{}";
  const key = (k: string) => (lang === "javascript" && IDENT.test(k) ? k : JSON.stringify(k));
  return `{\n${entries.map(([k, x]) => `${pad(indent + step)}${key(k)}: ${literal(x, lang, indent + step)}`).join(",\n")},\n${pad(indent)}}`;
}

/** The Python client's keyword for a body field (`from` → `from_`, `_source` → `source`), or
 * null when it can't be one. */
function pyKeyword(k: string): string | null {
  if (k === "from") return "from_";
  if (k === "_source") return "source";
  return IDENT.test(k) && !k.includes("$") && !PY_KEYWORDS.has(k) ? k : null;
}

const shellQuote = (s: string) => `'${s.replace(/'/g, "'\\''")}'`;

/** Where the copied code connects, and how it authenticates. */
export interface Target {
  url: string;
  flavor: Flavor;
}

/**
 * The request as code. Elasticsearch-flavor clusters get an API key from `$ELASTICSEARCH_API_KEY` (as the guides export it)
 * and Elastic's clients; OpenSearch-flavor ones (whose keys are internal users) get basic auth
 * from `$OPENSEARCH_USERNAME`/`$OPENSEARCH_PASSWORD` and OpenSearch's clients, which Elastic's
 * clients refuse to talk to.
 */
export function copyAs(lang: Language, req: PlaygroundRequest, target: Target): string {
  const url = target.url.replace(/\/+$/, "");
  const os = target.flavor === "opensearch";
  const parsed = JSON.parse(req.body) as Record<string, unknown>;
  const index = indexOf(req.path);
  switch (lang) {
    case "curl": {
      const auth = os ? `-u "$OPENSEARCH_USERNAME:$OPENSEARCH_PASSWORD"` : `-H "Authorization: ApiKey $${KEY_ENV}"`;
      // A built body is indented; a raw one is sent as typed (`1.0` and long integers stay exact).
      const data = JSON.stringify(parsed) === req.body ? JSON.stringify(parsed, null, 2) : req.body;
      return `curl -X ${req.method} ${shellQuote(url + req.path)} \\\n  ${auth} \\\n  -H 'content-type: application/json' \\\n  -d ${shellQuote(data)}`;
    }
    case "python": {
      const head = os
        ? `# pip install opensearch-py\nimport os\nfrom opensearchpy import OpenSearch\n\nclient = OpenSearch(\n    hosts=[${JSON.stringify(url)}],\n    http_auth=(os.environ["OPENSEARCH_USERNAME"], os.environ["OPENSEARCH_PASSWORD"]),\n)\n`
        : `# pip install elasticsearch\nimport os\nfrom elasticsearch import Elasticsearch\n\nclient = Elasticsearch(${JSON.stringify(url)}, api_key=os.environ["${KEY_ENV}"])\n`;
      const keys = Object.keys(parsed);
      const kwargs = !os && keys.every((k) => pyKeyword(k) !== null);
      const args = kwargs
        ? keys.map((k) => `    ${pyKeyword(k)}=${literal(parsed[k], "python", 4)},`).join("\n")
        : `    body=${literal(parsed, "python", 4)},`;
      return `${head}resp = client.search(\n    index=${JSON.stringify(index)},\n${args}\n)\nfor hit in resp["hits"]["hits"]:\n    print(hit["_score"], hit["_id"])\n`;
    }
    case "javascript": {
      if (os) {
        return `// npm i @opensearch-project/opensearch\nimport { Client } from "@opensearch-project/opensearch";\n\nconst client = new Client({\n  node: ${JSON.stringify(url)},\n  auth: { username: process.env.OPENSEARCH_USERNAME, password: process.env.OPENSEARCH_PASSWORD },\n});\nconst { body } = await client.search(${literal({ index, body: parsed }, "javascript")});\nfor (const hit of body.hits.hits) console.log(hit._score, hit._id);\n`;
      }
      return `// npm i @elastic/elasticsearch\nimport { Client } from "@elastic/elasticsearch";\n\nconst client = new Client({\n  node: ${JSON.stringify(url)},\n  auth: { apiKey: process.env.${KEY_ENV} },\n});\nconst resp = await client.search(${literal({ index, ...parsed }, "javascript")});\nfor (const hit of resp.hits.hits) console.log(hit._score, hit._id);\n`;
    }
  }
}

/** The index (or indices, data stream, pattern) a `/<index>/_search` path names, decoded. */
function indexOf(path: string): string {
  const seg = path.split("?", 1)[0].split("/").filter(Boolean);
  return seg.length > 1 && !seg[0].startsWith("_") ? decodeURIComponent(seg[0]) : "_all";
}
