/**
 * Documents in the index browser: flattening `_source` into table cells, choosing columns, and
 * the requests that write a document. Pure (tests: `search-docs.test.ts`).
 *
 * Documents travel as JSON text and are never re-encoded from parsed values: `JSON.parse` rounds
 * integers above 2^53, turns `1.0` into `1` (which Elasticsearch's dynamic mapping then maps as
 * `long`, not `float`) and moves integer-like keys first. So bodies go out as typed
 * ([`parseDocument`] only validates), `_source` is cut out of the response text
 * ([`hitSources`]), and display re-indents text token by token ([`formatJson`]).
 */

/** A source flattened to dotted paths (`user.name`); arrays stay values. */
export function flatten(source: unknown, prefix = "", out: Map<string, unknown> = new Map()): Map<string, unknown> {
  if (source && typeof source === "object" && !Array.isArray(source)) {
    const entries = Object.entries(source as Record<string, unknown>);
    if (entries.length === 0 && prefix) out.set(prefix.slice(0, -1), {});
    for (const [k, v] of entries) {
      if (v && typeof v === "object" && !Array.isArray(v)) flatten(v, `${prefix}${k}.`, out);
      else out.set(prefix + k, v);
    }
  } else if (prefix) {
    out.set(prefix.slice(0, -1), source);
  }
  return out;
}

/**
 * The columns of a page of documents: the paths most documents have, in first-seen order among
 * equally common ones, at most `max`.
 */
export function tableColumns(sources: unknown[], max = 8): string[] {
  return columnsOf(
    sources.map((s) => flatten(s).keys()),
    max,
  );
}

/** [`tableColumns`] from each document's paths (e.g. the keys of [`sourceCells`]). */
export function columnsOf(rows: Iterable<string>[], max = 8): string[] {
  const count = new Map<string, number>();
  const order = new Map<string, number>();
  for (const keys of rows) {
    for (const k of keys) {
      count.set(k, (count.get(k) ?? 0) + 1);
      if (!order.has(k)) order.set(k, order.size);
    }
  }
  return [...count.keys()].sort((a, b) => count.get(b)! - count.get(a)! || order.get(a)! - order.get(b)!).slice(0, max);
}

/** One cell: strings as they are, everything else as compact JSON; `undefined` is empty. */
export function cellText(v: unknown, max = 120): string {
  if (v === undefined) return "";
  const s = typeof v === "string" ? v : JSON.stringify(v);
  return s.length > max ? `${s.slice(0, max)}…` : s;
}

/**
 * A document body typed by hand: must be one JSON object. Returns the text to send, as typed
 * (trimmed); parsing only validates it.
 */
export function parseDocument(text: string): string {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch (e) {
    throw new Error(`Not valid JSON: ${e instanceof Error ? e.message : String(e)}`);
  }
  if (!v || typeof v !== "object" || Array.isArray(v)) throw new Error("A document is a JSON object: { … }");
  return text.trim();
}

/** A JSON value's place in its text, as [`jsonSpans`] finds it. */
export interface JsonSpan {
  start: number;
  end: number;
  kind: "object" | "array" | "string" | "number" | "literal";
  /** Members of an object, in order (keys decoded). */
  entries?: [string, JsonSpan][];
  items?: JsonSpan[];
}

const isWs = (c: string | undefined) => c === " " || c === "\n" || c === "\r" || c === "\t";

/** Index just past the string that starts at `i` (a `"`). */
function stringEnd(text: string, i: number): number {
  let j = i + 1;
  while (j < text.length && text[j] !== '"') j += text[j] === "\\" ? 2 : 1;
  if (j >= text.length) throw new Error("unterminated string");
  return j + 1;
}

const SCALAR = /-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?|true|false|null/y;

/**
 * Where every value of a JSON text is, without converting any (numbers stay text). Throws on
 * malformed input.
 */
