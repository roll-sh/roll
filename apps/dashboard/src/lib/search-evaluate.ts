// Offline relevance evaluation (docs/search/README.md, `…/indices/{index}/evaluate`): judged
// queries read from what a person pastes or drops, configurations, and the answer's types.

export interface Rating {
  _id: string;
  rating: number;
  _index?: string;
}

export interface Judgment {
  query: string;
  ratings: Rating[];
}

/** A configuration as the route takes it: a search body with `{{query}}`, or a template. */
export interface EvalConfiguration {
  name: string;
  request?: Record<string, unknown>;
  template?: { id?: string; source?: unknown; params?: Record<string, unknown> };
  /** A search application's own template (on the index named as the application). */
  application?: string;
  variant?: { experiment: string; variant: string };
  query_param?: string;
}

export interface EvalQuery {
  query: string;
  score: number | null;
  unrated?: number;
  failure?: string;
}

export interface EvalChange {
  query: string;
  baseline: number;
  candidate: number;
  difference: number;
}

export interface Evaluation {
  metric: Record<string, unknown>;
  queries: number;
  configurations: { name: string; score: number | null; failed: number; queries: EvalQuery[] }[];
  comparison?: {
    baseline: string;
    candidate: string;
    difference: number | null;
    better: number;
    worse: number;
    unchanged: number;
    better_queries: EvalChange[];
    worse_queries: EvalChange[];
  };
}

/** The route's limits (`search_control::evaluate`). */
export const MAX_QUERIES = 500;
export const MAX_RATINGS = 200;

/**
 * Judged queries from text: JSON as the route takes it (`[{query, ratings}]` or
 * `{judgments: […]}`), or CSV with a header naming `query`, `_id` (or `doc_id`, `docid`) and
 * `rating` (or `grade`), optionally `_index`, one rated document a line (Quepid's and most
 * judgment tools' export), grouped by query in order. Throws with what's wrong.
 */
export function parseJudgments(text: string): Judgment[] {
  const t = text.trim();
  if (!t) throw new Error("Paste judged queries: JSON or CSV with query, _id and rating.");
  if (t.startsWith("[") || t.startsWith("{")) {
    let v: unknown;
    try {
      v = JSON.parse(t);
    } catch {
      throw new Error("The judgments aren't valid JSON.");
    }
    const list = Array.isArray(v) ? v : (v as { judgments?: unknown }).judgments;
    if (!Array.isArray(list)) throw new Error("JSON judgments are a list of {query, ratings}.");
    return list as Judgment[];
  }
  const lines = t.split(/\r?\n/).filter((l) => l.trim());
  const head = lines[0].split(",").map((h) => h.trim().toLowerCase());
  const col = (...names: string[]) => head.findIndex((h) => names.includes(h));
  const [q, d, r, ix] = [col("query"), col("_id", "doc_id", "docid"), col("rating", "grade"), col("_index", "index")];
  if (q < 0 || d < 0 || r < 0) throw new Error("The CSV's header names query, _id (or doc_id) and rating.");
  const out = new Map<string, Rating[]>();
  lines.slice(1).forEach((line, n) => {
    const f = line.split(",").map((x) => x.trim());
    const rating = Number(f[r]);
    if (f[q] === undefined || f[d] === undefined || !Number.isInteger(rating)) throw new Error(`Line ${n + 2} needs a query, a document id and a whole-number rating.`);
    const doc: Rating = { _id: f[d], rating };
    if (ix >= 0 && f[ix]) doc._index = f[ix];
    const list = out.get(f[q]) ?? [];
    list.push(doc);
    out.set(f[q], list);
  });
  return [...out].map(([query, ratings]) => ({ query, ratings }));
}

/** A starting configuration: a `match` on one field, with `{{query}}` where each query goes. */
export function matchConfiguration(name: string, field: string): EvalConfiguration {
  return { name, request: { query: { match: { [field]: "{{query}}" } } } };
}

/** A configuration from its editor's text (the name kept apart). Throws with what's wrong. */
export function parseConfiguration(name: string, text: string): EvalConfiguration {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    throw new Error(`${name} isn't valid JSON.`);
  }
  if (!v || typeof v !== "object" || Array.isArray(v)) throw new Error(`${name} is an object: {request} or {template}.`);
  const o = v as Record<string, unknown>;
  // A bare search body is the common paste: take it as the request.
  if (!("request" in o) && !("template" in o) && !("application" in o)) return { name, request: o };
  return { ...(o as Omit<EvalConfiguration, "name">), name };
}

/** A score to four places; "—" when a configuration scored nothing. */
export function scoreText(v: number | null | undefined): string {
  return v == null ? "—" : v.toFixed(4);
}

/** A change in score with its sign: "+0.1250", "−0.0400". */
export function changeText(v: number | null | undefined): string {
  if (v == null) return "—";
  if (v === 0) return "0.0000";
  return `${v > 0 ? "+" : "−"}${Math.abs(v).toFixed(4)}`;
}