export function jsonSpans(text: string): JsonSpan {
  let i = 0;
  const skip = () => {
    while (isWs(text[i])) i++;
  };
  const expect = (c: string) => {
    skip();
    if (text[i] !== c) throw new Error(`expected ${c} at ${i}`);
    i++;
  };
  const value = (): JsonSpan => {
    skip();
    const start = i;
    const c = text[i];
    if (c === "{" || c === "[") {
      const close = c === "{" ? "}" : "]";
      i++;
      skip();
      const entries: [string, JsonSpan][] = [];
      const items: JsonSpan[] = [];
      if (text[i] === close) {
        i++;
      } else {
        for (;;) {
          if (c === "{") {
            skip();
            if (text[i] !== '"') throw new Error(`expected a key at ${i}`);
            const end = stringEnd(text, i);
            const key = JSON.parse(text.slice(i, end)) as string;
            i = end;
            expect(":");
            entries.push([key, value()]);
          } else {
            items.push(value());
          }
          skip();
          if (text[i] === ",") {
            i++;
            continue;
          }
          expect(close);
          break;
        }
      }
      return c === "{" ? { start, end: i, kind: "object", entries } : { start, end: i, kind: "array", items };
    }
    if (c === '"') {
      i = stringEnd(text, i);
      return { start, end: i, kind: "string" };
    }
    SCALAR.lastIndex = i;
    const m = SCALAR.exec(text);
    if (!m) throw new Error(`unexpected ${c === undefined ? "end" : JSON.stringify(c)} at ${i}`);
    i += m[0].length;
    return { start, end: i, kind: c === "t" || c === "f" || c === "n" ? "literal" : "number" };
  };
  const root = value();
  skip();
  if (i < text.length) throw new Error(`unexpected text at ${i}`);
  return root;
}

function member(span: JsonSpan | undefined, key: string): JsonSpan | undefined {
  return span?.entries?.find(([k]) => k === key)?.[1];
}

/**
 * The `_source` of each hit of a search response, exactly as the response holds it (`undefined`
 * for a hit without one); `[]` when the text isn't a search response.
 */
export function hitSources(responseText: string): (string | undefined)[] {
  let root: JsonSpan;
  try {
    root = jsonSpans(responseText);
  } catch {
    return [];
  }
  return (member(member(root, "hits"), "hits")?.items ?? []).map((h) => {
    const s = member(h, "_source");
    return s && responseText.slice(s.start, s.end);
  });
}

/**
 * JSON text re-indented by `indent` spaces (0: one compact line), token by token, so numbers,
 * keys and their order stay exactly as written. Text that isn't JSON comes back unchanged.
 */
export function formatJson(text: string, indent = 2): string {
  try {
    jsonSpans(text);
  } catch {
    return text;
  }
  const out: string[] = [];
  let depth = 0;
  const nl = () => (indent ? `\n${" ".repeat(depth * indent)}` : "");
  for (let i = 0; i < text.length; ) {
    const c = text[i];
    if (isWs(c)) {
      i++;
    } else if (c === '"') {
      const end = stringEnd(text, i);
      out.push(text.slice(i, end));
      i = end;
    } else if (c === "{" || c === "[") {
      let j = i + 1;
      while (isWs(text[j])) j++;
      if (text[j] === (c === "{" ? "}" : "]")) {
        out.push(c === "{" ? "{}" : "[]");
        i = j + 1;
      } else {
        depth++;
        out.push(c + nl());
        i++;
      }
    } else if (c === "}" || c === "]") {
      depth--;
      out.push(nl() + c);
      i++;
    } else if (c === ",") {
      out.push(`,${nl()}`);
      i++;
    } else if (c === ":") {
      out.push(indent ? ": " : ":");
      i++;
    } else {
      let j = i;
      while (j < text.length && !isWs(text[j]) && !",:{}[]".includes(text[j])) j++;
      out.push(text.slice(i, j));
      i = j;
    }
  }
  return out.join("");
}

/**
 * The table cells of a `_source` text: [`flatten`] + [`cellText`] without the loss. Strings are
 * decoded; every other leaf (numbers, literals, arrays) is its compact JSON text as written.
 */
export function sourceCells(source: string | undefined): Map<string, string> {
  const out = new Map<string, string>();
  if (source === undefined) return out;
  let root: JsonSpan;
  try {
    root = jsonSpans(source);
  } catch {
    return out;
  }
  const leaf = (s: JsonSpan) => {
    const t = source.slice(s.start, s.end);
    return s.kind === "string" ? (JSON.parse(t) as string) : formatJson(t, 0);
  };
  const walk = (s: JsonSpan, prefix: string) => {
    if (s.kind === "object") {
      if (s.entries!.length === 0 && prefix) out.set(prefix.slice(0, -1), "{}");
      for (const [k, v] of s.entries!) walk(v, `${prefix}${k}.`);
    } else if (prefix) {
      out.set(prefix.slice(0, -1), leaf(s));
    }
  };
  walk(root, "");
  return out;
}

/**
 * The request that writes a document: `PUT <index>/_doc/<id>` (create only, for a new one with an
 * id), or `POST <index>/_doc` (generated id). `refresh=wait_for` so the browser sees the change.
 */
export function writeRequest(index: string, id: string | null, opts: { create?: boolean; routing?: string } = {}): { method: "PUT" | "POST"; path: string } {
  const base = `/${encodeURIComponent(index)}/_doc`;
  const q = new URLSearchParams({ refresh: "wait_for" });
  if (opts.routing) q.set("routing", opts.routing);
  if (id === null || id === "") return { method: "POST", path: `${base}?${q}` };
  if (opts.create) q.set("op_type", "create");
  return { method: "PUT", path: `${base}/${encodeURIComponent(id)}?${q}` };
}

/** The request that deletes a document. */
export function deleteRequest(index: string, id: string, routing?: string): { method: "DELETE"; path: string } {
  const q = new URLSearchParams({ refresh: "wait_for" });
  if (routing) q.set("routing", routing);
  return { method: "DELETE", path: `/${encodeURIComponent(index)}/_doc/${encodeURIComponent(id)}?${q}` };
}

/** A mapped field: its full path (`user.name`, `title.keyword`) and type. */
export interface MappedField {
  name: string;
  type: string;
}

/** Field types Elasticsearch sorts on by their doc values. */
const SORTABLE = new Set([
  "keyword",
  "constant_keyword",
  "wildcard",
  "version",
  "ip",
  "boolean",
  "date",
  "date_nanos",
  "long",
  "integer",
  "short",
  "byte",
  "double",
  "float",
  "half_float",
  "scaled_float",
  "unsigned_long",
]);

/**
 * The field a table column sorts by, or `null` when Elasticsearch can't sort it: the column's own
 * field when its type has doc values, else (text) a keyword multi-field under it
 * (`title.keyword`, the dynamic mapping's). Unmapped columns and objects don't sort.
 */
export function sortField(column: string, fields: MappedField[]): string | null {
  const own = fields.find((f) => f.name === column);
  if (!own) return null;
  if (SORTABLE.has(own.type)) return column;
  if (own.type === "text" || own.type === "match_only_text") {
    const sub = fields.find((f) => f.name.startsWith(`${column}.`) && !f.name.slice(column.length + 1).includes(".") && SORTABLE.has(f.type) && f.type !== "boolean");
    return sub ? sub.name : null;
  }
  return null;
}

export type SortOrder = "asc" | "desc";

/** A table sort: the column clicked and the field it sorts by. */
export interface TableSort {
  column: string;
  field: string;
  order: SortOrder;
}

/** Clicking a sortable column header: ascending, then descending, then back to relevance. */
export function nextSort(current: TableSort | null, column: string, field: string): TableSort | null {
  if (!current || current.column !== column) return { column, field, order: "asc" };
  if (current.order === "asc") return { column, field, order: "desc" };
  return null;
}

/**
 * The `sort` clause of a table sort: missing values last both ways, as a table reads, and ties
 * in index order so pages don't overlap.
 */
export function sortClause(sort: TableSort | null): unknown[] | undefined {
  if (!sort) return undefined;
  return [{ [sort.field]: { order: sort.order, missing: "_last" } }, "_doc"];
}

/**
 * The columns a table shows: the ones picked (kept in the order picked, also when the page has no
 * value for them), else the most common paths of the page.
 */
export function visibleColumns(picked: string[] | null, available: string[], max = 8): string[] {
  if (picked && picked.length > 0) return picked;
  return available.slice(0, max);
}

/** Toggles one column of a picker, starting from what is shown now. */
export function toggleColumn(shown: string[], column: string): string[] {
  return shown.includes(column) ? shown.filter((c) => c !== column) : [...shown, column];
}

/** One row of an index's mapping as the browser lists it. */
export interface MappingRow {
  /** Full path: `user.name`, `title.keyword`. */
  name: string;
  type: string;
  /** Nesting under objects and multi-fields (indents the row). */
  depth: number;
  /** A runtime field (computed at search time, not stored). */
  runtime?: boolean;
  /** A multi-field of its parent (`title.keyword`). */
  multi?: boolean;
  /** What changes how the field is indexed or searched, in a few words each. */
  notes: string[];
}

/** The settings of a field worth a word in the mapping table. */
function fieldNotes(d: Record<string, unknown>): string[] {
  const notes: string[] = [];
  const str = (k: string) => (typeof d[k] === "string" || typeof d[k] === "number" ? String(d[k]) : null);
  const type = typeof d.type === "string" ? d.type : null;
  if (type === "semantic_text") notes.push(`inference ${str("inference_id") ?? "(default endpoint)"}`);
  if (str("search_inference_id")) notes.push(`search inference ${str("search_inference_id")}`);
  if (type === "dense_vector") {
    if (str("dims")) notes.push(`${str("dims")} dims`);
    if (str("element_type") && d.element_type !== "float") notes.push(`${str("element_type")} elements`);
    if (str("similarity")) notes.push(str("similarity")!);
    const opts = d.index_options as { type?: string } | undefined;
    if (opts?.type) notes.push(opts.type);
  }
  if (type === "knn_vector") {
    if (str("dimension")) notes.push(`${str("dimension")} dims`);
    if (str("space_type")) notes.push(str("space_type")!);
    const method = d.method as { name?: string; engine?: string } | undefined;
    if (method?.name) notes.push(method.engine ? `${method.name} (${method.engine})` : method.name);
  }
  if (str("analyzer")) notes.push(`analyzer ${str("analyzer")}`);
  if (str("search_analyzer")) notes.push(`search analyzer ${str("search_analyzer")}`);
  if (str("normalizer")) notes.push(`normalizer ${str("normalizer")}`);
  if (str("ignore_above")) notes.push(`ignore above ${str("ignore_above")}`);
  if (str("format")) notes.push(`format ${str("format")}`);
  if (str("scaling_factor")) notes.push(`scaling factor ${str("scaling_factor")}`);
  if (str("path") && type === "alias") notes.push(`→ ${str("path")}`);
  if (str("value") && type === "constant_keyword") notes.push(`= ${str("value")}`);
  if (d.copy_to) notes.push(`copied to ${([] as unknown[]).concat(d.copy_to).join(", ")}`);
  if (d.index === false) notes.push("not indexed");
  if (d.doc_values === false) notes.push("no doc values");
  if (d.store === true) notes.push("stored");
  if (d.enabled === false) notes.push("not parsed");
  if (d.dynamic !== undefined && d.dynamic !== true && d.dynamic !== "true") notes.push(`dynamic ${String(d.dynamic)}`);
  if (d.script) notes.push("script");
  return notes;
}

/**
 * An index's `GET /<index>/_mapping` as rows: every field in mapping order (an object before its
 * fields, a field before its multi-fields), then runtime fields. The first index of the response
 * gives the order; fields only other indices have (a pattern, a data stream's backing indices)
 * follow in theirs.
 */
export function mappingRows(resp: unknown): MappingRow[] {
  const rows: MappingRow[] = [];
  const seen = new Set<string>();
  const walk = (props: unknown, prefix: string, depth: number, extra: Partial<MappingRow>) => {
    if (!props || typeof props !== "object") return;
    for (const [name, def] of Object.entries(props as Record<string, unknown>)) {
      if (!def || typeof def !== "object") continue;
      const d = def as Record<string, unknown>;
      const full = prefix + name;
      if (!seen.has(full)) {
        seen.add(full);
        rows.push({ name: full, type: typeof d.type === "string" ? d.type : "object", depth, notes: fieldNotes(d), ...extra });
      }
      walk(d.properties, `${full}.`, depth + 1, {});
      walk(d.fields, `${full}.`, depth + 1, { multi: true });
    }
  };
  const indices = resp && typeof resp === "object" ? Object.values(resp as Record<string, unknown>) : [];
  const mappings = indices.map((i) => (i as { mappings?: { properties?: unknown; runtime?: unknown } } | null)?.mappings).filter((m) => !!m);
  for (const m of mappings) walk(m!.properties, "", 0, {});
  for (const m of mappings) walk(m!.runtime, "", 0, { runtime: true });
  return rows;
}

/** The index-wide settings of a mapping worth a word (`dynamic`, `_source`, `subobjects`). */
export function mappingNotes(resp: unknown): string[] {
  const first = resp && typeof resp === "object" ? Object.values(resp as Record<string, unknown>)[0] : null;
  const m = (first as { mappings?: Record<string, unknown> } | null)?.mappings;
  if (!m) return [];
  const notes: string[] = [];
  if (m.dynamic !== undefined && m.dynamic !== true && m.dynamic !== "true") notes.push(`dynamic: ${String(m.dynamic)}`);
  const source = m._source as { mode?: string; enabled?: boolean } | undefined;
  if (source?.mode) notes.push(`_source: ${source.mode}`);
  if (source?.enabled === false) notes.push("_source: disabled");
  if (m.subobjects === false || m.subobjects === "false") notes.push("subobjects: false");
  if (Array.isArray(m.dynamic_templates) && m.dynamic_templates.length) notes.push(`${m.dynamic_templates.length} dynamic template${m.dynamic_templates.length === 1 ? "" : "s"}`);
  return notes;
}

/** Field types the new-index editor offers (`dense_vector` is `knn_vector` on OpenSearch). */
export const NEW_FIELD_TYPES = ["text", "keyword", "long", "double", "boolean", "date", "ip", "geo_point", "semantic_text", "dense_vector"] as const;
export type NewFieldType = (typeof NEW_FIELD_TYPES)[number];

/** One field of the new-index editor. */
export interface NewField {
  /** A path: `user.name` maps `name` under the object `user`. */
  name: string;
  type: NewFieldType;
  /** text: the analyzer (`standard` when unset) and a `.keyword` sub-field for sorting. */
  analyzer?: string;
  keyword?: boolean;
  /** semantic_text: the endpoint (required: the default, ELSER, can't run on roll). */
  inferenceId?: string;
  /** dense_vector: dimensions and similarity (`cosine`, `dot_product`, `l2_norm`). */
  dims?: number;
  similarity?: "cosine" | "dot_product" | "l2_norm";
}

const OS_SPACE: Record<string, string> = { cosine: "cosinesimil", dot_product: "innerproduct", l2_norm: "l2" };

function fieldMapping(f: NewField, opensearch: boolean): Record<string, unknown> {
  switch (f.type) {
    case "text": {
      const m: Record<string, unknown> = { type: "text" };
      if (f.analyzer && f.analyzer !== "standard") m.analyzer = f.analyzer;
      if (f.keyword) m.fields = { keyword: { type: "keyword", ignore_above: 256 } };
      return m;
    }
    case "semantic_text":
      return f.inferenceId ? { type: "semantic_text", inference_id: f.inferenceId } : { type: "semantic_text" };
    case "dense_vector": {
      const sim = f.similarity ?? "cosine";
      if (opensearch) return { type: "knn_vector", dimension: f.dims ?? 0, space_type: OS_SPACE[sim] };
      return { type: "dense_vector", dims: f.dims ?? 0, similarity: sim };
    }
    default:
      return { type: f.type };
  }
}

/** What would make Elasticsearch refuse the fields, or `null`. Rows without a name are skipped. */
export function newIndexProblem(fields: NewField[], opensearch: boolean): string | null {
  const named = fields.filter((f) => f.name.trim());
  const names = new Set<string>();
  for (const f of named) {
    const n = f.name.trim();
    if (n.split(".").some((p) => !p)) return `"${n}": a field name can't start or end with a dot, or hold two in a row.`;
    if (names.has(n)) return `"${n}" is listed twice.`;
    names.add(n);
    if (f.type === "dense_vector") {
      const max = opensearch ? 16000 : 4096;
      if (!Number.isInteger(f.dims) || (f.dims ?? 0) < 1 || (f.dims ?? 0) > max) return `"${n}": dimensions are a whole number from 1 to ${max}.`;
    }
    if (f.type === "semantic_text" && opensearch) return `"${n}": semantic_text is Elasticsearch's; on OpenSearch, use a knn_vector field with an ingest pipeline.`;
    // Without one, Elasticsearch embeds with its ELSER endpoint, which needs machine learning
    // nodes: every write would fail.
    if (f.type === "semantic_text" && !f.inferenceId) return `"${n}": pick an embeddings endpoint (the default, ELSER, needs machine learning nodes, which roll doesn't run).`;
  }
  for (const n of names) {
    const parts = n.split(".");
    for (let i = 1; i < parts.length; i++) {
      const parent = parts.slice(0, i).join(".");
      if (names.has(parent)) return `"${parent}" is a field and also holds "${n}": make "${parent}" an object (leave it out) or rename one.`;
    }
  }
  return null;
}

/** The `PUT /<index>` body of the editor's fields (dotted names as objects). */
export function newIndexBody(fields: NewField[], dynamic: "true" | "strict", opensearch: boolean): Record<string, unknown> {
  const properties: Record<string, unknown> = {};
  for (const f of fields) {
    const name = f.name.trim();
    if (!name) continue;
    const parts = name.split(".");
    let at = properties;
    for (const p of parts.slice(0, -1)) {
      const obj = (at[p] ??= { properties: {} }) as { properties: Record<string, unknown> };
      at = obj.properties;
    }
    at[parts[parts.length - 1]] = fieldMapping(f, opensearch);
  }
  const mappings: Record<string, unknown> = {};
  if (dynamic === "strict") mappings.dynamic = "strict";
  mappings.properties = properties;
  const body: Record<string, unknown> = { mappings };
  if (opensearch && fields.some((f) => f.name.trim() && f.type === "dense_vector")) body.settings = { "index.knn": true };
  return body;
}
